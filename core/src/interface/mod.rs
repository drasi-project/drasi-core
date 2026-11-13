// Copyright 2024 The Drasi Authors.
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

mod durability;
mod index_backend;
mod query_clock;

pub use drasi_core_types::interface::{
    index_joins_by_label, CheckpointStore, ElementArchiveIndex, ElementIndex, ElementResult,
    ElementStream, FutureElementRef, FutureQueue, FutureQueueConsumer, IndexJoin,
    IndexJoinsByLabel, IndexSet, LiveResultsWriter, NoOpSessionControl, OutboxPageLimits,
    OutboxWriter, PushType, RowMutation, SessionControl, SessionGuard, SourceCheckpoint,
};
pub use drasi_core_types::interface::{
    AccumulatorIndex, LazySortedSetStore, ResultIndex, ResultKey, ResultOwner, ResultSequence,
    ResultSequenceCounter,
};
pub use drasi_core_types::interface::{
    MiddlewareError, MiddlewareSetupError, SourceMiddleware, SourceMiddlewareFactory,
};
pub use drasi_core_types::{CheckpointKey, OutputPartition, StorageScope, StorePartition};
use drasi_query_ast::api::QueryParseError;
pub use durability::{DurabilityRequirementError, FailureMode, FailureSurvival, StorageDurability};
pub use index_backend::CreatedIndexes;
pub use index_backend::IndexBackendPlugin;
pub use query_clock::QueryClock;
use thiserror::Error;

use crate::evaluation::EvaluationError;

pub use drasi_core_types::interface::IndexError;

#[derive(Error, Debug)]
pub enum QueryBuilderError {
    #[error("Unsupported query feature: {feature}")]
    UnsupportedFeature { feature: &'static str },

    #[error("Middleware setup error: {0}")]
    MiddlewareSetupError(MiddlewareSetupError),

    #[error("Parser error: {0}")]
    ParserError(QueryParseError),

    #[error("Evaluation error: {0}")]
    EvaluationError(EvaluationError),

    #[error("Index setup error: {0}")]
    IndexSetup(#[source] IndexError),
}

impl From<MiddlewareSetupError> for QueryBuilderError {
    fn from(e: MiddlewareSetupError) -> Self {
        QueryBuilderError::MiddlewareSetupError(e)
    }
}

impl From<QueryParseError> for QueryBuilderError {
    fn from(e: QueryParseError) -> Self {
        QueryBuilderError::ParserError(e)
    }
}

impl From<EvaluationError> for QueryBuilderError {
    fn from(e: EvaluationError) -> Self {
        QueryBuilderError::EvaluationError(e)
    }
}
