use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;
use testcontainers::core::ContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::ImageExt;
use testcontainers::{ContainerAsync, GenericImage};

#[derive(Clone)]
pub struct LokiGuard {
    inner: Arc<LokiGuardInner>,
}

struct LokiGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    endpoint: String,
}

pub async fn setup_loki() -> Result<LokiGuard> {
    let image = GenericImage::new("grafana/loki", "3.4.3")
        .with_exposed_port(ContainerPort::Tcp(3100))
        .with_cmd(["-config.file=/etc/loki/local-config.yaml"]);

    let container = image.start().await?;
    let port = container.get_host_port_ipv4(3100).await?;
    let endpoint = format!("http://localhost:{port}"); // DevSkim: ignore DS137138

    wait_for_loki_ready(&endpoint, Duration::from_secs(20)).await?;

    Ok(LokiGuard {
        inner: Arc::new(LokiGuardInner {
            container: std::sync::Mutex::new(Some(container)),
            endpoint,
        }),
    })
}

impl LokiGuard {
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
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
        }
    }
}

impl Drop for LokiGuardInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.container.lock() {
            if let Some(container) = guard.take() {
                drop(container);
            }
        }
    }
}

pub async fn wait_for_loki_ready(endpoint: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow::anyhow!("timed out waiting for Loki readiness"));
        }

        let url = format!("{endpoint}/ready");
        if let Ok(response) = client.get(url).send().await {
            if response.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

pub async fn query_loki_lines(endpoint: &str, query: &str) -> Result<Vec<String>> {
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{endpoint}/loki/api/v1/query_range"))
        .query(&[("query", query), ("limit", "1000")])
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "query failed with status {}",
            response.status()
        ));
    }

    let body: Value = response.json().await?;
    let mut lines = Vec::new();

    if let Some(streams) = body
        .get("data")
        .and_then(|data| data.get("result"))
        .and_then(|result| result.as_array())
    {
        for stream in streams {
            if let Some(values) = stream.get("values").and_then(|values| values.as_array()) {
                for pair in values {
                    if let Some(line) = pair.get(1).and_then(|line| line.as_str()) {
                        lines.push(line.to_string());
                    }
                }
            }
        }
    }

    Ok(lines)
}

pub async fn wait_for_line_contains(
    endpoint: &str,
    query: &str,
    expected_fragment: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            return Err(anyhow::anyhow!(
                "timed out waiting for fragment '{expected_fragment}'"
            ));
        }

        let lines = query_loki_lines(endpoint, query).await?;
        if lines.iter().any(|line| line.contains(expected_fragment)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
