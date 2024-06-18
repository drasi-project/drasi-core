use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    }, interface::{FutureElementRef, FutureQueueConsumer}, query::QueryBuilder
};

#[tokio::test]
async fn dependency_leaks() {
    let query_str = "MATCH (n:Person) RETURN n";    
    let mut builder = QueryBuilder::new(query_str);

    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());

    builder = builder.with_element_index(element_index.clone());
    builder = builder.with_archive_index(element_index.clone());
    builder = builder.with_result_index(result_index.clone());
    builder = builder.with_future_queue(future_queue.clone());

    let query = builder.build().await;
    let fq = Arc::new(TestFutureConsumer {});
    query.set_future_consumer(fq).await;

    query.terminate_future_consumer().await;
    drop(query);

    assert_eq!(Arc::strong_count(&element_index), 1);
    assert_eq!(Arc::strong_count(&result_index), 1);
    assert_eq!(Arc::strong_count(&future_queue), 1);
}

struct TestFutureConsumer {}

#[async_trait]
impl FutureQueueConsumer for TestFutureConsumer {
    async fn on_due(
        &self,
        _future_ref: &FutureElementRef,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn on_error(
        &self,
        _future_ref: &FutureElementRef,
        _errorr: Box<dyn std::error::Error + Send + Sync>,
    ) {
    }

    fn now(&self) -> u64 {
        0
    }
}
