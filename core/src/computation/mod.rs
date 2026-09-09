// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Optional resources and query execution for the parallel computation graph.
//! Neither legacy query methods nor legacy plugin traits acquire these contracts.
//!
//! [`ComputationQuery`] constructs its own evaluator from [`ComputationIndexes`].
//! Atomic output requires every writer to participate in the actual index session.
//! Interrupted evaluation is fenced rather than automatically retried; call
//! [`ComputationQuery::shutdown`] after cancelling/awaiting the operation to join
//! registered provider work before recovering a replacement. Dropping an object
//! cannot substitute for its provider's asynchronous cleanup.

mod indexes;
mod transaction;

pub use crate::query::computation_exports::ComputationQuery;
pub use indexes::{
    ComputationIndexProvider, ComputationIndexes, ComputationResource, ComputationResourceCleanup,
    InMemoryComputationProvider,
};
pub use transaction::{AtomicResultTransaction, TransactionDomain};

use crate::{
    evaluation::EvaluationError,
    interface::{FutureElementRef, IndexError, QueryBuilderError},
};

pub struct ComputationFutureResult {
    pub results: Vec<crate::evaluation::context::QueryPartEvaluationContext>,
    pub source_id: std::sync::Arc<str>,
    pub future: FutureElementRef,
}

#[derive(Debug, thiserror::Error)]
pub enum ComputationQueryError {
    #[error(transparent)]
    Build(#[from] QueryBuilderError),
    #[error(transparent)]
    Evaluation(#[from] EvaluationError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("the computation index bundle does not support complete atomic output")]
    AtomicOutputUnsupported,
    #[error("the computation transaction belongs to a different session or resource bundle")]
    TransactionMismatch,
    #[error("an interrupted or failed computation requires reconstruction and recovery")]
    RecoveryRequired,
    #[error("cancel or await the active query operation before resource shutdown")]
    OperationInProgress,
    #[error("computation failed: {failure}; session rollback also failed: {rollback}")]
    Rollback {
        #[source]
        failure: Box<ComputationQueryError>,
        rollback: IndexError,
    },
}

pub type Result<T> = std::result::Result<T, ComputationQueryError>;

#[cfg(test)]
mod tests;
