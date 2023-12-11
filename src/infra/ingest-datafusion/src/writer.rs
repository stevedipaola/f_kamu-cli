// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::dataframe::DataFrameWriteOptions;
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::prelude::*;
use internal_error::*;
use kamu_core::ingest::*;
use kamu_core::*;
use odf::{AsTypedBlock, MergeStrategyAppend};
use opendatafabric as odf;

///////////////////////////////////////////////////////////////////////////////

/// Implementation of the [DataWriter] interface using Apache DataFusion engine
pub struct DataWriterDataFusion {
    ctx: SessionContext,
    dataset: Arc<dyn Dataset>,
    merge_strategy: Arc<dyn MergeStrategy>,
    block_ref: BlockRef,

    // Mutable
    meta: DataWriterMetadataState,
}

/// Contains a projection of the metadata needed for [DataWriter] to function
#[derive(Debug, Clone)]
pub struct DataWriterMetadataState {
    pub head: odf::Multihash,
    pub schema: Option<SchemaRef>,
    pub source_event: Option<odf::MetadataEvent>,
    pub merge_strategy: odf::MergeStrategy,
    pub vocab: odf::DatasetVocabularyResolvedOwned,
    pub data_slices: Vec<odf::Multihash>,
    pub last_offset: Option<i64>,
    pub last_checkpoint: Option<odf::Multihash>,
    pub last_watermark: Option<DateTime<Utc>>,
    pub last_source_state: Option<odf::SourceState>,
}

///////////////////////////////////////////////////////////////////////////////

impl DataWriterDataFusion {
    pub fn builder(dataset: Arc<dyn Dataset>, ctx: SessionContext) -> DataWriterDataFusionBuilder {
        DataWriterDataFusionBuilder::new(dataset, ctx)
    }

    /// Use [Self::builder] to create an instance
    fn new(
        ctx: SessionContext,
        dataset: Arc<dyn Dataset>,
        merge_strategy: Arc<dyn MergeStrategy>,
        block_ref: BlockRef,
        metadata_state: DataWriterMetadataState,
    ) -> Self {
        Self {
            ctx,
            dataset,
            merge_strategy,
            block_ref,
            meta: metadata_state,
        }
    }

    pub fn last_offset(&self) -> Option<i64> {
        self.meta.last_offset
    }

    pub fn last_source_state(&self) -> Option<&odf::SourceState> {
        self.meta.last_source_state.as_ref()
    }

    pub fn vocab(&self) -> &odf::DatasetVocabularyResolvedOwned {
        &self.meta.vocab
    }

    pub fn source_event(&self) -> Option<&odf::MetadataEvent> {
        self.meta.source_event.as_ref()
    }

    fn validate_input(&self, df: &DataFrame) -> Result<(), BadInputSchemaError> {
        use datafusion::arrow::datatypes::DataType;

        for system_column in [
            &self.meta.vocab.offset_column,
            &self.meta.vocab.system_time_column,
        ] {
            if df.schema().has_column_with_unqualified_name(system_column) {
                return Err(BadInputSchemaError::new(
                    format!(
                        "Data contains a column that conflicts with the system column name, you \
                         should either rename the data column or configure the dataset vocabulary \
                         to use a different name: {}",
                        system_column
                    ),
                    SchemaRef::new(df.schema().into()),
                ));
            }
        }

        // Event time: If present must be a TIMESTAMP or DATE
        let event_time_col = df
            .schema()
            .fields()
            .iter()
            .find(|f| f.name().as_str() == self.meta.vocab.event_time_column);

        if let Some(event_time_col) = event_time_col {
            match event_time_col.data_type() {
                DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => {}
                typ => {
                    return Err(BadInputSchemaError::new(
                        format!(
                            "Event time column '{}' should be either Date or Timestamp, but \
                             found: {}",
                            self.meta.vocab.event_time_column, typ
                        ),
                        SchemaRef::new(df.schema().into()),
                    ));
                }
            }
        }

        Ok(())
    }

