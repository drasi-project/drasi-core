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

//! Test utilities for Redis-based platform integration tests
//!
//! This module provides helper functions for testing platform sources and reactions
//! using testcontainers to provide a real Redis server environment.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use testcontainers::ImageExt;
use testcontainers_modules::redis::Redis;

/// Setup a Redis testcontainer and return a guard that manages cleanup
///
/// Returns a `RedisGuard` that manages the container lifecycle. The container
/// will be stopped and removed via blocking cleanup when the guard is dropped.
///
/// **RECOMMENDED**: Call `.cleanup().await` explicitly before the test ends for
/// the most reliable cleanup.
///
/// # Example
/// ```ignore
/// let redis = setup_redis().await;
/// // Use redis.url() to get the connection string
/// // ... test code ...
/// redis.cleanup().await; // Explicit cleanup (recommended)
/// Ok(())
/// ```
pub async fn setup_redis() -> RedisGuard {
    RedisGuard::new().await
}

/// Low-level setup function that returns raw container and URL
///
/// Internal use only. Prefer using `setup_redis()` which returns a `RedisGuard`
/// for automatic cleanup.
#[allow(clippy::unwrap_used)]
async fn setup_redis_raw() -> (testcontainers::ContainerAsync<Redis>, String) {
    use testcontainers::runners::AsyncRunner;

    let attribution = redis_attribution::from_env().expect("invalid Redis fixture attribution");
    let observer = redis_attribution::observer_from_env(attribution.as_ref())
        .expect("invalid Redis Observer admission");

    // Docker Desktop has a known bug where `PublishAllPorts: true` sporadically
    // fails to map exposed ports, returning an empty binding array even though the
    // container is running and the port is listed in ExposedPorts. Work around this
    // by retrying the entire container creation when port mapping fails.
    for attempt in 0..5u32 {
        let mut image = Redis::default().with_tag("7-alpine");
        if let Some(owner) = &attribution {
            image = image
                .with_container_name(owner.container_name(attempt + 1))
                .with_labels(owner.labels(attempt + 1));
        }
        let container = if let Some(owner) = &attribution {
            owner
                .before_creation(attempt + 1, observer.as_ref(), || async {
                    Ok(image.start().await?)
                })
                .await
                .expect("persist and admit Redis fixture creation intent before starting")
        } else {
            image.start().await.unwrap()
        };
        if let Some(owner) = &attribution {
            owner
                .record_started(attempt + 1, container.id())
                .expect("persist Redis fixture full container identity before port lookup");
        }

        match container.get_host_port_ipv4(6379).await {
            Ok(redis_port) => {
                let redis_url = format!("redis://127.0.0.1:{redis_port}");
                return (container, redis_url);
            }
            Err(e) => {
                log::warn!(
                    "Redis container port mapping failed (attempt {}/5): {e}. Retrying...",
                    attempt + 1
                );
                let _ = container.stop().await;
                drop(container);
            }
        }
    }
    panic!("Failed to start Redis container with mapped port after 5 attempts");
}

mod redis_attribution {
    use crate::fixture_attribution::{Attribution, Fixture, ObserverAdmission};
    use anyhow::Result;
    use std::ffi::OsString;

    const OWNER_ENV: &str = "DRASI_REDIS_TEST_OWNER_TOKEN";
    const DIRECTORY_ENV: &str = "DRASI_REDIS_TEST_RECEIPT_DIR";
    const ACK_DIRECTORY_ENV: &str = "DRASI_REDIS_TEST_ACK_DIR";
    const ACK_TIMEOUT_ENV: &str = "DRASI_REDIS_TEST_ACK_TIMEOUT_MS";
    const ACK_EXPECTATIONS_ENV: &str = "DRASI_REDIS_TEST_ACK_EXPECTATIONS_SHA256";
    const SOURCE: &str = "shared-tests/src/redis_helpers.rs";

    pub(super) fn from_env() -> Result<Option<Attribution>> {
        from_settings(std::env::var_os(OWNER_ENV), std::env::var_os(DIRECTORY_ENV))
    }

