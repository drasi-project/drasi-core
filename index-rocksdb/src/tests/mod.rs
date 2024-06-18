use std::{env, sync::Arc};

use async_trait::async_trait;
use drasi_query_ast::ast::Query;
use drasi_core::{
    interface::{AccumulatorIndex, ElementIndex, FutureQueue},
    path_solver::match_path::MatchPath,
    query::QueryBuilder,
};
use shared_tests::QueryTestConfig;
use uuid::Uuid;

use crate::{
    element_index::{self, RocksDbElementIndex},
    future_queue::RocksDbFutureQueue,
    result_index::RocksDbResultIndex,
};

struct RocksDbQueryConfig {
    pub url: String,
}

impl RocksDbQueryConfig {
    pub fn new() -> Self {
        let url = match env::var("ROCKS_PATH") {
            Ok(url) => url,
            Err(_) => "test-data".to_string(),
        };

        RocksDbQueryConfig { url }
    }

    pub fn build_future_queue(&self, query_id: &str) -> RocksDbFutureQueue {
        RocksDbFutureQueue::new(query_id, &self.url).unwrap()
    }
}

#[async_trait]
impl QueryTestConfig for RocksDbQueryConfig {
    async fn config_query(&self, builder: QueryBuilder) -> QueryBuilder {
        log::info!("using in RocksDb indexes");
        let query_id = format!("test-{}", Uuid::new_v4().to_string());

        let options = element_index::RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };

        let element_index =
            RocksDbElementIndex::new(&query_id, &self.url, options)
                .unwrap();
        let ari = RocksDbResultIndex::new(&query_id, &self.url).unwrap();
        let fqi = RocksDbFutureQueue::new(&query_id, &self.url).unwrap();

        element_index.clear().await.unwrap();
        ari.clear().await.unwrap();
        fqi.clear().await.unwrap();

        let element_index = Arc::new(element_index);

        builder
            .with_element_index(element_index.clone())
            .with_archive_index(element_index.clone())
            .with_result_index(Arc::new(ari))
            .with_future_queue(Arc::new(fqi))
    }
}

mod building_comfort {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn building_comfort_use_case() {
        let test_config = RocksDbQueryConfig::new();
        building_comfort::building_comfort_use_case(&test_config).await;
    }
}

mod curbside_pickup {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
    }

    #[tokio::test]
    async fn vehicle_arrives_then_order_ready() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
    }

    #[tokio::test]
    async fn vehicle_arrives_then_order_ready_duplicate() {
        let test_config = RocksDbQueryConfig::new();
        curbside_pickup::vehicle_arrives_then_order_ready_duplicate(&test_config).await;
    }
}

mod incident_alert {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn incident_alert() {
        let test_config = RocksDbQueryConfig::new();
        incident_alert::incident_alert(&test_config).await;
    }
}

mod min_value {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn min_value() {
        let test_config = RocksDbQueryConfig::new();
        min_value::min_value(&test_config).await;
    }
}

mod overdue_invoice {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn overdue_invoice() {
        let test_config = RocksDbQueryConfig::new();
        overdue_invoice::overdue_invoice(&test_config).await;
    }

    #[tokio::test]
    pub async fn overdue_count_persistent() {
        let test_config = RocksDbQueryConfig::new();
        overdue_invoice::overdue_count_persistent(&test_config).await;
    }
}

mod sensor_heartbeat {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn not_reported() {
        let test_config = RocksDbQueryConfig::new();
        sensor_heartbeat::not_reported(&test_config).await;
    }

    #[tokio::test]
    pub async fn percent_not_reported() {
        let test_config = RocksDbQueryConfig::new();
        sensor_heartbeat::percent_not_reported(&test_config).await;
    }
}

mod temporal_retrieval {
    use super::RocksDbQueryConfig;
    use shared_tests::temporal_retrieval::get_version_by_timestamp;
    use shared_tests::temporal_retrieval::get_versions_by_timerange;

    #[tokio::test]
    async fn get_version_by_timestamp() {
        let test_config = RocksDbQueryConfig::new();
        get_version_by_timestamp::get_version_by_timestamp(&test_config).await;
    }

    #[tokio::test]
    async fn get_versions_by_range() {
        let test_config = RocksDbQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange(&test_config).await;
    }

    #[tokio::test]
    async fn get_versions_by_range_with_initial_value() {
        let test_config = RocksDbQueryConfig::new();
        get_versions_by_timerange::get_versions_by_timerange_with_initial_value_flag(&test_config)
            .await;
    }
}

mod greater_than_a_threshold {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn greater_than_a_threshold() {
        let test_config = RocksDbQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold(&test_config).await;
    }

    #[tokio::test]
    pub async fn greater_than_a_threshold_by_customer() {
        let test_config = RocksDbQueryConfig::new();
        greater_than_a_threshold::greater_than_a_threshold_by_customer(&test_config).await;
    }
}

mod linear_regression {
    use super::RocksDbQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn linear_gradient() {
        let test_config = RocksDbQueryConfig::new();
        linear_regression::linear_gradient(&test_config).await;
    }
}

mod index {
    use super::RocksDbQueryConfig;
    use drasi_core::interface::FutureQueue;
    use uuid::Uuid;

    #[tokio::test]
    async fn future_queue_push_always() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_always(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_not_exists() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_not_exists(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_overwrite() {
        let test_config = RocksDbQueryConfig::new();
        let fqi = test_config.build_future_queue(format!("test-{}", Uuid::new_v4()).as_str());
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_overwrite(&fqi).await;
    }
}
