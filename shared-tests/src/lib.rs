use async_trait::async_trait;
use drasi_core::query::QueryBuilder;

pub mod index;
pub mod sequence_counter;
pub mod temporal_retrieval;
pub mod use_cases;

#[cfg(test)]
mod in_memory;

#[async_trait]
pub trait QueryTestConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder;
}
