// Copyright Kamu Data, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::sync::Arc;

use async_graphql::Context;
use internal_error::*;
use kamu_core::{AccessError, Dataset, DatasetRepository};
use opendatafabric::DatasetHandle;
use thiserror::Error;

///////////////////////////////////////////////////////////////////////////////

// TODO: Return gql-specific error and get rid of unwraps
pub(crate) fn from_catalog<T>(ctx: &Context<'_>) -> Result<Arc<T>, dill::InjectionError>
where
    T: ?Sized + Send + Sync + 'static,
{
    let cat = ctx.data::<dill::Catalog>().unwrap();
    cat.get_one::<T>()
}

///////////////////////////////////////////////////////////////////////////////

pub(crate) async fn get_dataset(
    ctx: &Context<'_>,
    dataset_handle: &DatasetHandle,
) -> Result<Arc<dyn Dataset>, InternalError> {
    let dataset_repo = from_catalog::<dyn DatasetRepository>(ctx).unwrap();
    let dataset = dataset_repo
        .get_dataset(&dataset_handle.as_local_ref())
        .await
        .int_err()?;
    Ok(dataset)
}

///////////////////////////////////////////////////////////////////////////////

pub(crate) async fn check_dataset_write_access(
    ctx: &Context<'_>,
    dataset_handle: &DatasetHandle,
) -> Result<(), CheckDatasetAccessError> {
    let dataset_action_authorizer =
        from_catalog::<dyn kamu_core::auth::DatasetActionAuthorizer>(ctx).int_err()?;

    dataset_action_authorizer
        .check_action_allowed(dataset_handle, kamu_core::auth::DatasetAction::Write)
        .await?;

    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum CheckDatasetAccessError {
    #[error(transparent)]
    Access(AccessError),

    #[error(transparent)]
    Internal(InternalError),
}

impl From<InternalError> for CheckDatasetAccessError {
    fn from(value: InternalError) -> Self {
        Self::Internal(value)
    }
}

impl From<kamu_core::auth::DatasetActionUnauthorizedError> for CheckDatasetAccessError {
    fn from(v: kamu_core::auth::DatasetActionUnauthorizedError) -> Self {
        match v {
            kamu_core::auth::DatasetActionUnauthorizedError::Access(e) => Self::Access(e),
            kamu_core::auth::DatasetActionUnauthorizedError::Internal(e) => Self::Internal(e),
        }
    }
}

///////////////////////////////////////////////////////////////////////////////

/// This wrapper is unfortunately necessary because of poor error handling
/// strategy of async-graphql that:
///
/// - prevents ? operator from quietly wrapping any Display value in query
///   handler into an error thus putting us in danger of leaking sensitive info
///
/// - ensures that only `InternalError` can be returned via ? operator
///
/// - ensures that original error is preserved as `source` so it can be
///   inspected and logged by the tracing middleware

#[derive(Debug)]
pub enum GqlError {
    Internal(InternalError),
    Gql(async_graphql::Error),
}

impl From<InternalError> for GqlError {
    fn from(value: InternalError) -> Self {
        Self::Internal(value)
    }
}

impl From<async_graphql::Error> for GqlError {
    fn from(value: async_graphql::Error) -> Self {
        Self::Gql(value)
    }
}

impl Into<async_graphql::Error> for GqlError {
    fn into(self) -> async_graphql::Error {
        match self {
            Self::Internal(err) => async_graphql::Error::new_with_source(err),
            Self::Gql(err) => err,
        }
    }
}
