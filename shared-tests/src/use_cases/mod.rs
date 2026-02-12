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

use async_trait::async_trait;
use drasi_core::{evaluation::context::QueryPartEvaluationContext, query::QueryBuilder};

/// Placeholder for `row_signature` in test assertions. The `data_eq()` method
/// and `contains_data()` helper ignore this field when comparing results.
pub const IGNORED_ROW_SIGNATURE: u64 = 0;

/// Checks if any result context matches the expected data, ignoring `row_signature`.
pub fn contains_data(
    results: &[QueryPartEvaluationContext],
    expected: &QueryPartEvaluationContext,
) -> bool {
    results.iter().any(|ctx| ctx.data_eq(expected))
}

pub mod building_comfort;
pub mod collect_aggregation;
pub mod curbside_pickup;
pub mod dapr_state_store;
pub mod decoder;
pub mod incident_alert;
pub mod linear_regression;
pub mod min_value;
pub mod optional_match;
pub mod overdue_invoice;
pub mod parse_json;
pub mod promote;
pub mod relabel;
pub mod remap;
pub mod sensor_heartbeat;
pub mod source_update_upsert;
pub mod unwind;

pub mod before;
pub mod crosses_above_a_threshold;
pub mod crosses_above_and_stays_above;
pub mod crosses_above_three_times_in_an_hour;
pub mod decrease_by_ten;
pub mod document;
pub mod exceeds_one_standard_deviation;
pub mod future_aggregations;
pub mod greater_than_a_threshold;
pub mod logical_conditions;
pub mod prev_distinct;
pub mod rolling_average_decrease_by_ten;
pub mod steps_happen_in_any_order;
pub mod windows;

#[async_trait]
pub trait QueryTestConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder;
}
