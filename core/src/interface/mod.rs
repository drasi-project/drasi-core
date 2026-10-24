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

mod checkpoint_store;
mod durability;
mod element_index;
mod future_queue;
mod index_backend;
mod live_results_writer;
mod outbox_writer;
mod query_clock;
mod result_index;
mod session_control;
mod source_middleware;

pub use checkpoint_store::CheckpointStore;
pub use checkpoint_store::SourceCheckpoint;
use drasi_query_ast::api::QueryParseError;
pub use durability::{DurabilityRequirementError, FailureMode, FailureSurvival, StorageDurability};
pub use element_index::ElementArchiveIndex;
pub use element_index::ElementIndex;
pub use element_index::ElementResult;
pub use element_index::ElementStream;
pub use future_queue::FutureElementRef;
pub use future_queue::FutureQueue;
pub use future_queue::FutureQueueConsumer;
pub use future_queue::PushType;
pub use index_backend::CreatedIndexes;
pub use index_backend::IndexBackendPlugin;
pub use index_backend::IndexSet;
pub use live_results_writer::LiveResultsWriter;
pub use live_results_writer::RowMutation;
pub use outbox_writer::{OutboxPageLimits, OutboxWriter};
pub use query_clock::QueryClock;
pub use result_index::AccumulatorIndex;
pub use result_index::LazySortedSetStore;
pub use result_index::ResultIndex;
pub use result_index::ResultKey;
pub use result_index::ResultOwner;
pub use result_index::ResultSequence;
pub use result_index::ResultSequenceCounter;
pub use session_control::NoOpSessionControl;
pub use session_control::SessionControl;
pub use session_control::SessionGuard;
pub use source_middleware::MiddlewareError;
pub use source_middleware::MiddlewareSetupError;
pub use source_middleware::SourceMiddleware;
pub use source_middleware::SourceMiddlewareFactory;
use thiserror::Error;

use crate::evaluation::EvaluationError;

pub use drasi_core_types::interface::IndexError;

#[derive(Error, Debug)]
pub enum QueryBuilderError {
    #[error("Middleware setup error: {0}")]
    MiddlewareSetupError(MiddlewareSetupError),

    #[error("Parser error: {0}")]
    ParserError(QueryParseError),

    #[error("Evaluation error: {0}")]
    EvaluationError(EvaluationError),
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
