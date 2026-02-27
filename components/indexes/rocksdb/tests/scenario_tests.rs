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

use std::{env, sync::Arc};

use async_trait::async_trait;

use drasi_core::{
    interface::{AccumulatorIndex, ElementIndex, FutureQueue},
    query::QueryBuilder,
};
use shared_tests::QueryTestConfig;
use uuid::Uuid;

use drasi_index_rocksdb::{
    element_index::{RocksDbElementIndex, RocksIndexOptions},
    future_queue::RocksDbFutureQueue,
    open_unified_db,
    result_index::RocksDbResultIndex,
    RocksDbSessionControl, RocksDbSessionState,
};

struct RocksDbQueryConfig {
    pub url: String,
}

impl RocksDbQueryConfig {
    pub fn new() -> Self {
        let base_path = match env::var("ROCKS_PATH") {
            Ok(url) => url,
            Err(_) => "test-data".to_string(),
        };
        // Create unique directory per test instance
        let url = format!("{}/{}", base_path, Uuid::new_v4());

        RocksDbQueryConfig { url }
    }

    #[allow(clippy::unwrap_used)]
    pub fn build_future_queue(&self, query_id: &str) -> RocksDbFutureQueue {
        let options = RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };
        let db = open_unified_db(&self.url, query_id, &options).unwrap();
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        RocksDbFutureQueue::new(db, session_state)
    }
}

impl Drop for RocksDbQueryConfig {
    fn drop(&mut self) {
        // Clean up the test-specific directory
        let _ = std::fs::remove_dir_all(&self.url);
    }
}

#[allow(clippy::unwrap_used)]
#[async_trait]
impl QueryTestConfig for RocksDbQueryConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder {
        log::info!("using in RocksDb indexes");
        let query_id = format!("test-{}", Uuid::new_v4());

        let options = RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };

        let db = open_unified_db(&self.url, &query_id, &options).unwrap();
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));

        let element_index = RocksDbElementIndex::new(db.clone(), options, session_state.clone());
        let ari = RocksDbResultIndex::new(db.clone(), session_state.clone());
        let fqi = RocksDbFutureQueue::new(db, session_state.clone());
        let session_control = Arc::new(RocksDbSessionControl::new(session_state));

        element_index.clear().await.unwrap();
        ari.clear().await.unwrap();
        fqi.clear().await.unwrap();

        let element_index = Arc::new(element_index);

        builder
            .with_element_index(element_index.clone())
            .with_archive_index(element_index.clone())
            .with_result_index(Arc::new(ari))
            .with_future_queue(Arc::new(fqi))
            .with_session_control(session_control)
    }
}

mod building_comfort {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    async fn building_comfort_use_case() {
        let test_config = RocksDbQueryConfig::new();
        building_comfort::building_comfort_use_case(&test_config).await;
    }
}

mod curbside_pickup {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    async fn order_ready_then_vehicle_arrives() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn vehicle_arrives_then_order_ready() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn vehicle_arrives_then_order_ready_duplicate() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready_duplicate(&test_config).await;
    }
}

mod incident_alert {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    pub async fn incident_alert() {
        let test_config = RocksDbQueryConfig::new();
        incident_alert::incident_alert(&test_config).await;
    }
}

mod min_value {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    pub async fn min_value() {
        let test_config = RocksDbQueryConfig::new();
        min_value::min_value(&test_config).await;
    }
}

mod overdue_invoice {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    pub async fn overdue_invoice() {
        let test_config = RocksDbQueryConfig::new();
        overdue_invoice::overdue_invoice(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    pub async fn overdue_count_persistent() {
        let test_config = RocksDbQueryConfig::new();
        overdue_invoice::overdue_count_persistent(&test_config).await;
    }
}

mod sensor_heartbeat {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    pub async fn not_reported() {
        let test_config = RocksDbQueryConfig::new();
        sensor_heartbeat::not_reported(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    pub async fn percent_not_reported() {
        let test_config = RocksDbQueryConfig::new();
        sensor_heartbeat::percent_not_reported(&test_config).await;
    }
}

mod temporal_retrieval {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::temporal_retrieval::get_version_by_timestamp;
    use shared_tests::temporal_retrieval::get_versions_by_timerange;

