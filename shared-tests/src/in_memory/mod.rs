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

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex, query::QueryBuilder,
};

use crate::QueryTestConfig;

struct InMemoryQueryConfig {}

impl InMemoryQueryConfig {
    pub fn new() -> Self {
        InMemoryQueryConfig {}
    }
}

#[async_trait]
impl QueryTestConfig for InMemoryQueryConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder {
        log::info!("using in memory indexes");
        let mut element_index = InMemoryElementIndex::new();
        element_index.enable_archive();
        let element_index = Arc::new(element_index);

        builder
            .with_element_index(element_index.clone())
            .with_archive_index(element_index.clone())
    }
}

mod building_comfort {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    async fn building_comfort_use_case() {
        let test_config = InMemoryQueryConfig::new();
        building_comfort::building_comfort_use_case(&test_config).await;
    }
}

mod curbside_pickup {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives() {
        let test_config = InMemoryQueryConfig::new();
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
    }

    #[tokio::test]
    async fn vehicle_arrives_then_order_ready() {
        let test_config = InMemoryQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
    }

    #[tokio::test]
    async fn vehicle_arrives_then_order_ready_duplicate() {
        let test_config = InMemoryQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready_duplicate(&test_config).await;
    }
}

mod incident_alert {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn incident_alert() {
        let test_config = InMemoryQueryConfig::new();
        incident_alert::incident_alert(&test_config).await;
    }
}

mod linear_regression {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn linear_gradient() {
        let test_config = InMemoryQueryConfig::new();
        linear_regression::linear_gradient(&test_config).await;
    }
}

mod min_value {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn min_value() {
        let test_config = InMemoryQueryConfig::new();
        min_value::min_value(&test_config).await;
    }
}

mod remap {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn remap() {
        let test_config = InMemoryQueryConfig::new();
        remap::remap(&test_config).await;
    }
}

mod collect_movies {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn collect_movies() {
        let test_config = InMemoryQueryConfig::new();
        collect_movies::collect_movies(&test_config).await;
    }
}

mod overdue_invoice {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn overdue_invoice() {
        let test_config = InMemoryQueryConfig::new();
        overdue_invoice::overdue_invoice(&test_config).await;
    }

    #[tokio::test]
    pub async fn overdue_count_persistent() {
        let test_config = InMemoryQueryConfig::new();
        overdue_invoice::overdue_count_persistent(&test_config).await;
    }
}

mod sensor_heartbeat {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn not_reported() {
        let test_config = InMemoryQueryConfig::new();
        sensor_heartbeat::not_reported(&test_config).await;
    }

    #[tokio::test]
    pub async fn percent_not_reported() {
        let test_config = InMemoryQueryConfig::new();
        sensor_heartbeat::percent_not_reported(&test_config).await;
    }
}

mod temporal_retrieval {
    use super::InMemoryQueryConfig;
    use crate::temporal_retrieval::get_version_by_timestamp;
    use crate::temporal_retrieval::get_versions_by_timerange;

    #[tokio::test]
    async fn get_version_by_timestamp() {
        let test_config = InMemoryQueryConfig::new();
        get_version_by_timestamp::get_version_by_timestamp(&test_config).await;
    }

    #[tokio::test]
    async fn get_versions_by_timerange_with_initial_value_flag() {
        let test_config = InMemoryQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange_with_initial_value_flag(&test_config)
            .await;
    }

    #[tokio::test]
    async fn get_versions_by_range() {
        let test_config = InMemoryQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange(&test_config).await;
    }
}

mod crosses_above_a_threshold {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn crosses_above_a_threshold() {
        let test_config = InMemoryQueryConfig::new();
        crosses_above_a_threshold::crosses_above_a_threshold(&test_config).await;
    }

    #[tokio::test]
    pub async fn crosses_above_a_threshold_with_overdue_days() {
        let test_config = InMemoryQueryConfig::new();
        crosses_above_a_threshold::crosses_above_a_threshold_with_overdue_days(&test_config).await;
    }
}

mod greater_than_a_threshold {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn greater_than_a_threshold() {
        let test_config = InMemoryQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold(&test_config).await;
    }

    #[tokio::test]
    pub async fn greater_than_a_threshold_by_customer() {
        let test_config = InMemoryQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold_by_customer(&test_config).await;
    }
}

mod crosses_above_and_stays_above {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn crosses_above_and_stays_above() {
        let test_config = InMemoryQueryConfig::new();
        crosses_above_and_stays_above::crosses_above_and_stays_above(&test_config).await;
    }
}

mod crosses_above_three_times_in_an_hour {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn crosses_above_three_times_in_an_hour() {
        let test_config = InMemoryQueryConfig::new();
        crosses_above_three_times_in_an_hour::crosses_above_three_times_in_an_hour(&test_config)
            .await;
    }
}

mod logical_conditions {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn logical_conditions() {
        let test_config = InMemoryQueryConfig::new();
        logical_conditions::logical_conditions(&test_config).await;
    }
}

mod steps_happen_in_any_order {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn steps_happen_in_any_order() {
        let test_config = InMemoryQueryConfig::new();
        steps_happen_in_any_order::steps_happen_in_any_order(&test_config).await;
    }
}

mod rolling_average_decrease_by_ten {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn rolling_average_decrease_by_ten() {
        let test_config = InMemoryQueryConfig::new();
        rolling_average_decrease_by_ten::rolling_average_decrease_by_ten(&test_config).await;
    }
}
mod decreases_by_ten {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn decrease_by_ten() {
        let test_config = InMemoryQueryConfig::new();
        decrease_by_ten::decrease_by_ten(&test_config).await;
    }

    #[tokio::test]
    pub async fn decrease_by_ten_percent() {
        let test_config = InMemoryQueryConfig::new();
        decrease_by_ten::decrease_by_ten_percent(&test_config).await;
    }
}
mod exceeds_one_standard_deviation {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    pub async fn exceeds_one_standard_deviation() {
        let test_config = InMemoryQueryConfig::new();
        exceeds_one_standard_deviation::exceeds_one_standard_deviation(&test_config).await;
    }
}
mod unit_tests {
    use crate::sequence_counter;
    use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;

    #[tokio::test]
    pub async fn sequence_counter() {
        let subject = InMemoryResultIndex::new();
        sequence_counter::sequence_counter(&subject).await;
    }
}

mod index {
    use crate::index;
    use drasi_core::{
        in_memory_index::in_memory_future_queue::InMemoryFutureQueue, interface::FutureQueue,
    };

    #[tokio::test]
    async fn future_queue_push_always() {
        let fqi = InMemoryFutureQueue::new();
        fqi.clear().await.unwrap();
        index::future_queue::push_always(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_not_exists() {
        let fqi = InMemoryFutureQueue::new();
        fqi.clear().await.unwrap();
        index::future_queue::push_not_exists(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_overwrite() {
        let fqi = InMemoryFutureQueue::new();
        fqi.clear().await.unwrap();
        index::future_queue::push_overwrite(&fqi).await;
    }
}

mod document {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    async fn document() {
        let test_config = InMemoryQueryConfig::new();
        document::document(&test_config).await;
    }
}

mod unwind {
    use super::InMemoryQueryConfig;
    use crate::use_cases::*;

    #[tokio::test]
    async fn unwind() {
        let test_config = InMemoryQueryConfig::new();
        unwind::unwind(&test_config).await;
    }
}