    // TODO: This function currently ensures that all timestamps in the ouput are
    // represeted as `Timestamp(Millis, "UTC")` for compatibility with other engines
    // (e.g. Flink does not support event time with nanosecond precision).
    fn normalize_raw_result(&self, df: DataFrame) -> Result<DataFrame, InternalError> {
        use datafusion::arrow::datatypes::{DataType, TimeUnit};

        let utc_tz: Arc<str> = Arc::from("UTC");
        let mut select: Vec<Expr> = Vec::new();
        let mut noop = true;

        for field in df.schema().fields() {
            let expr = match field.data_type() {
                DataType::Timestamp(TimeUnit::Millisecond, Some(tz)) if tz.as_ref() == "UTC" => {
                    col(field.unqualified_column())
                }
                DataType::Timestamp(_, _) => {
                    noop = false;
                    cast(
                        col(field.unqualified_column()),
                        DataType::Timestamp(TimeUnit::Millisecond, Some(utc_tz.clone())),
                    )
                    .alias(field.name())
                }
                _ => col(field.unqualified_column()),
            };
            select.push(expr);
        }

        if noop {
            Ok(df)
        } else {
            let df = df.select(select).int_err()?;
            tracing::info!(schema = ?df.schema(), "Schema after timestamp normalization");
            Ok(df)
        }
    }

    // TODO: PERF: This will not scale well as number of blocks grows
    async fn get_all_previous_data(
        &self,
        prev_data_slices: &Vec<odf::Multihash>,
    ) -> Result<Option<DataFrame>, InternalError> {
        if prev_data_slices.is_empty() {
            return Ok(None);
        }

        let data_repo = self.dataset.as_data_repo();

        use futures::StreamExt;
        let prev_data_paths: Vec<_> = futures::stream::iter(prev_data_slices.iter().rev())
            .then(|hash| data_repo.get_internal_url(hash))
            .map(|url| url.to_string())
            .collect()
            .await;

        let df = self
            .ctx
            .read_parquet(
                prev_data_paths,
                ParquetReadOptions {
                    // TODO: Specify schema
                    schema: None,
                    file_extension: "",
                    // TODO: PERF: Possibly speed up by specifying `offset`
                    file_sort_order: Vec::new(),
                    table_partition_cols: Vec::new(),
                    parquet_pruning: None,
                    skip_metadata: None,
                    insert_mode: datafusion::datasource::listing::ListingTableInsertMode::Error,
                },
            )
            .await
            .int_err()?;

        Ok(Some(df))
    }

    async fn with_system_columns(
        &self,
        df: DataFrame,
        system_time: DateTime<Utc>,
        fallback_event_time: DateTime<Utc>,
        start_offset: i64,
    ) -> Result<DataFrame, InternalError> {
        use datafusion::arrow::datatypes::DataType;
        use datafusion::logical_expr as expr;
        use datafusion::logical_expr::expr::WindowFunction;
        use datafusion::scalar::ScalarValue;

        // Collect non-system column names for later
        let mut raw_columns_wo_event_time: Vec<_> = df
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .filter(|n| n.as_str() != self.meta.vocab.event_time_column)
            .collect();

        // System time
        let df = df
            .with_column(
                &self.meta.vocab.system_time_column,
                Expr::Literal(ScalarValue::TimestampMillisecond(
                    Some(system_time.timestamp_millis()),
                    Some("UTC".into()),
                )),
            )
            .int_err()?;

        // Event time: Add from source event time if missing in data
        let df = if df
            .schema()
            .has_column_with_unqualified_name(&self.meta.vocab.event_time_column)
        {
            df
        } else {
            df.with_column(
                &self.meta.vocab.event_time_column,
                Expr::Literal(ScalarValue::TimestampMillisecond(
                    Some(fallback_event_time.timestamp_millis()),
                    Some("UTC".into()),
                )),
            )
            .int_err()?
        };

        // Offset
        // Note: ODF expects events within one chunk to be sorted by event time, so we
        // ensure data is held in one partition to avoid reordering when saving to
        // parquet.
        // TODO: For some reason this adds two collumns: the expected
        // "offset", but also "ROW_NUMBER()" for now we simply filter out the
        // latter.
        let df = df
            .repartition(Partitioning::RoundRobinBatch(1))
            .int_err()?
            .with_column(
                &self.meta.vocab.offset_column,
                Expr::WindowFunction(WindowFunction {
                    fun: expr::WindowFunction::BuiltInWindowFunction(
                        expr::BuiltInWindowFunction::RowNumber,
                    ),
                    args: vec![],
                    partition_by: vec![],
                    order_by: vec![
                        col(&self.meta.vocab.event_time_column as &str).sort(true, false)
                    ],
                    window_frame: expr::WindowFrame::new(false),
                }),
            )
            .int_err()?;

        let df = df
            .with_column(
                &self.meta.vocab.offset_column,
                cast(
                    col(&self.meta.vocab.offset_column as &str) + lit(start_offset - 1),
                    DataType::Int64,
                ),
            )
            .int_err()?;

        // Reorder columns for nice looks
        let mut full_columns = vec![
            self.meta.vocab.offset_column.to_string(),
            self.meta.vocab.system_time_column.to_string(),
            self.meta.vocab.event_time_column.to_string(),
        ];
        full_columns.append(&mut raw_columns_wo_event_time);
        let full_columns_str: Vec<_> = full_columns.iter().map(String::as_str).collect();

        let df = df.select_columns(&full_columns_str).int_err()?;
        Ok(df)
    }