    pub(super) fn observer_from_env(
        owner: Option<&Attribution>,
    ) -> Result<Option<ObserverAdmission>> {
        ObserverAdmission::from_settings(
            owner,
            std::env::var_os(ACK_DIRECTORY_ENV),
            std::env::var_os(ACK_TIMEOUT_ENV),
            std::env::var_os(ACK_EXPECTATIONS_ENV),
        )
    }

    fn from_settings(
        owner: Option<OsString>,
        directory: Option<OsString>,
    ) -> Result<Option<Attribution>> {
        Attribution::from_settings(
            Fixture::new("redis", "redis:7-alpine", SOURCE, 5),
            (OWNER_ENV, DIRECTORY_ENV),
            owner,
            directory,
        )
    }

    #[cfg(all(test, unix))]
    mod tests {
        use super::*;
        use crate::fixture_attribution::directory_identity;
        use anyhow::Context;
        use serde_json::{json, Value};
        use std::os::unix::{
            ffi::OsStringExt,
            fs::{symlink, MetadataExt, PermissionsExt},
        };
        use std::{
            fs,
            path::{Path, PathBuf},
        };
        use testcontainers::ImageExt;
        use testcontainers_modules::redis::Redis;
        use uuid::Uuid;

        struct ReceiptDirectory(PathBuf);

        impl ReceiptDirectory {
            fn new() -> Result<Self> {
                let path = std::env::temp_dir()
                    .canonicalize()?
                    .join(format!("drasi-redis-receipts-{}", Uuid::new_v4()));
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                Ok(Self(path))
            }

            fn attribution(&self) -> Result<Attribution> {
                from_settings(
                    Some(Uuid::new_v4().to_string().into()),
                    Some(self.0.clone().into_os_string()),
                )?
                .context("enabled Redis attribution")
            }
        }

        impl Drop for ReceiptDirectory {
            fn drop(&mut self) {
                if self.0.exists() {
                    fs::remove_dir_all(&self.0)
                        .expect("remove this test's private receipt directory");
                }
            }
        }

        #[test]
        fn attribution_is_disabled_only_when_both_settings_are_absent() -> Result<()> {
            assert!(from_settings(None, None)?.is_none());
            assert!(from_settings(Some(Uuid::new_v4().to_string().into()), None).is_err());
            assert!(from_settings(None, Some("/missing".into())).is_err());
            Ok(())
        }

