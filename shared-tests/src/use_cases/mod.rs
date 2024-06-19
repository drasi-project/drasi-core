use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::query::QueryBuilder;
use drasi_query_ast::ast::Query;

pub mod building_comfort;
pub mod curbside_pickup;
pub mod incident_alert;
pub mod linear_regression;
pub mod min_value;
pub mod overdue_invoice;
pub mod remap;
pub mod sensor_heartbeat;

pub mod crosses_above_a_threshold;
pub mod crosses_above_and_stays_above;
pub mod crosses_above_three_times_in_an_hour;
pub mod decrease_by_ten;
pub mod exceeds_one_standard_deviation;
pub mod greater_than_a_threshold;
pub mod logical_conditions;
pub mod rolling_average_decrease_by_ten;
pub mod steps_happen_in_any_order;

#[async_trait]
pub trait QueryTestConfig {
    async fn config_query(&self, builder: QueryBuilder, query: Arc<Query>) -> QueryBuilder;
}