    fn validate_output_schema(&self, new_schema: &SchemaRef) -> Result<(), BadInputSchemaError> {
        if let Some(prev_schema) = self.meta.schema.as_ref().map(|s| s.as_ref()) {
            if *prev_schema != *new_schema.as_ref() {
                return Err(BadInputSchemaError::new(
                    "Schema of the new slice differs from the schema defined by SetDataSchema \
                     event",
                    new_schema.clone(),
                ));
            }
        }
        Ok(())
    }

    // TODO: Externalize configuration
    fn get_write_properties(&self) -> WriterProperties {
        // TODO: `offset` column is sorted integers so we could use delta encoding, but
        // Flink does not support it.
        // See: https://github.com/kamu-data/kamu-engine-flink/issues/3
        WriterProperties::builder()
            .set_writer_version(datafusion::parquet::file::properties::WriterVersion::PARQUET_1_0)
            .set_compression(datafusion::parquet::basic::Compression::SNAPPY)
            // system_time value will be the same for all rows in a batch
            .set_column_dictionary_enabled(self.meta.vocab.system_time_column.as_ref().into(), true)
            .build()
    }

    #[tracing::instrument(level = "debug", skip_all, fields(?path))]
    async fn write_output(
        &self,
        path: PathBuf,
        df: DataFrame,
    ) -> Result<Option<OwnedFile>, InternalError> {
        use datafusion::arrow::array::UInt64Array;

        let res = df
            .write_parquet(
                path.as_os_str().to_str().unwrap(),
                DataFrameWriteOptions::new().with_single_file_output(true),
                Some(self.get_write_properties()),
            )
            .await
            .int_err()?;

        let file = OwnedFile::new(path);

        assert_eq!(res.len(), 1);
        assert_eq!(res[0].num_columns(), 1);
        assert_eq!(res[0].num_rows(), 1);
        let num_records = res[0]
            .column(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(0);

        if num_records > 0 {
            tracing::info!(
                path = ?file.as_path(),
                num_records,
                "Produced parquet file",
            );
            Ok(Some(file))
        } else {
            tracing::info!("Produced empty result",);
            Ok(None) // Empty file will be cleaned up here
        }
    }

    // Read output file back (metadata-only query) to get offsets and watermark
    async fn compute_offset_and_watermark(
        &self,
        path: &Path,
        prev_watermark: Option<DateTime<Utc>>,
    ) -> Result<(odf::OffsetInterval, Option<DateTime<Utc>>), InternalError> {
        use datafusion::arrow::array::{
            Date32Array,
            Date64Array,
            Int64Array,
            TimestampMillisecondArray,
        };

        let df = self
            .ctx
            .read_parquet(
                path.to_str().unwrap(),
                ParquetReadOptions {
                    schema: None,
                    file_sort_order: Vec::new(),
                    file_extension: path.extension().unwrap_or_default().to_str().unwrap(),
                    table_partition_cols: Vec::new(),
                    parquet_pruning: None,
                    skip_metadata: None,
                    insert_mode: datafusion::datasource::listing::ListingTableInsertMode::Error,
                },
            )
            .await
            .int_err()?;

        // Data must not be empty
        assert_ne!(df.clone().count().await.int_err()?, 0);

        // Calculate stats
        let stats = df
            .aggregate(
                vec![],
                vec![
                    min(col(self.meta.vocab.offset_column.as_ref())),
                    max(col(self.meta.vocab.offset_column.as_ref())),
                    // TODO: Add support for more watermark strategies
                    max(col(self.meta.vocab.event_time_column.as_ref())),
                ],
            )
            .int_err()?;

        let batches = stats.collect().await.int_err()?;
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);

        let offset_min = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0);