        #[test]
        fn attribution_rejects_invalid_settings() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            for token in [
                "",
                "not-a-uuid",
                "00000000-0000-0000-0000-000000000000",
                "AA4B0192-1F0F-5BB8-A458-820438EE7BEA",
            ] {
                assert!(from_settings(
                    Some(token.into()),
                    Some(directory.0.clone().into_os_string())
                )
                .is_err());
            }
            assert!(from_settings(
                Some(OsString::from_vec(vec![0xff])),
                Some(directory.0.clone().into_os_string())
            )
            .is_err());
            assert!(from_settings(
                Some(Uuid::new_v4().to_string().into()),
                Some(OsString::from_vec(vec![0xff]))
            )
            .is_err());
            assert!(directory_identity(Path::new("relative")).is_err());
            assert!(directory_identity(&directory.0.join("missing")).is_err());
            assert!(directory_identity(&directory.0.join("../")).is_err());
            fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755))?;
            assert!(directory.attribution().is_err());
            Ok(())
        }

        #[test]
        fn attribution_rejects_symlink_directory_traversal() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            let target = ReceiptDirectory::new()?;
            let link = directory.0.join("link");
            symlink(&target.0, &link)?;
            assert!(directory_identity(&link).is_err());
            assert!(directory_identity(&link.join("missing")).is_err());
            Ok(())
        }

        #[test]
        fn attribution_names_and_labels_distinguish_fixtures_and_attempts() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            let first = directory.attribution()?;
            let second = from_settings(
                Some(first.owner_token.clone().into()),
                Some(directory.0.clone().into_os_string()),
            )?
            .context("second fixture")?;
            assert_ne!(first.container_name(1), first.container_name(2));
            assert_ne!(first.container_name(1), second.container_name(1));
            assert_eq!(first.labels(1)["drasi.test.owner"], first.owner_token);
            assert_ne!(
                first.labels(1)["drasi.test.fixture"],
                second.labels(1)["drasi.test.fixture"]
            );
            assert_eq!(first.labels(2)["drasi.test.attempt"], "2");
            let image = Redis::default()
                .with_tag("7-alpine")
                .with_container_name(first.container_name(1))
                .with_labels(first.labels(1));
            assert_eq!(image.container_name(), &Some(first.container_name(1)));
            for (key, value) in first.labels(1) {
                assert_eq!(image.labels().get(key), Some(&value));
            }
            Ok(())
        }

        #[test]
        fn intent_and_started_receipts_share_exact_immutable_identity() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            let owner = directory.attribution()?;
            owner.record_intent(1)?;
            owner.record_started(1, &"a".repeat(64))?;
            let intent_path = directory
                .0
                .join(format!("{}-a1.intent.json", owner.fixture_id));
            let started_path = directory
                .0
                .join(format!("{}-a1.started.json", owner.fixture_id));
            let intent: Value = serde_json::from_slice(&fs::read(&intent_path)?)?;
            let mut started: Value = serde_json::from_slice(&fs::read(&started_path)?)?;
            assert_eq!(started["container_id"], "a".repeat(64));
            assert_eq!(started["phase"], "started");
            started
                .as_object_mut()
                .context("started receipt")?
                .remove("container_id");
            started["phase"] = json!("creation_intent");
            assert_eq!(started, intent);
            assert_eq!(intent["actor_pid"], std::process::id());
            assert_eq!(intent["schema_version"], 1);
            assert_eq!(intent["image"], "redis:7-alpine");
            assert_eq!(intent["phase"], "creation_intent");
            assert_eq!(intent["test_name"], json!(std::thread::current().name()));
            assert_eq!(intent["fixture_source"], SOURCE);
            assert_eq!(
                intent["executable"],
                json!(std::env::current_exe()?.canonicalize()?)
            );
            assert_eq!(intent["container_name"], owner.container_name(1));
            assert_eq!(intent["labels"], json!(owner.labels(1)));
            for path in [intent_path, started_path] {
                let metadata = fs::metadata(path)?;
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
                assert_eq!(metadata.uid(), fs::metadata(&directory.0)?.uid());
            }
            Ok(())
        }

        #[test]
        fn receipts_refuse_overwrite_and_noncanonical_container_ids() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            let owner = directory.attribution()?;
            owner.record_intent(1)?;
            let path = directory
                .0
                .join(format!("{}-a1.intent.json", owner.fixture_id));
            let original = fs::read(&path)?;
            assert!(owner.record_intent(1).is_err());
            assert_eq!(fs::read(&path)?, original);
            for id in ["short".to_owned(), "A".repeat(64), "g".repeat(64)] {
                assert!(owner.record_started(1, &id).is_err());
            }
            assert_eq!(fs::read_dir(&directory.0)?.count(), 1);
            owner.record_started(1, &"a".repeat(64))?;
            assert!(owner.record_started(1, &"b".repeat(64)).is_err());
            assert_eq!(fs::read(&path)?, original);
            Ok(())
        }

        #[test]
        fn receipt_storage_failure_is_explicit_before_creation_intent() -> Result<()> {
            let directory = ReceiptDirectory::new()?;
            let owner = directory.attribution()?;
            fs::remove_dir(&directory.0)?;
            assert!(owner.record_intent(1).is_err());
            assert!(!directory.0.exists());
            Ok(())
        }

        #[tokio::test]
        async fn observer_immediate_exit_child() -> Result<()> {
            let owner = from_env()?;
            let observer = observer_from_env(owner.as_ref())?;
            let calls = std::cell::Cell::new(0);
            if let Some(observer) = &observer {
                let owner = owner.as_ref().context("opted-in child attribution")?;
                owner
                    .before_creation(1, Some(observer), || async {
                        calls.set(calls.get() + 1);
                        println!(
                            "{}",
                            json!({
                                "event": "observer-admitted-immediate-exit-child",
                                "mode": "genuine-observer-opt-in",
                                "actor_pid": std::process::id(),
                                "owner_token": owner.owner_token,
                                "fixture_id": owner.fixture_id,
                            })
                        );
                        Ok(())
                    })
                    .await?;
                assert_eq!(calls.get(), 1);
                assert_eq!(
                    fs::read_dir(
                        std::env::var_os(DIRECTORY_ENV).context("child receipt directory")?
                    )?
                    .count(),
                    1,
                    "the immediate-exit child creates only its durable intent, never a container"
                );
            } else {
                let directory = ReceiptDirectory::new()?;
                let owner = directory.attribution()?;
                owner
                    .before_creation(1, None, || async {
                        calls.set(calls.get() + 1);
                        Ok(())
                    })
                    .await?;
                assert_eq!(calls.get(), 1);
                println!(
                    "{}",
                    json!({
                        "event": "observer-admission-default-off",
                        "mode": "default-off-only-not-handshake-evidence",
                    })
                );
            }
            Ok(())
        }
    }
}

