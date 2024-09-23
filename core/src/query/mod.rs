#![allow(clippy::unwrap_used)]
mod auto_future_queue_consumer;
mod continuous_query;
mod query_builder;

pub use auto_future_queue_consumer::AutoFutureQueueConsumer;
pub use continuous_query::ContinuousQuery;
pub use query_builder::QueryBuilder;

#[cfg(test)]
mod tests;
