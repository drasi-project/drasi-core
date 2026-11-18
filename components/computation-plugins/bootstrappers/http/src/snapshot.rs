// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    BootstrapLabels, ChangeEnvelope, ComponentId, ComputationBootstrapSnapshot, GraphChangeCodec,
};
use drasi_computation_plugin_sdk::{QuiescentBootstrapProvider, Scope, ServedSource};
use drasi_core_types::models::{Element, SourceChange};
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

/// Default `maxResponseBytes`.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on `maxResponseBytes`.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// Default `outputCapacity`.
pub const DEFAULT_OUTPUT_CAPACITY: usize = 64;
/// Upper bound on `outputCapacity`.
pub const MAX_OUTPUT_CAPACITY: usize = 65_536;
/// Default `maxDurationMs`.
pub const DEFAULT_MAX_DURATION_MS: u64 = 300_000;
/// Default `shutdownTimeoutMs`.
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 5_000;

fn response_bytes() -> usize {
    DEFAULT_MAX_RESPONSE_BYTES
}
fn output_capacity() -> usize {
    DEFAULT_OUTPUT_CAPACITY
}
fn duration_ms() -> u64 {
    DEFAULT_MAX_DURATION_MS
}
fn shutdown_ms() -> u64 {
    DEFAULT_SHUTDOWN_TIMEOUT_MS
}

/// The published configuration: the fields of [`HttpBootstrapConfig`] with
/// their serde defaults and the bounds `validate` enforces. `settings` is
/// the typed `drasi_http_common` bootstrap settings object (camelCase at
/// every level; an unknown key at any level is refused when parsed).
pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    use serde_json::json;
    ConfigDefinition::try_new([
        Field::new("settings", Object, true, "HTTP endpoints to read: {endpoints: [{url, method, headers, body, auth, pagination, response}], timeoutSeconds, maxRetries, retryDelayMs, maxPages}. Every key is camelCase (auth {type: bearer, tokenEnv}, pagination {type: offset-limit, offsetParam, limitParam, pageSize, totalPath}, ...); unknown keys are refused."),
        Field::new("nodeLabels", Array, false, "Upper bound on the node labels read; empty reads every label. Each query's labels narrow it further.").default(json!([])),
        Field::new("relationLabels", Array, false, "Upper bound on the relation labels read; empty reads every label.").default(json!([])),
        Field::new("maxResponseBytes", Integer, false, "Largest response body accepted, in bytes.").default(DEFAULT_MAX_RESPONSE_BYTES as u64).minimum(1).maximum(MAX_RESPONSE_BYTES as u64),
        Field::new("outputCapacity", Integer, false, "Elements buffered between the HTTP reader and the query.").default(DEFAULT_OUTPUT_CAPACITY as u64).minimum(1).maximum(MAX_OUTPUT_CAPACITY as u64),
        Field::new("maxDurationMs", Integer, false, "Deadline for the whole snapshot.").default(DEFAULT_MAX_DURATION_MS).minimum(1),
        Field::new("shutdownTimeoutMs", Integer, false, "Bound on waiting for the reader to stop.").default(DEFAULT_SHUTDOWN_TIMEOUT_MS).minimum(1),
    ])
    .expect("constant HTTP bootstrap configuration")
}

/// The provider's configuration. The served source and stream are not
/// configured: the graph relation (the source whose bootstrap input the
/// provider is connected to) supplies them. `nodeLabels`/`relationLabels`
/// are an upper bound on what the provider reads (empty means every label);
/// each query's declared labels narrow it further.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpBootstrapConfig {
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

impl HttpBootstrapConfig {
    /// Parses and validates a configuration; unknown keys at any level fail.
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value.clone())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        HttpSnapshotReader::validate_bounded_config(&self.settings)?;
        ensure!(
            (1..=MAX_RESPONSE_BYTES).contains(&self.max_response_bytes),
            "maxResponseBytes must be in 1..=33554432"
        );
        ensure!(
            (1..=MAX_OUTPUT_CAPACITY).contains(&self.output_capacity),
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
    served: ServedSource,
    labels: Option<BootstrapLabels>,
    shared: Arc<Shared>,
    pub(crate) query_scope: Option<(Scope, ComponentId)>,
}

impl HttpBootstrapProvider {
    /// A provider emitting `served`'s namespace and stream, selecting only
    /// elements whose labels the query declared (`labels`, `None` = all)
    /// within the configured labels.
    pub fn new(
        config: HttpBootstrapConfig,
        served: ServedSource,
        labels: Option<BootstrapLabels>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            shared: Arc::new(Shared {
                worker: RwLock::new(None),
                run: Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                timeout: Duration::from_millis(config.shutdown_timeout_ms),
            }),
            config,
            served,
            labels,
            query_scope: None,
        })
    }
}

/// Whether the query's declared labels select a mapped change. The configured
/// labels already bounded what the reader produced.
pub(crate) fn requested(labels: Option<&BootstrapLabels>, change: &SourceChange) -> bool {
    let Some(labels) = labels else {
        return true;
    };
    let element = match change {
        SourceChange::Insert { element } | SourceChange::Update { element, .. } => element,
        _ => return true,
    };
    match element {
        Element::Node { metadata, .. } => metadata
            .labels
            .iter()
            .any(|label| labels.requests_node(label)),
        Element::Relation { metadata, .. } => metadata
            .labels
            .iter()
            .any(|label| labels.requests_relation(label)),
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
impl QuiescentBootstrapProvider for HttpBootstrapProvider {
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
        let served = self.served.clone();
        let labels = self.labels.clone();
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
                            source_id: &served.source,
                            node_labels: &config.node_labels,
                            relation_labels: &config.relation_labels,
                        },
                        |change| {
                            let selected = requested(labels.as_ref(), &change);
                            let next = if selected {
                                sequence.checked_add(1)
                            } else {
                                Some(sequence)
                            };
                            if let (true, Some(next)) = (selected, next) {
                                sequence = next;
                            }
                            let stream = served.stream.clone();
                            let outputs = &outputs;
                            async move {
                                if !selected {
                                    return Ok(());
                                }
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
