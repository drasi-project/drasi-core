// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    ChangeEnvelope, ComponentId, ComputationBootstrapSnapshot, GraphChangeCodec, StreamId,
};
use drasi_computation_plugin_sdk::{NativeBootstrapProvider, Scope};
use drasi_http_common::bootstrap::{
    config::HttpBootstrapConfig as RestConfig,
    reader::{HttpSnapshotReader, SnapshotRequest},
};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, RwLock},
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

fn response_bytes() -> usize {
    16 * 1024 * 1024
}
fn output_capacity() -> usize {
    64
}
fn duration_ms() -> u64 {
    300_000
}
fn shutdown_ms() -> u64 {
    5_000
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpBootstrapConfig {
    pub source: ComponentId,
    pub stream: StreamId,
    #[serde(deserialize_with = "deserialize_settings")]
    pub settings: RestConfig,
    #[serde(default)]
    pub node_labels: Vec<String>,
    #[serde(default)]
    pub relation_labels: Vec<String>,
    #[serde(default = "response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "output_capacity")]
    pub output_capacity: usize,
    #[serde(default = "duration_ms")]
    pub max_duration_ms: u64,
    #[serde(default = "shutdown_ms")]
    pub shutdown_timeout_ms: u64,
}

fn deserialize_settings<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<RestConfig, D::Error> {
    fn fields<'a>(
        value: &'a serde_json::Value,
        allowed: &[&str],
    ) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
        let object = value
            .as_object()
            .context("HTTP bootstrap settings must be objects")?;
        for field in object.keys() {
            ensure!(
                allowed.contains(&field.as_str()),
                "unknown HTTP bootstrap field {field}"
            );
        }
        Ok(object)
    }
    fn validate(value: &serde_json::Value) -> Result<()> {
        let settings = fields(
            value,
            &[
                "endpoints",
                "timeoutSeconds",
                "maxRetries",
                "retryDelayMs",
                "maxPages",
            ],
        )?;
        for endpoint in settings
            .get("endpoints")
            .and_then(serde_json::Value::as_array)
            .context("endpoints must be an array")?
        {
            let endpoint = fields(
                endpoint,
                &[
                    "url",
                    "method",
                    "headers",
                    "body",
                    "auth",
                    "pagination",
                    "response",
                ],
            )?;
            if let Some(auth) = endpoint.get("auth").filter(|value| !value.is_null()) {
                let names: &[&str] = match auth.get("type").and_then(serde_json::Value::as_str) {
                    Some("bearer") => &["type", "token_env"],
                    Some("api-key") => &["type", "location", "name", "value_env"],
                    Some("basic") => &["type", "username_env", "password_env"],
                    Some("oauth2-client-credentials") => &[
                        "type",
                        "token_url",
                        "client_id_env",
                        "client_secret_env",
                        "scopes",
                    ],
                    _ => anyhow::bail!("unsupported HTTP bootstrap authentication type"),
                };
                fields(auth, names)?;
            }
            if let Some(pagination) = endpoint.get("pagination").filter(|value| !value.is_null()) {
                let names: &[&str] =
                    match pagination.get("type").and_then(serde_json::Value::as_str) {
                        Some("offset-limit") => &[
                            "type",
                            "offset_param",
                            "limit_param",
                            "page_size",
                            "total_path",
                        ],
                        Some("page-number") => &[
                            "type",
                            "page_param",
                            "page_size_param",
                            "page_size",
                            "total_pages_path",
                        ],
                        Some("cursor") => &[
                            "type",
                            "cursor_param",
                            "cursor_path",
                            "has_more_path",
                            "page_size_param",
                            "page_size",
                        ],
                        Some("link-header") => &["type", "page_size_param", "page_size"],
                        Some("next-url") => &["type", "next_url_path", "base_url"],
                        _ => anyhow::bail!("unsupported HTTP bootstrap pagination type"),
                    };
                fields(pagination, names)?;
            }
            let response = fields(
                endpoint.get("response").context("response is required")?,
                &["itemsPath", "contentType", "mappings"],
            )?;
            for mapping in response
                .get("mappings")
                .and_then(serde_json::Value::as_array)
                .context("mappings must be an array")?
            {
                let mapping = fields(mapping, &["elementType", "operation", "template"])?;
                fields(
                    mapping
                        .get("template")
                        .context("mapping template is required")?,
                    &["id", "labels", "properties", "from", "to"],
                )?;
            }
        }
        Ok(())
    }
    let value = serde_json::Value::deserialize(deserializer)?;
    validate(&value).map_err(serde::de::Error::custom)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

impl HttpBootstrapConfig {
    fn validate(&self) -> Result<()> {
        HttpSnapshotReader::validate_bounded_config(&self.settings)?;
        ensure!(
            (1..=32 * 1024 * 1024).contains(&self.max_response_bytes),
            "maxResponseBytes must be in 1..=33554432"
        );
        ensure!(
            (1..=65_536).contains(&self.output_capacity),
            "outputCapacity must be in 1..=65536"
        );
        ensure!(
            self.settings.max_retries <= 100 && self.settings.max_pages.unwrap_or(10_000) > 0,
            "maxRetries must not exceed 100 and maxPages must be positive"
        );
        for (name, millis) in [
            ("maxDurationMs", self.max_duration_ms),
            ("shutdownTimeoutMs", self.shutdown_timeout_ms),
        ] {
            ensure!(millis > 0, "{name} must be positive");
            Instant::now()
                .checked_add(Duration::from_millis(millis))
                .with_context(|| format!("{name} is not representable"))?;
        }
        Instant::now()
            .checked_add(Duration::from_secs(self.settings.timeout_seconds))
            .context("timeoutSeconds is not representable")?;
        Ok(())
    }
}

