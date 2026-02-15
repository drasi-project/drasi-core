// Copyright 2026 The Drasi Authors.
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

//! Test utilities for Qdrant-based testing using testcontainers
//!
//! This module provides helper functions for testing components that require
//! a Qdrant vector database, using testcontainers to provide a real Qdrant
//! server environment.

use anyhow::Result;
use qdrant_client::qdrant::{SearchPointsBuilder, SearchResponse};
use qdrant_client::Qdrant;
use std::sync::Arc;
use std::time::Duration;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// Qdrant container configuration
#[derive(Debug, Clone)]
pub struct QdrantConfig {
    pub host: String,
    pub grpc_port: u16,
    pub http_port: u16,
}

impl QdrantConfig {
    /// Get the gRPC endpoint URL
    pub fn grpc_endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.grpc_port)
    }

    /// Get the HTTP endpoint URL
    pub fn http_endpoint(&self) -> String {
        format!("http://{}:{}", self.host, self.http_port)
    }
}

/// Setup a Qdrant testcontainer and return a guard that manages cleanup
///
/// Returns a `QdrantGuard` that manages the container lifecycle. The container
/// will be stopped and removed via blocking cleanup when the guard is dropped.
///
/// **RECOMMENDED**: Call `.cleanup().await` explicitly before the test ends for
/// the most reliable cleanup.
///
/// # Example
/// ```ignore
/// let qdrant = setup_qdrant().await;
/// // Use qdrant.config() to get connection details
/// // ... test code ...
/// qdrant.cleanup().await; // Explicit cleanup (recommended)
/// Ok(())
/// ```
pub async fn setup_qdrant() -> QdrantGuard {
    QdrantGuard::new().await
}

/// Low-level setup function that returns raw container and config
#[allow(clippy::unwrap_used)]
async fn setup_qdrant_raw() -> (ContainerAsync<GenericImage>, QdrantConfig) {
    // Qdrant ports
    let grpc_port = 6334;
    let http_port = 6333;

    // Create Qdrant container using generic image
    let container = GenericImage::new("qdrant/qdrant", "latest")
        .with_exposed_port(ContainerPort::Tcp(grpc_port))
        .with_exposed_port(ContainerPort::Tcp(http_port))
        .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
        .start()
        .await
        .unwrap();

    let mapped_grpc_port = container.get_host_port_ipv4(grpc_port).await.unwrap();
    let mapped_http_port = container.get_host_port_ipv4(http_port).await.unwrap();

    let config = QdrantConfig {
        host: "localhost".to_string(), // DevSkim: ignore DS137138
        grpc_port: mapped_grpc_port,
        http_port: mapped_http_port,
    };

    // Give Qdrant a moment to fully initialize
    tokio::time::sleep(Duration::from_secs(2)).await;

    (container, config)
}

/// Guard wrapper for Qdrant container that ensures proper cleanup
///
/// This struct wraps the Qdrant container and uses blocking cleanup in Drop.
#[derive(Clone)]
pub struct QdrantGuard {
    inner: Arc<QdrantGuardInner>,
}

struct QdrantGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    config: QdrantConfig,
}

impl QdrantGuard {
    /// Create a new Qdrant container with guaranteed cleanup
    pub async fn new() -> Self {
        let (container, config) = setup_qdrant_raw().await;
        Self {
            inner: Arc::new(QdrantGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        }
    }

    /// Get the Qdrant configuration
    pub fn config(&self) -> &QdrantConfig {
        &self.inner.config
    }

    /// Get a Qdrant client connection
    pub fn get_client(&self) -> Result<Qdrant> {
        let client = Qdrant::from_url(&self.config().grpc_endpoint()).build()?;
        Ok(client)
    }

    /// Explicitly stop and remove the container
    ///
    /// Call this at the end of your test to ensure the container is cleaned up.
    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        if let Some(container) = container_to_stop {
            let container_id = container.id().to_string();

            match container.stop().await {
                Ok(_) => {
                    log::debug!("Successfully stopped Qdrant container: {container_id}");
                }
                Err(e) => {
                    log::warn!("Error stopping container {container_id}: {e}");
                }
            }

            drop(container);
        }
    }
}

impl Drop for QdrantGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                let container_id = container.id().to_string();

                let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        handle.block_on(async move {
                            let _ = container.stop().await;
                            drop(container);
                        });
                    } else {
                        drop(container);
                    }
                }));

                if cleanup_result.is_ok() {
                    log::debug!("Qdrant container {container_id} cleaned up in Drop");
                } else {
                    log::warn!("Failed to cleanup Qdrant container {container_id} in Drop");
                }
            }
        }
    }
}

/// Search for points in a Qdrant collection
///
/// # Arguments
/// * `client` - Qdrant client
/// * `collection_name` - Name of the collection to search
/// * `vector` - Query vector
/// * `limit` - Maximum number of results
///
/// # Returns
/// Search response with matching points
pub async fn search_points(
    client: &Qdrant,
    collection_name: &str,
    vector: Vec<f32>,
    limit: u64,
) -> Result<SearchResponse> {
    let response = client
        .search_points(SearchPointsBuilder::new(collection_name, vector, limit).with_payload(true))
        .await?;

    Ok(response)
}

/// Get collection info from Qdrant
///
/// # Arguments
/// * `client` - Qdrant client
/// * `collection_name` - Name of the collection
///
/// # Returns
/// Collection info response
pub async fn get_collection_info(
    client: &Qdrant,
    collection_name: &str,
) -> Result<qdrant_client::qdrant::GetCollectionInfoResponse> {
    let info = client.collection_info(collection_name).await?;
    Ok(info)
}

/// Get the count of points in a collection
///
/// # Arguments
/// * `client` - Qdrant client
/// * `collection_name` - Name of the collection
///
/// # Returns
/// Number of points in the collection
pub async fn get_points_count(client: &Qdrant, collection_name: &str) -> Result<u64> {
    let info = client.collection_info(collection_name).await?;
    Ok(info
        .result
        .map(|r| r.points_count.unwrap_or(0))
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qdrant_client::qdrant::{CreateCollectionBuilder, Distance, VectorParamsBuilder};

    #[tokio::test]
    async fn test_setup_qdrant() {
        let qdrant = setup_qdrant().await;
        let config = qdrant.config();

        assert_eq!(config.host, "localhost"); // DevSkim: ignore DS137138
        assert!(config.grpc_port > 0);
        assert!(config.http_port > 0);

        // Verify we can connect
        let client = qdrant.get_client().expect("Should create client");

        // List collections (should be empty)
        let collections = client
            .list_collections()
            .await
            .expect("Should list collections");
        assert!(collections.collections.is_empty());

        qdrant.cleanup().await;
    }

    #[tokio::test]
    async fn test_create_collection() {
        let qdrant = setup_qdrant().await;
        let client = qdrant.get_client().expect("Should create client");

        // Create a test collection
        let collection_name = "test_collection";
        client
            .create_collection(
                CreateCollectionBuilder::new(collection_name)
                    .vectors_config(VectorParamsBuilder::new(128, Distance::Cosine)),
            )
            .await
            .expect("Should create collection");

        // Verify collection exists
        let exists = client
            .collection_exists(collection_name)
            .await
            .expect("Should check existence");
        assert!(exists);

        // Verify collection info
        let info = get_collection_info(&client, collection_name)
            .await
            .expect("Should get collection info");
        assert!(info.result.is_some());

        qdrant.cleanup().await;
    }
}
