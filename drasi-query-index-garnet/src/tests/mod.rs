use std::{env, sync::Arc};

use async_trait::async_trait;
use drasi_query_ast::ast::Query;
use drasi_query_core::{
    index_cache::{
        cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
    },
    path_solver::match_path::MatchPath,
    query::QueryBuilder,
};
use shared_tests::QueryTestConfig;
use uuid::Uuid;

use crate::{
    element_index::GarnetElementIndex, future_queue::GarnetFutureQueue,
    result_index::GarnetResultIndex,
};

struct GarnetQueryConfig {
    url: String,
    use_cache: bool,
}

impl GarnetQueryConfig {
    pub fn new(use_cache: bool) -> Self {
        let url = match env::var("REDIS_URL") {
            Ok(url) => url,
            Err(_) => "redis://127.0.0.1:6379".to_string(),
        };

        GarnetQueryConfig { url, use_cache }
    }

    pub async fn build_future_queue(&self, query_id: &str) -> GarnetFutureQueue {
        GarnetFutureQueue::connect(query_id, &self.url)
            .await
            .unwrap()
    }
}

#[async_trait]
impl QueryTestConfig for GarnetQueryConfig {
    async fn config_query(&self, builder: QueryBuilder, query: Arc<Query>) -> QueryBuilder {
        log::info!("using in Garnet indexes");
        let mp = MatchPath::from_query(&query.phases[0]).unwrap();
        let query_id = format!("test-{}", Uuid::new_v4().to_string());

        let mut element_index =
            GarnetElementIndex::connect(&query_id, &self.url, &mp, builder.get_joins())
                .await
                .unwrap();
        let ari = GarnetResultIndex::connect(&query_id, &self.url)
            .await
            .unwrap();

        let fq = GarnetFutureQueue::connect(&query_id, &self.url)
            .await
            .unwrap();

        element_index.enable_archive();
        let element_index = Arc::new(element_index);
        let archive_index = element_index.clone();

        if self.use_cache {
            let element_index = Arc::new(CachedElementIndex::new(element_index, 3).unwrap());
            let ari = CachedResultIndex::new(Arc::new(ari), 3).unwrap();

            builder
                .with_element_index(element_index)
                .with_archive_index(archive_index)
                .with_result_index(Arc::new(ari))
                .with_future_queue(Arc::new(fq))
        } else {
            builder
                .with_element_index(element_index)
                .with_archive_index(archive_index)
                .with_result_index(Arc::new(ari))
                .with_future_queue(Arc::new(fq))
        }
    }
}

mod building_comfort {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn building_comfort_use_case() {
        let test_config = GarnetQueryConfig::new(false);
        building_comfort::building_comfort_use_case(&test_config).await;
    }

    #[tokio::test]
    async fn building_comfort_use_case_with_cache() {
        let test_config = GarnetQueryConfig::new(true);
        building_comfort::building_comfort_use_case(&test_config).await;
    }
}

mod curbside_pickup {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives() {
        let test_config = GarnetQueryConfig::new(false);
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready() {
        let test_config = GarnetQueryConfig::new(false);
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready_duplicate() {
        let test_config = GarnetQueryConfig::new(false);
        curbside_pickup::vehicle_arrives_then_order_ready_duplicate(&test_config).await;
    }

    #[tokio::test]
    async fn order_ready_then_vehicle_arrives_with_cache() {
        let test_config = GarnetQueryConfig::new(true);
        curbside_pickup::order_ready_then_vehicle_arrives(&test_config).await;
    }

    #[tokio::test]
    #[ignore]
    async fn vehicle_arrives_then_order_ready_with_cache() {
        let test_config = GarnetQueryConfig::new(true);
        curbside_pickup::vehicle_arrives_then_order_ready(&test_config).await;
    }
}

mod incident_alert {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn incident_alert() {
        let test_config = GarnetQueryConfig::new(false);
        incident_alert::incident_alert(&test_config).await;
    }