    #[tokio::test]
    #[serial]
    async fn get_version_by_timestamp() {
        let test_config = RocksDbQueryConfig::new();
        get_version_by_timestamp::get_version_by_timestamp(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn get_versions_by_range() {
        let test_config = RocksDbQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn get_versions_by_range_with_initial_value() {
        let test_config = RocksDbQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange_with_initial_value_flag(&test_config)
            .await;
    }
}

mod greater_than_a_threshold {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    pub async fn greater_than_a_threshold() {
        let test_config = RocksDbQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    pub async fn greater_than_a_threshold_by_customer() {
        let test_config = RocksDbQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold_by_customer(&test_config).await;
    }
}

mod linear_regression {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    async fn linear_gradient() {
        let test_config = RocksDbQueryConfig::new();
        linear_regression::linear_gradient(&test_config).await;
    }
}

mod index {
    use super::RocksDbQueryConfig;
    use drasi_core::interface::FutureQueue;
    use serial_test::serial;
    use uuid::Uuid;

    #[tokio::test]
    #[serial]
    async fn future_queue_push_always() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_always(&fqi).await;
    }

    #[tokio::test]
    #[serial]
    async fn future_queue_push_not_exists() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_not_exists(&fqi).await;
    }

    #[tokio::test]
    #[serial]
    async fn future_queue_clear_removes_all() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        shared_tests::index::future_queue::clear_removes_all(&fqi).await;
    }

    #[tokio::test]
    #[serial]
    async fn future_queue_push_overwrite() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_overwrite(&fqi).await;
    }
}

mod before {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    async fn before_value() {
        let test_config = RocksDbQueryConfig::new();
        before::before_value(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn before_sum() {
        let test_config = RocksDbQueryConfig::new();
        before::before_sum(&test_config).await;
    }
}

mod prev_unique {
    use super::RocksDbQueryConfig;
    use serial_test::serial;
    use shared_tests::use_cases::*;

    #[tokio::test]
    #[serial]
    async fn prev_unique() {
        let test_config = RocksDbQueryConfig::new();
        prev_distinct::prev_unique(&test_config).await;
    }

    #[tokio::test]
    #[serial]
    async fn prev_unique_with_match() {
        let test_config = RocksDbQueryConfig::new();
        prev_distinct::prev_unique_with_match(&test_config).await;
    }
}

mod collect_aggregation {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn collect_based_aggregation_test() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::collect_based_aggregation_test(&test_config).await;
    }

    #[tokio::test]
    async fn simple_aggregation_test() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::simple_aggregation_test(&test_config).await;
    }

    #[tokio::test]
    async fn collect_with_filter() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::collect_with_filter_test(&test_config).await;
    }

    #[tokio::test]
    async fn collect_objects() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::collect_objects_test(&test_config).await;
    }

    #[tokio::test]
    async fn collect_mixed_types() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::collect_mixed_types_test(&test_config).await;
    }

    #[tokio::test]
    async fn multiple_collects() {
        let test_config = RocksDbQueryConfig::new();
        collect_aggregation::multiple_collects_test(&test_config).await;
    }
}

mod session {
    use std::sync::Arc;

    use drasi_core::{
        evaluation::functions::aggregation::ValueAccumulator,
        interface::{
            AccumulatorIndex, ElementIndex, FutureQueue, PushType, ResultKey, ResultOwner,
            SessionControl,
        },
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
    };
    use drasi_index_rocksdb::{
        element_index::{RocksDbElementIndex, RocksIndexOptions},
        future_queue::RocksDbFutureQueue,
        open_unified_db,
        result_index::RocksDbResultIndex,
        RocksDbSessionControl, RocksDbSessionState,
    };
    use serial_test::serial;
    use uuid::Uuid;

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    #[serial]
    async fn session_rollback_discards_writes() {
        let url = format!("test-data/{}", Uuid::new_v4());
        let query_id = format!("test-{}", Uuid::new_v4());
        let options = RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };
        let db = open_unified_db(&url, &query_id, &options).unwrap();
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let element_index = RocksDbElementIndex::new(db.clone(), options, session_state.clone());
        let result_index = RocksDbResultIndex::new(db.clone(), session_state.clone());
        let future_queue = RocksDbFutureQueue::new(db, session_state.clone());
        let session_control = RocksDbSessionControl::new(session_state);

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

        let _ = std::fs::remove_dir_all(&url);
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    #[serial]
    async fn session_commit_persists_writes() {
        let url = format!("test-data/{}", Uuid::new_v4());
        let query_id = format!("test-{}", Uuid::new_v4());
        let options = RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };
        let db = open_unified_db(&url, &query_id, &options).unwrap();
        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let element_index = RocksDbElementIndex::new(db.clone(), options, session_state.clone());
        let result_index = RocksDbResultIndex::new(db.clone(), session_state.clone());
        let future_queue = RocksDbFutureQueue::new(db, session_state.clone());
        let session_control = RocksDbSessionControl::new(session_state);

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

        let _ = std::fs::remove_dir_all(&url);
    }
}

mod source_update_upsert {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn test_upsert_semantics() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_upsert_semantics(&test_config).await;
    }

    #[tokio::test]
    async fn test_partial_updates() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_partial_updates(&test_config).await;
    }

    #[tokio::test]
    async fn test_stateless_processing() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_stateless_processing(&test_config).await;
    }

    #[tokio::test]
    async fn test_query_matching() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_query_matching(&test_config).await;
    }

    #[tokio::test]
    async fn test_multiple_entities() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_multiple_entities(&test_config).await;
    }

    #[tokio::test]
    async fn test_relationship_upsert() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_relationship_upsert(&test_config).await;
    }

    #[tokio::test]
    async fn test_aggregation_with_upserts() {
        let test_config = RocksDbQueryConfig::new();
        source_update_upsert::test_aggregation_with_upserts(&test_config).await;
    }
}
