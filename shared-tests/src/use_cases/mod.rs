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
pub mod unwind;

pub mod crosses_above_a_threshold;
pub mod crosses_above_and_stays_above;
pub mod crosses_above_three_times_in_an_hour;
pub mod decrease_by_ten;
pub mod document;
pub mod exceeds_one_standard_deviation;
pub mod greater_than_a_threshold;
pub mod logical_conditions;
pub mod rolling_average_decrease_by_ten;
pub mod steps_happen_in_any_order;

#[async_trait]
pub trait QueryTestConfig {
    async fn config_query(&self, builder: QueryBuilder, query: Arc<Query>) -> QueryBuilder;
}