struct Shared {
    worker: RwLock<Option<JoinHandle<Result<bool>>>>,
    run: Mutex<Option<CancellationToken>>,
    lifecycle: tokio::sync::Mutex<()>,
    timeout: Duration,
}

impl Shared {
    fn cancel(&self) -> Result<()> {
        if let Some(run) = self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP snapshot ownership poisoned"))?
            .as_ref()
        {
            run.cancel();
        }
        Ok(())
    }

    async fn join(&self) -> Result<bool> {
        let mut worker = self.worker.write().await;
        let result = join_owned_worker_gracefully(&mut worker, self.timeout).await;
        if worker.is_none() {
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("HTTP snapshot ownership poisoned"))?
                .take();
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(false),
            WorkerCompletion::Cancelled => anyhow::bail!("HTTP snapshot worker cancelled"),
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Err(error) = self.cancel() {
            eprintln!("drasi.network HTTP snapshot cleanup failed: {error:#}");
        }
        if let Some(worker) = self.worker.get_mut() {
            eprintln!("drasi.network HTTP snapshot dropped without awaited cleanup");
            worker.abort();
        }
    }
}

pub struct HttpBootstrapProvider {
    config: HttpBootstrapConfig,
    shared: Arc<Shared>,
    pub(crate) query_scope: Option<(Scope, ComponentId)>,
}

impl HttpBootstrapProvider {
    pub fn new(config: HttpBootstrapConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            shared: Arc::new(Shared {
                worker: RwLock::new(None),
                run: Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                timeout: Duration::from_millis(config.shutdown_timeout_ms),
            }),
            config,
            query_scope: None,
        })
    }
}

struct Reader {
    receiver: mpsc::Receiver<ChangeEnvelope>,
    shared: Arc<Shared>,
    cancel: CancellationToken,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[async_trait]
impl NativeBootstrapProvider for HttpBootstrapProvider {
    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> Result<()> {
        if let Some((scope, query)) = &self.query_scope {
            ensure!(
                scope.instance_id == instance_id
                    && scope.graph_id == graph_id
                    && query == component_id,
                "HTTP snapshot belongs to another query"
            );
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        ensure!(
            self.shared.worker.read().await.is_none(),
            "HTTP snapshot requires awaited cleanup before another snapshot"
        );
        let cancel = CancellationToken::new();
        *self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP snapshot ownership poisoned"))? =
            Some(cancel.clone());
        let config = self.config.clone();
        let (outputs, receiver) = mpsc::channel(config.output_capacity);
        let worker_cancel = cancel.clone();
        spawn_owned_worker(&self.shared.worker, async move {
            let work = async {
                let reader = HttpSnapshotReader::bounded(
                    config.settings.clone(),
                    config.max_response_bytes,
                )?;
                let mut sequence = 0u64;
                reader
                    .read(
                        SnapshotRequest {
                            source_id: config.source.as_str(),
                            node_labels: &config.node_labels,
                            relation_labels: &config.relation_labels,
                        },
                        |change| {
                            let next = sequence.checked_add(1);
                            if let Some(next) = next {
                                sequence = next;
                            }
                            let stream = config.stream.clone();
                            let outputs = &outputs;
                            async move {
                                let sequence = next.context("HTTP snapshot sequence exhausted")?;
                                let envelope = GraphChangeCodec::encode_change(
                                    change,
                                    stream,
                                    sequence,
                                    Some(chrono::Utc::now()),
                                )?;
                                outputs.send(envelope).await.context("HTTP snapshot reader closed")?;
                                Ok(())
                            }
                        },
                    )
                    .await?;
                Result::<bool>::Ok(true)
            };
            let result = tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => Ok(false),
                result = tokio::time::timeout(Duration::from_millis(config.max_duration_ms), work) =>
                    result.context("HTTP snapshot exceeded maxDurationMs").and_then(|result| result),
            };
            if let Err(error) = &result {
                eprintln!("drasi.network HTTP snapshot failed: {error:#}");
            }
            result
        })
        .await?;
        let reader = Reader {
            receiver,
            shared: self.shared.clone(),
            cancel,
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    ensure!(!reader.cancel.is_cancelled(), "HTTP snapshot stopped");
                    let change = tokio::select! {
                        biased;
                        _ = reader.cancel.cancelled() => anyhow::bail!("HTTP snapshot stopped"),
                        change = reader.receiver.recv() => change,
                    };
                    if let Some(change) = change {
                        return Ok(Some((change, reader)));
                    }
                    let _lifecycle = reader.shared.lifecycle.lock().await;
                    ensure!(!reader.cancel.is_cancelled(), "HTTP snapshot stopped");
                    ensure!(
                        reader.shared.join().await?,
                        "HTTP snapshot did not complete"
                    );
                    Ok(None)
                },
            )),
            watermarks: Vec::new(),
        })
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        self.shared.cancel()?;
        self.shared.join().await?;
        Ok(())
    }
}