/// Guard wrapper for Redis container that ensures proper cleanup
///
/// This struct wraps the Redis container and uses blocking cleanup in Drop.
/// The testcontainers library's async drop may not complete before tests exit,
/// but we force a blocking cleanup using a runtime handle.
///
/// The container is wrapped in an Arc to allow cloning for multiple test accessors.
#[derive(Clone)]
pub struct RedisGuard {
    inner: Arc<RedisGuardInner>,
}

struct RedisGuardInner {
    container: std::sync::Mutex<Option<testcontainers::ContainerAsync<Redis>>>,
    url: String,
}

impl RedisGuard {
    /// Create a new Redis container with guaranteed cleanup
    pub async fn new() -> Self {
        let (container, url) = setup_redis_raw().await;
        Self {
            inner: Arc::new(RedisGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                url,
            }),
        }
    }

    /// Get the Redis connection URL
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Explicitly stop and remove the container
    ///
    /// Call this at the end of your test to ensure the container is cleaned up.
    /// This is an async method that properly stops and removes the container.
    pub async fn cleanup(self) {
        // Take the container out while holding the lock, then drop the lock before awaiting
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        // Now await without holding the lock
        if let Some(container) = container_to_stop {
            let container_id = container.id().to_string();

            // Stop and remove the container
            match container.stop().await {
                Ok(_) => {
                    log::debug!("Successfully stopped Redis container: {container_id}");
                }
                Err(e) => {
                    log::warn!("Error stopping container {container_id}: {e}");
                }
            }

            // Explicit drop to trigger removal
            drop(container);
        }
    }
}

impl Drop for RedisGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                let container_id = container.id().to_string();

                // Block on cleanup to ensure it completes
                let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        // We're inside a runtime — use spawn instead of block_on
                        // to avoid "Cannot start a runtime from within a runtime" panic.
                        handle.spawn(async move {
                            let _ = container.stop().await;
                            drop(container);
                        });
                    } else {
                        // We're not in a runtime, just drop it
                        drop(container);
                    }
                }));

                if cleanup_result.is_ok() {
                    log::debug!("Redis container {container_id} cleaned up in Drop");
                } else {
                    log::warn!("Failed to cleanup Redis container {container_id} in Drop");
                }
            }
        }
    }
}

// /// Publish a platform CloudEvent to a Redis stream
// ///
// /// # Arguments
// /// * `redis_url` - Redis connection URL
// /// * `stream_key` - Stream key to publish to
// /// * `cloud_event` - CloudEvent JSON value to publish
// ///
// /// # Returns
// /// The stream ID of the published event
#[allow(dead_code)]
pub async fn publish_platform_event(
    redis_url: &str,
    stream_key: &str,
    cloud_event: Value,
) -> Result<String> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Serialize CloudEvent to JSON string
    let event_json = serde_json::to_string(&cloud_event)?;

    // Publish to Redis stream with "data" field
    let stream_id: String = redis::cmd("XADD")
        .arg(stream_key)
        .arg("*") // Auto-generate stream ID
        .arg("data")
        .arg(event_json)
        .query_async(&mut conn)
        .await?;

    Ok(stream_id)
}

