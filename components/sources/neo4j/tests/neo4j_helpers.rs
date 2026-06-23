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

use anyhow::{anyhow, Result};
use neo4rs::{query, ConfigBuilder, Graph};
use std::sync::Arc;
use std::time::Duration;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

#[derive(Debug, Clone)]
pub struct Neo4jConfig {
    pub host: String,
    pub bolt_port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl Neo4jConfig {
    pub fn bolt_uri(&self) -> String {
        format!("{}:{}", self.host, self.bolt_port)
    }
}

#[derive(Clone)]
pub struct Neo4jGuard {
    inner: Arc<Neo4jGuardInner>,
}

struct Neo4jGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    config: Neo4jConfig,
}

impl Neo4jGuard {
    pub async fn new() -> Result<Self> {
        let (container, config) = setup_neo4j_raw().await?;
        Ok(Self {
            inner: Arc::new(Neo4jGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        })
    }

    pub fn config(&self) -> &Neo4jConfig {
        &self.inner.config
    }

    pub async fn get_graph(&self) -> Result<Graph> {
        connect_graph(self.config()).await
    }

    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut guard) = self.inner.container.lock() {
                guard.take()
            } else {
                None
            }
        };

        if let Some(container) = container_to_stop {
            let _ = container.stop().await;
            drop(container);
        }
    }
}

impl Drop for Neo4jGuardInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.container.lock() {
            if let Some(container) = guard.take() {
                drop(container);
            }
        }
    }
}

pub async fn setup_neo4j() -> Result<Neo4jGuard> {
    Neo4jGuard::new().await
}

async fn setup_neo4j_raw() -> Result<(ContainerAsync<GenericImage>, Neo4jConfig)> {
    use testcontainers::core::ContainerPort;
    use testcontainers::runners::AsyncRunner;

    let image = GenericImage::new("neo4j", "5-enterprise")
        .with_exposed_port(ContainerPort::Tcp(7687))
        .with_exposed_port(ContainerPort::Tcp(7474))
        .with_env_var("NEO4J_ACCEPT_LICENSE_AGREEMENT", "yes")
        .with_env_var("NEO4J_AUTH", "neo4j/testpassword");

    let container = image
        .start()
        .await
        .map_err(|e| anyhow!("Failed to start Neo4j container: {e}"))?;
    let bolt_port = container
        .get_host_port_ipv4(7687)
        .await
        .map_err(|e| anyhow!("Failed to map Neo4j bolt port: {e}"))?;

    let config = Neo4jConfig {
        host: "localhost".to_string(), // DevSkim: ignore DS137138
        bolt_port,
        user: "neo4j".to_string(),
        password: "testpassword".to_string(),
        database: "neo4j".to_string(),
    };

    wait_for_cdc_ready(&config).await?;
    Ok((container, config))
}

pub async fn connect_graph(config: &Neo4jConfig) -> Result<Graph> {
    let neo4j_config = ConfigBuilder::default()
        .uri(config.bolt_uri())
        .user(config.user.as_str())
        .password(config.password.as_str())
        .db(config.database.as_str())
        .build()?;
    Ok(Graph::connect(neo4j_config).await?)
}

pub async fn wait_for_cdc_ready(config: &Neo4jConfig) -> Result<()> {
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..60 {
        match connect_graph(config).await {
            Ok(graph) => {
                let enable_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    graph.run(query(
                        "ALTER DATABASE neo4j SET OPTION txLogEnrichment 'FULL'",
                    )),
                )
                .await;
                if let Err(e) = enable_result {
                    last_error = Some(anyhow!("Timed out enabling CDC: {e}"));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                if let Ok(Err(e)) = enable_result {
                    last_error = Some(anyhow!("Failed enabling CDC: {e}"));
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    graph.execute(query("CALL db.cdc.current() YIELD id RETURN id")),
                )
                .await;
                match result {
                    Ok(Ok(_)) => return Ok(()),
                    Ok(Err(e)) => {
                        last_error = Some(anyhow!("CDC not ready yet: {e}"));
                    }
                    Err(e) => last_error = Some(anyhow!("Timed out waiting for CDC query: {e}")),
                }
            }
            Err(e) => {
                last_error = Some(anyhow!("Neo4j not ready yet: {e}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow!(
        "Neo4j CDC did not become ready within timeout: {last_error:?}"
    ))
}

pub async fn reset_graph(graph: &Graph) -> Result<()> {
    graph.run(query("MATCH (n) DETACH DELETE n")).await?;
    Ok(())
}