        let offset_max = batches[0]
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(0);

        let offset_interval = odf::OffsetInterval {
            start: offset_min,
            end: offset_max,
        };

        // Event time is either Date or Timestamp(Millisecond, UTC)
        let event_time_arr = batches[0].column(2).as_any();
        let event_time_max = if let Some(event_time_arr) =
            event_time_arr.downcast_ref::<TimestampMillisecondArray>()
        {
            let event_time_max_millis = event_time_arr.value(0);
            Utc.timestamp_millis_opt(event_time_max_millis).unwrap()
        } else if let Some(event_time_arr) = event_time_arr.downcast_ref::<Date64Array>() {
            let naive_datetime = event_time_arr.value_as_datetime(0).unwrap();
            DateTime::from_naive_utc_and_offset(naive_datetime, Utc)
        } else if let Some(event_time_arr) = event_time_arr.downcast_ref::<Date32Array>() {
            let naive_datetime = event_time_arr.value_as_datetime(0).unwrap();
            DateTime::from_naive_utc_and_offset(naive_datetime, Utc)
        } else {
            return Err(format!(
                "Expected event time column to be Date64 or Timestamp(Millisecond, UTC), but got \
                 {}",
                batches[0].schema().field(2)
            )
            .int_err()
            .into());
        };

        // Ensure watermark is monotonically non-decreasing
        let output_watermark = match prev_watermark {
            None => Some(event_time_max),
            Some(prev) if prev < event_time_max => Some(event_time_max),
            prev => prev,
        };

        Ok((offset_interval, output_watermark))
    }
}

///////////////////////////////////////////////////////////////////////////////

#[async_trait::async_trait]
impl DataWriter for DataWriterDataFusion {
    #[tracing::instrument(level = "info", skip_all)]
    async fn write(
        &mut self,
        new_data: Option<DataFrame>,
        opts: WriteDataOpts,
    ) -> Result<WriteDataResult, WriteDataError> {
        let staged = self.stage(new_data, opts).await?;
        let commit = self.commit(staged).await?;
        Ok(commit)
    }

