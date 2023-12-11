// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::backtrace::Backtrace;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::prelude::*;
use internal_error::*;
use opendatafabric as odf;

use super::MergeError;
use crate::{AddDataParams, CommitError, OwnedFile};

///////////////////////////////////////////////////////////////////////////////

/// Auxiliary interface for appending data to root datasets.
/// Writers perform necessary transformations and merge strategies
/// to commit data into a dataset in bitemporal ledger form.
#[async_trait::async_trait]
pub trait DataWriter {
    // TODO: Avoid using Option<> and create empty DataFrame instead.
    // This would require us always knowing what the schema of data is (e.g. before
    // the first ingest run).
    async fn write(
        &mut self,
        new_data: Option<DataFrame>,
        opts: WriteDataOpts,
    ) -> Result<WriteDataResult, WriteDataError>;

    /// Prepares all data for commit without actually committing
    async fn stage(
        &self,
        new_data: Option<DataFrame>,
        opts: WriteDataOpts,
    ) -> Result<StageDataResult, StageDataError>;

    /// Commit previously staged data and advance writer state
    async fn commit(&mut self, staged: StageDataResult) -> Result<WriteDataResult, CommitError>;
}

///////////////////////////////////////////////////////////////////////////////

pub struct WriteDataOpts {
    /// Will be used for system time data column and metadata block timestamp
    pub system_time: DateTime<Utc>,
    /// If data does not contain event time column already this value will be
    /// used to populate it
    pub source_event_time: DateTime<Utc>,
    /// Data source state to store in the commit
    pub source_state: Option<odf::SourceState>,
    // TODO: Find a better way to deal with temporary files
    /// Local FS path to which data slice will be written before commiting it
    /// into the data object store of a dataset
    pub data_staging_path: PathBuf,
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteDataResult {
    pub old_head: odf::Multihash,
    pub new_head: odf::Multihash,
    pub new_block: odf::MetadataBlockTyped<odf::AddData>,
}

///////////////////////////////////////////////////////////////////////////////

/// Do not create directly, only use with [DataWriter::stage].
#[derive(Debug)]
pub struct StageDataResult {
    pub system_time: DateTime<Utc>,
    pub add_data: AddDataParams,
    pub output_schema: Option<SchemaRef>,
    pub data_file: Option<OwnedFile>,
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Debug, thiserror::Error)]
pub enum WriteDataError {
    #[error(transparent)]
    BadInputSchema(#[from] BadInputSchemaError),

    #[error(transparent)]
    MergeError(#[from] MergeError),

    #[error(transparent)]
    EmptyCommit(#[from] EmptyCommitError),

    #[error(transparent)]
    CommitError(#[from] CommitError),

    #[error(transparent)]
    Internal(#[from] InternalError),
}

impl From<StageDataError> for WriteDataError {
    fn from(value: StageDataError) -> Self {
        match value {
            StageDataError::BadInputSchema(v) => WriteDataError::BadInputSchema(v),
            StageDataError::MergeError(v) => WriteDataError::MergeError(v),
            StageDataError::EmptyCommit(v) => WriteDataError::EmptyCommit(v),
            StageDataError::Internal(v) => WriteDataError::Internal(v),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Debug, thiserror::Error)]
pub enum StageDataError {
    #[error(transparent)]
    BadInputSchema(#[from] BadInputSchemaError),

    #[error(transparent)]
    MergeError(#[from] MergeError),

    #[error(transparent)]
    EmptyCommit(#[from] EmptyCommitError),

    #[error(transparent)]
    Internal(#[from] InternalError),
}

///////////////////////////////////////////////////////////////////////////////

#[derive(Debug, thiserror::Error)]
pub struct BadInputSchemaError {
    schema: SchemaRef,
    message: String,
    backtrace: Backtrace,
}

impl BadInputSchemaError {
    pub fn new(message: impl Into<String>, schema: SchemaRef) -> Self {
        Self {
            schema,
            message: message.into(),
            backtrace: Backtrace::capture(),
        }
    }
}

impl std::fmt::Display for BadInputSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Bad input schema: {}", self.message)?;
        let parquet_schema =
            datafusion::parquet::arrow::arrow_to_parquet_schema(&self.schema).unwrap();

        let mut buf = Vec::new();
        datafusion::parquet::schema::printer::print_schema(&mut buf, parquet_schema.root_schema());
        let schema = String::from_utf8(buf).unwrap();

        writeln!(f, "{}", schema)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Nothing to commit")]
pub struct EmptyCommitError {}