/// Read events from a Redis stream
///
/// # Arguments
/// * `redis_url` - Redis connection URL
/// * `stream_key` - Stream key to read from
/// * `start_id` - Stream ID to start reading from ("0" for all, ">" for new)
/// * `count` - Maximum number of events to read
///
/// # Returns
/// Vec of (stream_id, event_data) tuples
#[allow(dead_code)]
pub async fn read_from_stream(
    redis_url: &str,
    stream_key: &str,
    start_id: &str,
    count: usize,
) -> Result<Vec<(String, HashMap<String, String>)>> {
    use redis::streams::StreamReadReply;

    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Use XREAD command directly
    let results: StreamReadReply = redis::cmd("XREAD")
        .arg("COUNT")
        .arg(count)
        .arg("STREAMS")
        .arg(stream_key)
        .arg(start_id)
        .query_async(&mut conn)
        .await?;

    let mut events = Vec::new();
    for stream_key in results.keys {
        for stream_id in stream_key.ids {
            let mut data = HashMap::new();
            for (key, value) in stream_id.map {
                // Handle redis::Value - use FromRedisValue trait
                if let Ok(value_str) = redis::from_redis_value::<String>(&value) {
                    data.insert(key, value_str);
                }
            }
            events.push((stream_id.id, data));
        }
    }

    Ok(events)
}