    #[tokio::test]
    pub async fn incident_alert_with_cache() {
        let test_config = GarnetQueryConfig::new(true);
        incident_alert::incident_alert(&test_config).await;
    }
}

mod min_value {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn min_value() {
        let test_config = GarnetQueryConfig::new(false);
        min_value::min_value(&test_config).await;
    }

    #[tokio::test]
    pub async fn min_value_with_cache() {
        let test_config = GarnetQueryConfig::new(true);
        min_value::min_value(&test_config).await;
    }
}

mod overdue_invoice {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn overdue_invoice() {
        let test_config = GarnetQueryConfig::new(false);
        overdue_invoice::overdue_invoice(&test_config).await;
    }

    #[tokio::test]
    pub async fn overdue_count_persistent() {
        let test_config = GarnetQueryConfig::new(false);
        overdue_invoice::overdue_count_persistent(&test_config).await;
    }
}

mod sensor_heartbeat {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn not_reported() {
        let test_config = GarnetQueryConfig::new(false);
        sensor_heartbeat::not_reported(&test_config).await;
    }

    #[tokio::test]
    pub async fn percent_not_reported() {
        let test_config = GarnetQueryConfig::new(false);
        sensor_heartbeat::percent_not_reported(&test_config).await;
    }
}

mod temporal_retrieval {
    use super::GarnetQueryConfig;
    use shared_tests::temporal_retrieval::get_version_by_timestamp;
    use shared_tests::temporal_retrieval::get_versions_by_timerange;

    #[tokio::test]
    async fn get_version_by_timestamp() {
        let test_config = GarnetQueryConfig::new(false);
        get_version_by_timestamp::get_version_by_timestamp(&test_config).await;
    }

    #[tokio::test]
    async fn get_versions_by_range() {
        let test_config = GarnetQueryConfig::new(false);
        get_versions_by_timerange::get_versions_by_timerange(&test_config).await;
    }

    #[tokio::test]
    async fn get_versions_by_range_with_initial_value() {
        let test_config = GarnetQueryConfig::new(false);
        get_versions_by_timerange::get_versions_by_timerange_with_initial_value_flag(&test_config)
            .await;
    }
}

mod greater_than_a_threshold {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn greater_than_a_threshold() {
        let test_config = GarnetQueryConfig::new(false);
        greater_than_a_threshold::greater_than_a_threshold(&test_config).await;
    }

    #[tokio::test]
    pub async fn greater_than_a_threshold_by_customer() {
        let test_config = GarnetQueryConfig::new(false);
        greater_than_a_threshold::greater_than_a_threshold_by_customer(&test_config).await;
    }
}

mod steps_happen_in_any_order {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    pub async fn steps_happen_in_any_order() {
        let test_config = GarnetQueryConfig::new(false);
        steps_happen_in_any_order::steps_happen_in_any_order(&test_config).await;
    }
}

mod linear_regression {
    use super::GarnetQueryConfig;
    use shared_tests::use_cases::*;

    #[tokio::test]
    async fn linear_gradient() {
        let test_config = GarnetQueryConfig::new(false);
        linear_regression::linear_gradient(&test_config).await;
    }
}

mod index {
    use super::GarnetQueryConfig;
    use drasi_query_core::interface::FutureQueue;
    use uuid::Uuid;

    #[tokio::test]
    async fn future_queue_push_always() {
        let test_config = GarnetQueryConfig::new(false);
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_always(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_not_exists() {
        let test_config = GarnetQueryConfig::new(false);
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_not_exists(&fqi).await;
    }

    #[tokio::test]
    async fn future_queue_push_overwrite() {
        let test_config = GarnetQueryConfig::new(false);
        let fqi = test_config
            .build_future_queue(format!("test-{}", Uuid::new_v4()).as_str())
            .await;
        fqi.clear().await.unwrap();
        shared_tests::index::future_queue::push_overwrite(&fqi).await;
    }
}