    #[tracing::instrument(level = "info", skip_all)]
    async fn stage(
        &self,
        new_data: Option<DataFrame>,
        opts: WriteDataOpts,
    ) -> Result<StageDataResult, StageDataError> {
        let (add_data, output_schema, data_file) = if let Some(new_data) = new_data {
            self.validate_input(&new_data)?;

            // Normalize timestamps
            let df = self.normalize_raw_result(new_data)?;

            // Merge step
            // TODO: PERF: We could likely benefit from checkpointing here
            let prev = self.get_all_previous_data(&self.meta.data_slices).await?;

            let df = self.merge_strategy.merge(prev, df)?;

            tracing::debug!(
                schema = ?df.schema(),
                logical_plan = ?df.logical_plan(),
                "Performing merge step",
            );

            // Add system columns
            let df = self
                .with_system_columns(
                    df,
                    opts.system_time,
                    opts.source_event_time,
                    self.meta.last_offset.map(|e| e + 1).unwrap_or(0),
                )
                .await?;

            tracing::info!(schema = ?df.schema(), "Final output schema");

            // Validate schema matches the declared one
            let output_schema = SchemaRef::new(df.schema().into());
            self.validate_output_schema(&output_schema)?;

            // Write output
            let data_file = self.write_output(opts.data_staging_path, df).await?;

            // Prepare commit info
            let input_checkpoint = self.meta.last_checkpoint.clone();
            let source_state = opts.source_state.clone();
            let prev_watermark = self.meta.last_watermark.clone();

            if data_file.is_none() {
                // Empty result - carry watermark and propagate source state
                (
                    AddDataParams {
                        input_checkpoint,
                        output_data: None,
                        output_watermark: prev_watermark,
                        source_state,
                    },
                    Some(output_schema),
                    None,
                )
            } else {
                let (offset_interval, output_watermark) = self
                    .compute_offset_and_watermark(
                        data_file.as_ref().unwrap().as_path(),
                        prev_watermark,
                    )
                    .await?;

                (
                    AddDataParams {
                        input_checkpoint,
                        output_data: Some(offset_interval),
                        output_watermark,
                        source_state,
                    },
                    Some(output_schema),
                    data_file,
                )
            }
        } else {
            // TODO: Should watermark be advanced by the source event time?
            let add_data = AddDataParams {
                input_checkpoint: self.meta.last_checkpoint.clone(),
                output_data: None,
                output_watermark: self.meta.last_watermark.clone(),
                source_state: opts.source_state.clone(),
            };

            (add_data, None, None)
        };

        // Do we have anything to commit?
        if add_data.output_data.is_none()
            && add_data.output_watermark == self.meta.last_watermark
            && opts.source_state == self.meta.last_source_state
        {
            Err(EmptyCommitError {}.into())
        } else {
            Ok(StageDataResult {
                system_time: opts.system_time,
                add_data,
                output_schema,
                data_file,
            })
        }
    }

    #[tracing::instrument(level = "info", skip_all)]
    async fn commit(&mut self, staged: StageDataResult) -> Result<WriteDataResult, CommitError> {
        let old_head = self.meta.head.clone();

        // Commit schema if it was not previously defined
        if self.meta.schema.is_none() {
            if let Some(output_schema) = staged.output_schema {
                // TODO: Make commit of schema and data atomic
                let commit_schema_result = self
                    .dataset
                    .commit_event(
                        odf::SetDataSchema::new(&output_schema).into(),
                        CommitOpts {
                            block_ref: &self.block_ref,
                            system_time: Some(staged.system_time),
                            prev_block_hash: Some(Some(&self.meta.head)),
                            check_object_refs: false,
                        },
                    )
                    .await?;

                // Update state
                self.meta.head = commit_schema_result.new_head;
                self.meta.schema = Some(output_schema);
            }
        }

        let commit_data_result = self
            .dataset
            .commit_add_data(
                staged.add_data,
                staged.data_file,
                None,
                CommitOpts {
                    block_ref: &self.block_ref,
                    system_time: Some(staged.system_time),
                    prev_block_hash: Some(Some(&self.meta.head)),
                    check_object_refs: false,
                },
            )
            .await?;

        // Update state for the next append
        let new_block = self
            .dataset
            .as_metadata_chain()
            .get_block(&commit_data_result.new_head)
            .await
            .int_err()?
            .into_typed::<odf::AddData>()
            .unwrap();

        self.meta.head = commit_data_result.new_head.clone();

        if let Some(output_data) = &new_block.event.output_data {
            self.meta.last_offset = Some(output_data.interval.end);
            self.meta
                .data_slices
                .push(output_data.physical_hash.clone());
        }

        self.meta.last_checkpoint = new_block
            .event
            .output_checkpoint
            .as_ref()
            .map(|c| c.physical_hash.clone());

        self.meta.last_watermark = new_block.event.output_watermark;
        self.meta.last_source_state = new_block.event.source_state.clone();

        Ok(WriteDataResult {
            old_head,
            new_head: commit_data_result.new_head,
            new_block,
        })
    }
}

