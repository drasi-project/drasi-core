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

use std::sync::{Arc, Mutex};

use shared_tests::redis_helpers::{setup_redis, RedisGuard};

use async_trait::async_trait;

use drasi_core::{
    index_cache::{
        cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
    },
    interface::ElementIndex,
    query::QueryBuilder,
};
use shared_tests::QueryTestConfig;
use uuid::Uuid;

use drasi_index_garnet::{
    element_index::GarnetElementIndex, future_queue::GarnetFutureQueue,
    result_index::GarnetResultIndex, GarnetSessionControl, GarnetSessionState,
};

struct GarnetQueryConfig {
    url: String,
    use_cache: bool,
    element_index: Mutex<Option<Arc<dyn ElementIndex>>>,
    redis_grd: RedisGuard,
}

#[allow(clippy::unwrap_used)]
impl GarnetQueryConfig {
    pub async fn new(use_cache: bool) -> Self {
        let redis = setup_redis().await;
        let url = redis.url().to_string();
        GarnetQueryConfig {
            url,
            use_cache,
            element_index: Mutex::new(None),
            redis_grd: redis,
        }
    }

    pub async fn build_future_queue(&self, query_id: &str) -> GarnetFutureQueue {
        let client = redis::Client::open(self.url.as_str()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        GarnetFutureQueue::new(query_id, connection, session_state)
    }

    pub fn get_element_index(&self) -> Arc<dyn ElementIndex> {
        self.element_index.lock().unwrap().clone().unwrap()
    }
}

#[allow(clippy::unwrap_used)]
#[async_trait]
impl QueryTestConfig for GarnetQueryConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder {
        log::info!("using in Garnet indexes");
        let query_id = format!("test-{}", Uuid::new_v4());

        let client = redis::Client::open(self.url.as_str()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = Arc::new(GarnetSessionControl::new(session_state.clone()));

        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
        let ari = GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());
        let fq = GarnetFutureQueue::new(&query_id, connection, session_state);

        let element_index = Arc::new(element_index);
        let archive_index = element_index.clone();

        *self.element_index.lock().unwrap() = Some(element_index.clone());

        if self.use_cache {
            let element_index = Arc::new(CachedElementIndex::new(element_index, 3).unwrap());
            let ari = CachedResultIndex::new(Arc::new(ari), 3).unwrap();

            builder
                .with_element_index(element_index)
                .with_archive_index(archive_index)
                .with_result_index(Arc::new(ari))
                .with_future_queue(Arc::new(fq))
                .with_session_control(session_control)
        } else {
            builder
                .with_element_index(element_index)
                .with_archive_index(archive_index)
                .with_result_index(Arc::new(ari))
                .with_future_queue(Arc::new(fq))
                .with_session_control(session_control)
        }
    }
}

mod building_comfort {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn building_comfort_use_case() {
        let test_config = GarnetQueryConfig::new(false).await;
        building_comfort::building_comfort_use_case(&test_config).await;
        let element_index = test_config.get_element_index();
        element_index.clear().await.unwrap();
        println!("Element Index Cleared");
        test_config.redis_grd.cleanup().await;
    }

    // #[tokio::test]
    // async fn building_comfort_use_case_with_cache() {
    //     let test_config = GarnetQueryConfig::new(true);
    //     building_comfort::building_comfort_use_case(&test_config).await;
    //     let element_index = test_config.get_element_index();
    //     element_index.clear().await.unwrap();
    // }
}