/// Build a platform insert event in CloudEvent format
///
/// # Arguments
/// * `element_id` - Element ID
/// * `labels` - Element labels
/// * `properties` - Element properties
/// * `element_type` - "node" or "rel"
/// * `start_id` - Start node ID (for relations only)
/// * `end_id` - End node ID (for relations only)
pub fn build_platform_insert_event(
    element_id: &str,
    labels: Vec<&str>,
    properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut after = json!({
        "id": element_id,
        "labels": labels,
        "properties": properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        after["startId"] = json!(start_id.unwrap_or(""));
        after["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "i",
            "payload": {
                "after": after,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Build a platform update event in CloudEvent format
pub fn build_platform_update_event(
    element_id: &str,
    labels: Vec<&str>,
    old_properties: HashMap<String, Value>,
    new_properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut after = json!({
        "id": element_id,
        "labels": labels,
        "properties": new_properties,
    });

    let mut before = json!({
        "id": element_id,
        "labels": labels,
        "properties": old_properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        after["startId"] = json!(start_id.unwrap_or(""));
        after["endId"] = json!(end_id.unwrap_or(""));
        before["startId"] = json!(start_id.unwrap_or(""));
        before["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "u",
            "payload": {
                "after": after,
                "before": before,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Build a platform delete event in CloudEvent format
pub fn build_platform_delete_event(
    element_id: &str,
    labels: Vec<&str>,
    properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut before = json!({
        "id": element_id,
        "labels": labels,
        "properties": properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        before["startId"] = json!(start_id.unwrap_or(""));
        before["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "d",
            "payload": {
                "before": before,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Verify CloudEvent structure has all required fields
///
/// # Arguments
/// * `cloud_event` - CloudEvent JSON value to verify
///
/// # Returns
/// Ok if valid, Err with description if invalid
pub fn verify_cloudevent_structure(cloud_event: &Value) -> Result<()> {
    // Required fields
    let required_fields = vec![
        "id",
        "source",
        "specversion",
        "type",
        "datacontenttype",
        "topic",
        "time",
        "data",
    ];

    for field in required_fields {
        if cloud_event.get(field).is_none() {
            return Err(anyhow::anyhow!("Missing required field: {field}"));
        }
    }

    // Verify values
    if cloud_event["specversion"] != "1.0" {
        return Err(anyhow::anyhow!("Invalid specversion, expected '1.0'"));
    }

    if cloud_event["datacontenttype"] != "application/json" {
        return Err(anyhow::anyhow!(
            "Invalid datacontenttype, expected 'application/json'"
        ));
    }

    if cloud_event["type"] != "com.dapr.event.sent" {
        return Err(anyhow::anyhow!(
            "Invalid type, expected 'com.dapr.event.sent'"
        ));
    }

    // Verify ID is a valid UUID v4
    let id = cloud_event["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("id is not a string"))?;
    uuid::Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("id is not a valid UUID"))?;

    // Verify timestamp is ISO 8601
    let time = cloud_event["time"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("time is not a string"))?;
    chrono::DateTime::parse_from_rfc3339(time)
        .map_err(|_| anyhow::anyhow!("time is not valid ISO 8601 format"))?;

    Ok(())
}

/// Get stream length using XLEN command
#[allow(dead_code)]
pub async fn get_stream_length(redis_url: &str, stream_key: &str) -> Result<usize> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let length: usize = redis::cmd("XLEN")
        .arg(stream_key)
        .query_async(&mut conn)
        .await?;

    Ok(length)
}

/// Get pending entries count for a consumer group
#[allow(dead_code)]
pub async fn get_pending_count(
    redis_url: &str,
    stream_key: &str,
    group_name: &str,
) -> Result<usize> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let result: Vec<redis::Value> = redis::cmd("XPENDING")
        .arg(stream_key)
        .arg(group_name)
        .query_async(&mut conn)
        .await?;

    // XPENDING returns [count, min_id, max_id, consumers]
    // We want the count (first element)
    if let Some(redis::Value::Int(count)) = result.first() {
        Ok(*count as usize)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_setup_redis() {
        // Use setup_redis() which returns RedisGuard
        let redis = setup_redis().await;
        assert!(redis.url().starts_with("redis://127.0.0.1:"));

        // Verify we can connect
        let client = redis::Client::open(redis.url()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let _: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();

        // Explicitly cleanup the container
        redis.cleanup().await;
    }

    #[test]
    fn test_build_platform_insert_event_node() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), json!("Alice"));
        props.insert("age".to_string(), json!(30));

        let event = build_platform_insert_event("1", vec!["Person"], props, "node", None, None);

        assert_eq!(event["data"][0]["op"], "i");
        assert_eq!(event["data"][0]["payload"]["after"]["id"], "1");
        assert_eq!(event["data"][0]["payload"]["after"]["labels"][0], "Person");
        assert_eq!(
            event["data"][0]["payload"]["after"]["properties"]["name"],
            "Alice"
        );
        assert_eq!(event["data"][0]["payload"]["source"]["table"], "node");
    }

    #[test]
    fn test_build_platform_insert_event_relation() {
        let mut props = HashMap::new();
        props.insert("since".to_string(), json!("2020"));

        let event =
            build_platform_insert_event("r1", vec!["KNOWS"], props, "rel", Some("1"), Some("2"));

        assert_eq!(event["data"][0]["op"], "i");
        assert_eq!(event["data"][0]["payload"]["after"]["startId"], "1");
        assert_eq!(event["data"][0]["payload"]["after"]["endId"], "2");
    }

    #[test]
    fn test_build_platform_update_event() {
        let mut old_props = HashMap::new();
        old_props.insert("value".to_string(), json!(10));

        let mut new_props = HashMap::new();
        new_props.insert("value".to_string(), json!(20));

        let event = build_platform_update_event(
            "1",
            vec!["Counter"],
            old_props,
            new_props,
            "node",
            None,
            None,
        );

        assert_eq!(event["data"][0]["op"], "u");
        assert_eq!(
            event["data"][0]["payload"]["before"]["properties"]["value"],
            10
        );
        assert_eq!(
            event["data"][0]["payload"]["after"]["properties"]["value"],
            20
        );
    }

    #[test]
    fn test_build_platform_delete_event() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), json!("Bob"));

        let event = build_platform_delete_event("2", vec!["Person"], props, "node", None, None);

        assert_eq!(event["data"][0]["op"], "d");
        assert_eq!(event["data"][0]["payload"]["before"]["id"], "2");
        assert!(event["data"][0]["payload"]["after"].is_null());
    }

    #[test]
    fn test_verify_cloudevent_structure_valid() {
        let cloud_event = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "source": "drasi-core",
            "specversion": "1.0",
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
            "pubsubname": "drasi-pubsub",
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_ok());
    }

    #[test]
    fn test_verify_cloudevent_structure_missing_field() {
        let cloud_event = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "source": "drasi-core",
            // Missing specversion
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_err());
    }

    #[test]
    fn test_verify_cloudevent_structure_invalid_uuid() {
        let cloud_event = json!({
            "id": "not-a-uuid",
            "source": "drasi-core",
            "specversion": "1.0",
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_err());
    }
}
