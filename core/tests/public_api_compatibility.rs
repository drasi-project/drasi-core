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

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::{
    evaluation::{
        functions::{future::RegisterFutureFunctions, FunctionRegistry},
        ExpressionEvaluator,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    index_cache::{
        cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
        shadowed_future_queue::ShadowedFutureQueue,
    },
    interface::{
        FutureQueue, IndexError, IndexSet, NoOpSessionControl, PushType, SessionControl,
        SessionGuard,
    },
    models::ElementReference,
};

struct ExternalControl;

#[async_trait]
impl SessionControl for ExternalControl {
    async fn begin(&self) -> Result<(), IndexError> {
        Ok(())
    }
    async fn commit(&self) -> Result<(), IndexError> {
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        Ok(())
    }
}

#[tokio::test]
async fn existing_constructors_traits_and_struct_literals_still_work() {
    let elements = Arc::new(InMemoryElementIndex::new());
    let results = Arc::new(InMemoryResultIndex::new());
    let queue = Arc::new(InMemoryFutureQueue::new());
    let _indexes = IndexSet {
        element_index: Arc::new(CachedElementIndex::new(elements.clone(), 8).unwrap()),
        archive_index: elements,
        result_index: Arc::new(CachedResultIndex::new(results.clone(), 8).unwrap()),
        future_queue: Arc::new(ShadowedFutureQueue::new(queue.clone())),
        session_control: Arc::new(NoOpSessionControl),
    };
    let registry = Arc::new(FunctionRegistry::new());
    let evaluator = Arc::new(ExpressionEvaluator::new(registry.clone(), results.clone()));
    registry.register_future_functions(queue.clone(), results, Arc::downgrade(&evaluator));
    let control: Arc<dyn SessionControl> = Arc::new(ExternalControl);
    control.begin().await.unwrap();
    control.commit().await.unwrap();
    control.rollback().unwrap();
    let guard = SessionGuard::begin(control).await.unwrap();
    let _: () = guard.commit().await.unwrap();

    let reference = ElementReference::new("source", "node");
    assert!(queue
        .push(PushType::Always, 1, 2, &reference, 10, 20)
        .await
        .unwrap());
    assert_eq!(queue.pop().await.unwrap().unwrap().element_ref, reference);
    queue.remove(1, 2).await.unwrap();
}

#[test]
fn original_error_enums_remain_exhaustively_matchable() {
    use drasi_core::{evaluation::EvaluationError, interface::QueryBuilderError};

    fn index(error: IndexError) {
        match error {
            IndexError::IOError
            | IndexError::NotSupported
            | IndexError::ArchiveNotEnabled
            | IndexError::CorruptedData
            | IndexError::ConnectionFailed(_)
            | IndexError::UnknownStore(_)
            | IndexError::Other(_) => {}
        }
    }
    fn evaluation(error: EvaluationError) {
        match error {
            EvaluationError::DivideByZero
            | EvaluationError::InvalidType { .. }
            | EvaluationError::UnknownIdentifier(_)
            | EvaluationError::UnknownFunction(_)
            | EvaluationError::IndexError(_)
            | EvaluationError::MiddlewareError(_)
            | EvaluationError::ParseError
            | EvaluationError::InvalidContext
            | EvaluationError::OutOfRange { .. }
            | EvaluationError::OverflowError
            | EvaluationError::FunctionError(_)
            | EvaluationError::CorruptData
            | EvaluationError::InvalidArgument
            | EvaluationError::UnknownProperty { .. }
            | EvaluationError::FormatError { .. } => {}
        }
    }
    fn builder(error: QueryBuilderError) {
        match error {
            QueryBuilderError::MiddlewareSetupError(_)
            | QueryBuilderError::ParserError(_)
            | QueryBuilderError::EvaluationError(_) => {}
        }
    }

    index(IndexError::NotSupported);
    evaluation(EvaluationError::ParseError);
    builder(QueryBuilderError::EvaluationError(
        EvaluationError::ParseError,
    ));
}

#[test]
fn memory_sessions_do_not_require_a_tokio_runtime() {
    futures::executor::block_on(async {
        let guard = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let _: () = guard.commit().await.unwrap();
    });
}