mod curbside_pickup {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives() {
        let test_config = GarnetQueryConfig::new(false).await;
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready() {
        let test_config = GarnetQueryConfig::new(false).await;
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready_duplicate() {
        let test_config = GarnetQueryConfig::new(false).await;
        curbside_pickup::vehicle_arrives_then_order_ready_duplicate(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives_with_cache() {
        let test_config = GarnetQueryConfig::new(true).await;
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready_with_cache() {
        let test_config = GarnetQueryConfig::new(true).await;
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod incident_alert {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn incident_alert() {
        let test_config = GarnetQueryConfig::new(false).await;
        incident_alert::incident_alert(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    pub async fn incident_alert_with_cache() {
        let test_config = GarnetQueryConfig::new(true).await;
        incident_alert::incident_alert(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod min_value {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn min_value() {
        let test_config = GarnetQueryConfig::new(false).await;
        min_value::min_value(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    pub async fn min_value_with_cache() {
        let test_config = GarnetQueryConfig::new(true).await;
        min_value::min_value(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod overdue_invoice {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn overdue_invoice() {
        let test_config = GarnetQueryConfig::new(false).await;
        overdue_invoice::overdue_invoice(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    pub async fn overdue_count_persistent() {
        let test_config = GarnetQueryConfig::new(false).await;
        overdue_invoice::overdue_count_persistent(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod sensor_heartbeat {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn not_reported() {
        let test_config = GarnetQueryConfig::new(false).await;
        sensor_heartbeat::not_reported(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    pub async fn percent_not_reported() {
        let test_config = GarnetQueryConfig::new(false).await;
        sensor_heartbeat::percent_not_reported(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod temporal_retrieval {
    use super::GarnetQueryConfig;
    use shared_tests::temporal_retrieval::get_version_by_timestamp;
    use shared_tests::temporal_retrieval::get_versions_by_timerange;

    #[tokio::test]
    async fn get_version_by_timestamp() {
        let test_config = GarnetQueryConfig::new(false).await;
        get_version_by_timestamp::get_version_by_timestamp(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn get_versions_by_range() {
        let test_config = GarnetQueryConfig::new(false).await;
        get_versions_by_timerange::get_versions_by_timerange(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn get_versions_by_range_with_initial_value() {
        let test_config = GarnetQueryConfig::new(false).await;
        get_versions_by_timerange::get_versions_by_timerange_with_initial_value_flag(&test_config)
            .await;
        test_config.redis_grd.cleanup().await;
    }
}

mod greater_than_a_threshold {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn greater_than_a_threshold() {
        let test_config = GarnetQueryConfig::new(false).await;
        greater_than_a_threshold::greater_than_a_threshold(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    pub async fn greater_than_a_threshold_by_customer() {
        let test_config = GarnetQueryConfig::new(false).await;
        greater_than_a_threshold::greater_than_a_threshold_by_customer(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod steps_happen_in_any_order {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn steps_happen_in_any_order() {
        let test_config = GarnetQueryConfig::new(false).await;
        steps_happen_in_any_order::steps_happen_in_any_order(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod linear_regression {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn linear_gradient() {
        let test_config = GarnetQueryConfig::new(false).await;
        linear_regression::linear_gradient(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod index {
    use super::GarnetQueryConfig;
    use drasi_core::interface::FutureQueue;
    use uuid::Uuid;

    #[tokio::test]
    async fn future_queue_push_always() {
        let test_config = GarnetQueryConfig::new(false).await;
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_always(&fqi).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn future_queue_push_not_exists() {
        let test_config = GarnetQueryConfig::new(false).await;
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_not_exists(&fqi).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn future_queue_clear_removes_all() {
        let test_config = GarnetQueryConfig::new(false).await;
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        shared_tests::index::future_queue::clear_removes_all(&fqi).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn future_queue_push_overwrite() {
        let test_config = GarnetQueryConfig::new(false).await;
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_overwrite(&fqi).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod before {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn before_value() {
        let test_config = GarnetQueryConfig::new(false).await;
        before::before_value(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn before_sum() {
        let test_config = GarnetQueryConfig::new(false).await;
        before::before_sum(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod prev_unique {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn prev_unique() {
        let test_config = GarnetQueryConfig::new(false).await;
        prev_distinct::prev_unique(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod collect_aggregation {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn collect_based_aggregation_test() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::collect_based_aggregation_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn simple_aggregation_test() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::simple_aggregation_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn collect_with_filter() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::collect_with_filter_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn collect_objects() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::collect_objects_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn collect_mixed_types() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::collect_mixed_types_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn multiple_collects() {
        let test_config = GarnetQueryConfig::new(false).await;
        collect_aggregation::multiple_collects_test(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}

mod session {
    use std::sync::Arc;

    use drasi_core::{
        evaluation::functions::aggregation::ValueAccumulator,
        interface::{
            AccumulatorIndex, ElementIndex, FutureQueue, LazySortedSetStore, PushType, ResultKey,
            ResultOwner, SessionControl,
        },
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
    };
    use drasi_index_garnet::{
        element_index::GarnetElementIndex, future_queue::GarnetFutureQueue,
        result_index::GarnetResultIndex, GarnetSessionControl, GarnetSessionState,
    };
    use ordered_float::OrderedFloat;
    use shared_tests::redis_helpers::setup_redis;
    use uuid::Uuid;

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_rollback_discards_writes() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
        let result_index =
            GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());
        let future_queue = GarnetFutureQueue::new(&query_id, connection, session_state);

        element_index.clear().await.unwrap();
        result_index.clear().await.unwrap();
        future_queue.clear().await.unwrap();

        let element_ref = ElementReference::new("source1", "node1");
        let node = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
        };
        let result_key = ResultKey::InputHash(1);
        let result_owner = ResultOwner::Function(0);

        // Begin session, write to all three indexes, then rollback
        session_control.begin().await.unwrap();

        element_index.set_element(&node, &vec![]).await.unwrap();
        result_index
            .set(
                result_key.clone(),
                result_owner.clone(),
                Some(ValueAccumulator::Count { value: 42 }),
            )
            .await
            .unwrap();
        future_queue
            .push(PushType::Always, 1, 1, &element_ref, 10, 20)
            .await
            .unwrap();

        session_control.rollback();

        // Verify nothing persisted
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_none(), "element should not persist after rollback");

        let acc = result_index.get(&result_key, &result_owner).await.unwrap();
        assert!(
            acc.is_none(),
            "accumulator should not persist after rollback"
        );

        let due = future_queue.peek_due_time().await.unwrap();
        assert!(due.is_none(), "future queue should be empty after rollback");

        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_commit_persists_writes() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
        let result_index =
            GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());
        let future_queue = GarnetFutureQueue::new(&query_id, connection, session_state);

        element_index.clear().await.unwrap();
        result_index.clear().await.unwrap();
        future_queue.clear().await.unwrap();

        let element_ref = ElementReference::new("source1", "node1");
        let node = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
        };
        let result_key = ResultKey::InputHash(1);
        let result_owner = ResultOwner::Function(0);

        // Begin session, write to all three indexes, then commit
        session_control.begin().await.unwrap();

        element_index.set_element(&node, &vec![]).await.unwrap();
        result_index
            .set(
                result_key.clone(),
                result_owner.clone(),
                Some(ValueAccumulator::Count { value: 42 }),
            )
            .await
            .unwrap();
        future_queue
            .push(PushType::Always, 1, 1, &element_ref, 10, 20)
            .await
            .unwrap();

        session_control.commit().await.unwrap();

        // Verify data persisted
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_some(), "element should persist after commit");

        let acc = result_index.get(&result_key, &result_owner).await.unwrap();
        assert!(acc.is_some(), "accumulator should persist after commit");
        match acc.unwrap() {
            ValueAccumulator::Count { value } => assert_eq!(value, 42),
            other => panic!("expected Count, got {other:?}"),
        }

        let due = future_queue.peek_due_time().await.unwrap();
        assert_eq!(due, Some(20));

        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_read_your_writes() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
        let result_index =
            GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());

        element_index.clear().await.unwrap();
        result_index.clear().await.unwrap();

        let element_ref = ElementReference::new("source1", "node1");
        let node = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
        };
        let result_key = ResultKey::InputHash(1);
        let result_owner = ResultOwner::Function(0);

        session_control.begin().await.unwrap();

        element_index.set_element(&node, &vec![]).await.unwrap();
        result_index
            .set(
                result_key.clone(),
                result_owner.clone(),
                Some(ValueAccumulator::Count { value: 42 }),
            )
            .await
            .unwrap();

        // Read within the same session — buffer should serve these
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_some(), "element should be visible within session");

        let acc = result_index.get(&result_key, &result_owner).await.unwrap();
        assert!(
            acc.is_some(),
            "accumulator should be visible within session"
        );
        match acc.unwrap() {
            ValueAccumulator::Count { value } => assert_eq!(value, 42),
            other => panic!("expected Count, got {other:?}"),
        }

        session_control.commit().await.unwrap();

        // Verify still readable after commit
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_some(), "element should persist after commit");

        let acc = result_index.get(&result_key, &result_owner).await.unwrap();
        assert!(acc.is_some(), "accumulator should persist after commit");

        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_rollback_then_retry() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
        let result_index =
            GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());

        element_index.clear().await.unwrap();
        result_index.clear().await.unwrap();

        let element_ref = ElementReference::new("source1", "node1");
        let result_key = ResultKey::InputHash(1);
        let result_owner = ResultOwner::Function(0);

        // First session: write v1, then rollback
        let node_v1 = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
        };

        session_control.begin().await.unwrap();
        element_index.set_element(&node_v1, &vec![]).await.unwrap();
        result_index
            .set(
                result_key.clone(),
                result_owner.clone(),
                Some(ValueAccumulator::Count { value: 99 }),
            )
            .await
            .unwrap();
        session_control.rollback();

        // Second session: write v2, then commit
        let node_v2 = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::new(),
        };

        session_control.begin().await.unwrap();
        element_index.set_element(&node_v2, &vec![]).await.unwrap();
        result_index
            .set(
                result_key.clone(),
                result_owner.clone(),
                Some(ValueAccumulator::Count { value: 77 }),
            )
            .await
            .unwrap();
        session_control.commit().await.unwrap();

        // Verify v2 persisted (not v1)
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_some(), "element should persist after commit");
        match elem.unwrap().as_ref() {
            Element::Node { metadata, .. } => {
                assert_eq!(metadata.effective_from, 2000, "should be v2, not v1");
            }
            _ => panic!("expected Node"),
        }

        let acc = result_index.get(&result_key, &result_owner).await.unwrap();
        assert!(acc.is_some(), "accumulator should persist after commit");
        match acc.unwrap() {
            ValueAccumulator::Count { value } => {
                assert_eq!(value, 77, "should be second session's value");
            }
            other => panic!("expected Count, got {other:?}"),
        }

        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_increment_stacking() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let result_index =
            GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());

        result_index.clear().await.unwrap();

        session_control.begin().await.unwrap();

        // Two increments to the same key within one session
        result_index
            .increment_value_count(1, OrderedFloat(5.0), 1)
            .await
            .unwrap();
        result_index
            .increment_value_count(1, OrderedFloat(5.0), 1)
            .await
            .unwrap();

        // Within session: buffer should show accumulated count of 2
        let count = result_index
            .get_value_count(1, OrderedFloat(5.0))
            .await
            .unwrap();
        assert_eq!(
            count, 2,
            "buffer should show accumulated count within session"
        );

        session_control.commit().await.unwrap();

        // After commit: Redis should show 2
        let count = result_index
            .get_value_count(1, OrderedFloat(5.0))
            .await
            .unwrap();
        assert_eq!(count, 2, "committed count should be 2");

        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn session_delete_then_reinsert() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let query_id = format!("test-{}", Uuid::new_v4());

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = GarnetSessionControl::new(session_state.clone());
        let element_index =
            GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());

        element_index.clear().await.unwrap();

        let element_ref = ElementReference::new("source1", "node1");

        // Write v1 directly (no session — goes straight to Redis)
        let node_v1 = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::new(),
        };
        element_index.set_element(&node_v1, &vec![]).await.unwrap();

        // Within a session: delete then reinsert with different effective_from
        let node_v2 = Element::Node {
            metadata: ElementMetadata {
                reference: element_ref.clone(),
                labels: Arc::new([Arc::from("TestLabel")]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::new(),
        };

        session_control.begin().await.unwrap();
        element_index.delete_element(&element_ref).await.unwrap();
        element_index.set_element(&node_v2, &vec![]).await.unwrap();
        session_control.commit().await.unwrap();

        // Verify v2 persisted
        let elem = element_index.get_element(&element_ref).await.unwrap();
        assert!(elem.is_some(), "element should exist after delete+reinsert");
        match elem.unwrap().as_ref() {
            Element::Node { metadata, .. } => {
                assert_eq!(
                    metadata.effective_from, 2000,
                    "should be v2 after delete+reinsert"
                );
            }
            _ => panic!("expected Node"),
        }

        redis.cleanup().await;
    }
}

mod source_update_upsert {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn test_upsert_semantics() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_upsert_semantics(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_partial_updates() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_partial_updates(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_stateless_processing() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_stateless_processing(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_query_matching() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_query_matching(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_multiple_entities() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_multiple_entities(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_relationship_upsert() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_relationship_upsert(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }

    #[tokio::test]
    async fn test_aggregation_with_upserts() {
        let test_config = GarnetQueryConfig::new(false).await;
        source_update_upsert::test_aggregation_with_upserts(&test_config).await;
        test_config.redis_grd.cleanup().await;
    }
}