///////////////////////////////////////////////////////////////////////////////
// Builder
///////////////////////////////////////////////////////////////////////////////

pub struct DataWriterDataFusionBuilder {
    dataset: Arc<dyn Dataset>,
    ctx: SessionContext,
    block_ref: BlockRef,
    metadata_state: Option<DataWriterMetadataState>,
}

impl DataWriterDataFusionBuilder {
    pub fn new(dataset: Arc<dyn Dataset>, ctx: SessionContext) -> Self {
        Self {
            dataset,
            ctx,
            block_ref: BlockRef::Head,
            metadata_state: None,
        }
    }

    pub fn with_block_ref(self, block_ref: BlockRef) -> Self {
        Self { block_ref, ..self }
    }

    pub fn metadata_state(&self) -> Option<&DataWriterMetadataState> {
        self.metadata_state.as_ref()
    }

    /// Use to specify all needed state for builder to avoid scanning the
    /// metadatachain
    pub fn with_metadata_state(self, metadata_state: DataWriterMetadataState) -> Self {
        Self {
            metadata_state: Some(metadata_state),
            ..self
        }
    }

    /// Scans metadata chain to populate the needed metadata
    ///
    /// * `source_name` - name of the push source to use when extracting the
    ///   metadata needed for writing.
    pub async fn with_metadata_state_scanned(
        self,
        source_name: Option<&str>,
    ) -> Result<Self, ScanMetadataError> {
        // TODO: PERF: Full metadata scan below - this is expensive and should be
        // improved using skip lists and caching.

        let head = self
            .dataset
            .as_metadata_chain()
            .get_ref(&self.block_ref)
            .await
            .int_err()?;

        let mut schema = None;
        let mut source_event: Option<odf::MetadataEvent> = None;
        let mut data_slices = Vec::new();
        let mut last_checkpoint = None;
        let mut last_watermark = None;
        let mut last_source_state = None;
        let mut vocab: Option<odf::DatasetVocabulary> = None;
        let mut last_offset = None;

        {
            use futures::stream::TryStreamExt;
            let mut block_stream = self
                .dataset
                .as_metadata_chain()
                .iter_blocks_interval(&head, None, false);

            while let Some((_, block)) = block_stream.try_next().await.int_err()? {
                match block.event {
                    odf::MetadataEvent::SetDataSchema(set_data_schema) => {
                        if schema.is_none() {
                            schema = Some(set_data_schema.schema_as_arrow().int_err()?);
                        }
                    }
                    odf::MetadataEvent::AddData(e) => {
                        if let Some(output_data) = &e.output_data {
                            data_slices.push(output_data.physical_hash.clone());

                            if last_offset.is_none() {
                                last_offset = Some(output_data.interval.end);
                            }
                        }
                        if last_checkpoint.is_none() {
                            last_checkpoint = Some(e.output_checkpoint.map(|cp| cp.physical_hash));
                        }
                        if last_watermark.is_none() {
                            last_watermark = Some(e.output_watermark);
                        }
                        // TODO: Consider multiple sources situation
                        if last_source_state.is_none() {
                            last_source_state = Some(e.source_state);
                        }
                    }
                    odf::MetadataEvent::SetWatermark(e) => {
                        if last_watermark.is_none() {
                            last_watermark = Some(Some(e.output_watermark));
                        }
                    }
                    odf::MetadataEvent::SetPollingSource(e) => {
                        if source_name.is_some() {
                            return Err(SourceNotFoundError::new(
                                source_name,
                                "Expected a named push source, but found polling source",
                            )
                            .into());
                        }
                        if source_event.is_none() {
                            source_event = Some(e.into());
                        }
                    }
                    odf::MetadataEvent::DisablePollingSource(_) => {
                        unimplemented!("Disabling sources is not yet fully supported")
                    }
                    odf::MetadataEvent::AddPushSource(e) => {
                        if source_event.is_none() {
                            if source_name == e.source_name.as_deref() {
                                source_event = Some(e.into());
                            }
                        }
                    }
                    odf::MetadataEvent::DisablePushSource(_) => {
                        unimplemented!("Disabling sources is not yet fully supported")
                    }
                    odf::MetadataEvent::SetVocab(e) => {
                        vocab = Some(e.into());
                    }
                    odf::MetadataEvent::Seed(e) => {
                        assert_eq!(e.dataset_kind, odf::DatasetKind::Root);
                    }
                    odf::MetadataEvent::ExecuteQuery(_) => unreachable!(),
                    odf::MetadataEvent::SetAttachments(_)
                    | odf::MetadataEvent::SetInfo(_)
                    | odf::MetadataEvent::SetLicense(_)
                    | odf::MetadataEvent::SetTransform(_) => (),
                }
            }
        }

        let merge_strategy = match (&source_event, source_name) {
            // Source found
            (Some(e), _) => match e {
                odf::MetadataEvent::SetPollingSource(e) => Ok(e.merge.clone()),
                odf::MetadataEvent::AddPushSource(e) => Ok(e.merge.clone()),
                _ => unreachable!(),
            },
            // No source defined - assuming append strategy
            (None, None) => Ok(odf::MergeStrategy::Append(MergeStrategyAppend {})),
            // Source expected but not found
            (None, Some(source)) => Err(SourceNotFoundError::new(
                Some(source),
                format!("Source '{}' not found", source),
            )),
        }?;

        Ok(self.with_metadata_state(DataWriterMetadataState {
            head,
            schema,
            source_event,
            merge_strategy,
            vocab: vocab.unwrap_or_default().into(),
            data_slices,
            last_offset,
            last_checkpoint: last_checkpoint.unwrap_or_default(),
            last_watermark: last_watermark.unwrap_or_default(),
            last_source_state: last_source_state.unwrap_or_default(),
        }))
    }

