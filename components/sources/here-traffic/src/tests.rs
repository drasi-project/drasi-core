// Copyright 2025 The Drasi Authors.
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

use super::*;
use crate::mapping::{ChangeKind, RelationSnapshot};
use drasi_core::models::SourceChange;

mod lifecycle {
    use super::*;
    use drasi_lib::context::workers::WorkerCleanupError;
    use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreResult};
    use std::time::Duration;
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct HeldStore {
        inner: MemoryStateStoreProvider,
        entered: Notify,
        release: Notify,
    }

    #[async_trait]
    impl StateStoreProvider for HeldStore {
        async fn get(&self, store: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
            self.inner.get(store, key).await
        }
        async fn set(&self, store: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.set(store, key, value).await
        }
        async fn delete(&self, store: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.delete(store, key).await
        }
        async fn contains_key(&self, store: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.contains_key(store, key).await
        }
        async fn get_many(
            &self,
            store: &str,
            keys: &[&str],
        ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
            self.inner.get_many(store, keys).await
        }
        async fn set_many(&self, store: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
            self.inner.set_many(store, entries).await
        }
        async fn delete_many(&self, store: &str, keys: &[&str]) -> StateStoreResult<usize> {
            self.inner.delete_many(store, keys).await
        }
        async fn clear_store(&self, store: &str) -> StateStoreResult<usize> {
            self.inner.clear_store(store).await
        }
        async fn list_keys(&self, store: &str) -> StateStoreResult<Vec<String>> {
            self.inner.list_keys(store).await
        }
        async fn store_exists(&self, store: &str) -> StateStoreResult<bool> {
            self.inner.store_exists(store).await
        }
        async fn key_count(&self, store: &str) -> StateStoreResult<usize> {
            self.inner.key_count(store).await
        }
    }

    async fn server(requests: u64) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v7/flow"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results":[]})),
            )
            .expect(requests)
            .mount(&server)
            .await;
        server
    }

    fn source(url: &str) -> HereTrafficSourceBuilder {
        HereTrafficSource::builder("lifecycle", "test-key", "52.5,13.3,52.6,13.5")
            .with_base_url(url)
            .with_endpoints(vec![Endpoint::Flow])
            .with_polling_interval(Duration::from_secs(60))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn polling_restarts_reset_shutdown_and_clear_old_dispatchers() -> Result<()> {
        let server = server(3).await;
        let store = Arc::new(MemoryStateStoreProvider::new());
        let source = source(&server.uri())
            .with_state_store(store.clone())
            .build()?;
        source.stop().await?;
        for cycle in 0..3 {
            let mut receiver = source.base.try_test_subscribe().await?;
            source.start().await?;
            source.start().await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while server
                    .received_requests()
                    .await
                    .expect("recorded requests")
                    .len()
                    < cycle + 1
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            source.stop().await?;
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await?
                    .unwrap_err()
                    .to_string(),
                "Channel closed"
            );
            assert!(source.task_handle.read().await.is_none());
            assert_eq!(source.shutdown_tx.receiver_count(), 0);
            assert!(store.get(source.id(), LAST_POLL_KEY).await?.is_some());
        }
        source.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_write_remains_owned_after_cancelled_and_timed_out_stop() -> Result<()> {
        let server = server(1).await;
        let store = Arc::new(HeldStore {
            inner: MemoryStateStoreProvider::new(),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let source = source(&server.uri())
            .with_state_store(store.clone())
            .build()?;
        let mut receiver = source.base.try_test_subscribe().await?;
        source.start().await?;
        tokio::time::timeout(Duration::from_secs(2), store.entered.notified()).await?;
        {
            let stop = source.stop();
            tokio::pin!(stop);
            tokio::select! {
                result = &mut stop => panic!("write is still owned: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(source
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        let error = source.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::TimedOut { timeout }) if *timeout == Duration::from_secs(5))
        );
        assert!(!source
            .task_handle
            .read()
            .await
            .as_ref()
            .expect("retained worker")
            .is_finished());
        assert!(store.inner.get(source.id(), LAST_POLL_KEY).await?.is_none());
        store.release.notify_one();
        source.stop().await?;
        assert!(store.inner.get(source.id(), LAST_POLL_KEY).await?.is_some());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await?
                .unwrap_err()
                .to_string(),
            "Channel closed"
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panic_and_cancelled_base_cleanup_block_restart_until_joined() -> Result<()> {
        let source = source("http://127.0.0.1:1").build()?;
        *source.cleanup_required.lock().await = true;
        spawn_owned_worker(&source.task_handle, async {
            panic!("injected HERE polling panic")
        })
        .await?;
        let error = source.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::Join(error)) if error.is_panic())
        );
        assert!(source.task_handle.read().await.is_none());
        assert!(source
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        let base_cleanup = source.base.shutdown_tx.write().await;
        {
            let stop = source.stop();
            tokio::pin!(stop);
            tokio::select! {
                result = &mut stop => panic!("base cleanup is still pending: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(source
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        drop(base_cleanup);
        source.stop().await?;
        assert_eq!(source.status().await, ComponentStatus::Stopped);
        Ok(())
    }
}

fn sample_segment(jam_factor: f64, speed: f64) -> TrafficSegmentSnapshot {
    TrafficSegmentSnapshot {
        id: "segment_52.50000_13.40000".to_string(),
        road_name: Some("Test Road".to_string()),
        current_speed: Some(speed),
        speed_uncapped: None,
        free_flow_speed: Some(50.0),
        jam_factor: Some(jam_factor),
        confidence: Some(90.0),
        functional_class: Some(2),
        length_meters: Some(1500.0),
        latitude: 52.5,
        longitude: 13.4,
        last_updated: "2024-03-10T12:00:00Z".to_string(),
    }
}

fn sample_incident(id: &str, severity: &str) -> TrafficIncidentSnapshot {
    TrafficIncidentSnapshot {
        id: id.to_string(),
        incident_type: Some("ACCIDENT".to_string()),
        severity: Some(severity.to_string()),
        description: Some("Test incident".to_string()),
        status: "ACTIVE".to_string(),
        start_time: Some("2024-03-10T11:00:00Z".to_string()),
        end_time: None,
        latitude: 52.5005,
        longitude: 13.4005,
    }
}

#[test]
fn test_config_validation_invalid_bbox() {
    let mut config = HereTrafficConfig::new("key", "invalid");
    config.bounding_box = "1,2,3".to_string();
    assert!(config.validate().is_err());
}

#[test]
fn test_detect_flow_change_new_segment() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    let changes = state.update_flow("source", &config, vec![sample_segment(3.0, 45.0)]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Insert { .. }));
}

#[test]
fn test_detect_flow_change_thresholds() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    state.update_flow("source", &config, vec![sample_segment(3.0, 45.0)]);

    let changes = state.update_flow("source", &config, vec![sample_segment(3.3, 47.0)]);
    assert!(
        changes.is_empty(),
        "Changes below threshold should be ignored"
    );

    let changes = state.update_flow("source", &config, vec![sample_segment(3.9, 47.0)]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Update { .. }));
}

#[test]
fn test_detect_incident_resolved() {
    let mut state = state::SourceState::default();
    state
        .incidents
        .insert("INC_1".to_string(), sample_incident("INC_1", "HIGH"));

    let changes = state.update_incidents("source", vec![]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Delete { .. }));
}

#[test]
fn test_relation_generation() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    state.flow_segments.insert(
        "segment_52.50000_13.40000".to_string(),
        sample_segment(3.0, 45.0),
    );
    state
        .incidents
        .insert("INC_1".to_string(), sample_incident("INC_1", "HIGH"));

    let changes = state.update_relations("source", &config);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Insert { .. }));

    // Calling again with the same state should produce no changes (idempotent)
    let changes = state.update_relations("source", &config);
    assert!(
        changes.is_empty(),
        "No relation changes expected when state matches"
    );

    let relation_change = mapping::build_relation_change(
        "source",
        &RelationSnapshot {
            id: "affects_INC_1_segment_52.50000_13.40000".to_string(),
            incident_id: "INC_1".to_string(),
            segment_id: "segment_52.50000_13.40000".to_string(),
            distance_meters: 10.0,
        },
        ChangeKind::Delete,
    );
    assert!(matches!(relation_change, SourceChange::Delete { .. }));
}