    pub fn build(self) -> DataWriterDataFusion {
        let Some(metadata_state) = self.metadata_state else {
            // TODO: Typestate
            panic!(
                "Writer state is undefined - use with_metadata_state_scanned() to initialize it \
                 from metadata chain or pass it explicitly via with_metadata_state()"
            )
        };

        let merge_strategy =
            Self::merge_strategy_for(metadata_state.merge_strategy.clone(), &metadata_state.vocab);

        DataWriterDataFusion::new(
            self.ctx,
            self.dataset,
            merge_strategy,
            self.block_ref,
            metadata_state,
        )
    }

    fn merge_strategy_for(
        conf: odf::MergeStrategy,
        vocab: &odf::DatasetVocabularyResolved<'_>,
    ) -> Arc<dyn MergeStrategy> {
        use crate::merge_strategies::*;

        match conf {
            odf::MergeStrategy::Append(_cfg) => Arc::new(MergeStrategyAppend),
            odf::MergeStrategy::Ledger(cfg) => {
                Arc::new(MergeStrategyLedger::new(cfg.primary_key.clone()))
            }
            odf::MergeStrategy::Snapshot(cfg) => Arc::new(MergeStrategySnapshot::new(
                vocab.offset_column.to_string(),
                cfg.clone(),
            )),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Debug, thiserror::Error)]
pub enum ScanMetadataError {
    #[error(transparent)]
    SourceNotFound(
        #[from]
        #[backtrace]
        SourceNotFoundError,
    ),
    #[error(transparent)]
    Internal(
        #[from]
        #[backtrace]
        InternalError,
    ),
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SourceNotFoundError {
    pub source_name: Option<String>,
    message: String,
}

impl SourceNotFoundError {
    pub fn new(source_name: Option<impl Into<String>>, message: impl Into<String>) -> Self {
        Self {
            source_name: source_name.map(|v| v.into()),
            message: message.into(),
        }
    }
}

impl Into<PushSourceNotFoundError> for SourceNotFoundError {
    fn into(self) -> PushSourceNotFoundError {
        PushSourceNotFoundError::new(self.source_name)
    }
}
