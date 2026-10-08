// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only provider shared by C-boundary tests and the separately built fixture.

use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_plugin_sdk as sdk;
use drasi_core::{
    interface::StorageDurability,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::{watch, Mutex};
use tokio_stream::Stream;

pub struct Factory {
    pub progress: bool,
}
impl sdk::BootstrapFactory for Factory {
    fn metadata(&self) -> sdk::BootstrapFactoryMetadata {
        sdk::BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new(
                if self.progress {
                    "fixture/bootstrap-progress"
                } else {
                    "fixture/bootstrap"
                },
                "1",
            )
            .expect("constant bootstrap fixture identity"),
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: BTreeMap::from([
                    (
                        "testSecret".into(),
                        sdk::ConfigField {
                            value_type: sdk::ConfigType::String,
                            required: false,
                            secret: true,
                        },
                    ),
                    (
                        "durability".into(),
                        sdk::ConfigField {
                            value_type: sdk::ConfigType::Object,
                            required: false,
                            secret: false,
                        },
                    ),
                    (
                        "mode".into(),
                        sdk::ConfigField {
                            value_type: sdk::ConfigType::String,
                            required: false,
                            secret: false,
                        },
                    ),
                    (
                        "count".into(),
                        sdk::ConfigField {
                            value_type: sdk::ConfigType::Integer,
                            required: false,
                            secret: false,
                        },
                    ),
                    (
                        "cleanupDelayMs".into(),
                        sdk::ConfigField {
                            value_type: sdk::ConfigType::Integer,
                            required: false,
                            secret: false,
                        },
                    ),
                ]),
                allow_additional: false,
            },
            source_progress: self.progress,
        }
    }
    fn create(
        &self,
        request: &sdk::CreateRequest,
        progress: Option<sdk::NativeSourceProgress>,
    ) -> anyhow::Result<Arc<dyn ComputationBootstrapProvider>> {
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        anyhow::ensure!(config.count <= 100, "fixture count exceeds limit");
        anyhow::ensure!(
            config
                .test_secret
                .as_deref()
                .is_none_or(|value| value == "fixture-only"),
            "fixture secret was not resolved"
        );
        let invalid_reader = AtomicBool::new(config.mode == "forge-reader");
        Ok(Arc::new(Provider {
            config,
            progress,
            worker: Mutex::new(None),
            invalid_reader,
            pending: AtomicBool::new(true),
        }))
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Configuration {
    test_secret: Option<String>,
    mode: String,
    count: u64,
    cleanup_delay_ms: u64,
    durability: StorageDurability,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            test_secret: None,
            mode: "normal".into(),
            count: 3,
            cleanup_delay_ms: 0,
            durability: StorageDurability::LOCAL_POWER_LOSS,
        }
    }
}
struct Worker {
    cancel: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}
struct Provider {
    config: Configuration,
    progress: Option<sdk::NativeSourceProgress>,
    worker: Mutex<Option<Worker>>,
    invalid_reader: AtomicBool,
    pending: AtomicBool,
}
#[async_trait]
impl ComputationBootstrapProvider for Provider {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.progress.as_ref().map(|progress| {
            let reader = progress.reader();
            if self.invalid_reader.load(Ordering::Acquire) {
                SourceProgressReader::Local(Arc::new(
                    QuerySourceProgress::new(reader.graph_id(), reader.component_id().clone())
                        .expect("validated progress identity"),
                ))
            } else {
                reader
            }
        })
    }
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        if self.config.mode == "change-reader" {
            self.invalid_reader.store(true, Ordering::Release);
        }
        if let Some(progress) = &self.progress {
            progress.reader().snapshot()?;
        }
        Ok(match self.config.mode.as_str() {
            "reset" => BootstrapPreparation::ResetRequired,
            "refresh" => BootstrapPreparation::RefreshVolatile,
            _ => BootstrapPreparation::Ready,
        })
    }
    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        anyhow::ensure!(
            state.durability() == self.config.durability,
            "durability was not preserved"
        );
        let previous = state.read().await?;
        if previous.as_deref() == Some(b"complete".as_slice()) {
            self.pending.store(false, Ordering::Release);
            return self.prepare().await;
        }
        let bytes = if self.config.mode == "oversize" {
            Bytes::from(vec![1; MAX_BOOTSTRAP_STATE_BYTES + 1])
        } else if self.config.mode == "empty" {
            Bytes::new()
        } else {
            Bytes::from_static(b"initializing")
        };
        if self.config.mode == "ignore-error" {
            let _ = state.write(bytes).await;
        } else {
            state.write(bytes).await?;
            anyhow::ensure!(
                state.read().await?.as_deref() == Some(b"initializing".as_slice()),
                "initialization write not visible"
            );
        }
        if self.config.mode == "require-replay" {
            anyhow::ensure!(
                previous.as_deref() == Some(b"initializing".as_slice()),
                "initialization intent was lost"
            );
        }
        self.prepare().await
    }
    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        Ok(self.pending.load(Ordering::Acquire) && self.config.mode != "no-snapshot")
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let mut worker = self.worker.lock().await;
        anyhow::ensure!(worker.is_none(), "snapshot worker not joined");
        let (cancel, mut cancellation) = watch::channel(false);
        let cleanup_delay = self.config.cleanup_delay_ms;
        let join = tokio::spawn(async move {
            while !*cancellation.borrow_and_update() {
                if cancellation.changed().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(cleanup_delay)).await;
        });
        *worker = Some(Worker {
            cancel: cancel.clone(),
            join,
        });
        let count = self.config.count;
        let mode = self.config.mode.clone();
        let stream = async_stream::try_stream! {
            if mode == "pending" { std::future::pending::<()>().await; }
            if mode == "panic" { panic!("fixture bootstrap stream panic"); }
            for sequence in 1..=count {
                tokio::time::sleep(Duration::from_millis(1)).await;
                if mode == "stream-error" { Err(anyhow::anyhow!("fixture bootstrap stream failure"))?; }
                yield GraphChangeCodec::encode_change(
                    SourceChange::Insert { element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("fixture", &sequence.to_string()),
                            labels: Arc::from([Arc::from("Item")]),
                            effective_from: sequence,
                        },
                        properties: ElementPropertyMap::from(json!({"value": sequence})),
                    }},
                    StreamId::try_new("fixture/out")?, sequence, None,
                )?;
            }
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(CancelStream {
                inner: Box::pin(stream),
                cancel,
            }),
            watermarks: vec![BootstrapWatermark {
                stream: StreamId::try_new("fixture/out")?,
                source_id: None,
                sequence: count,
                position: Some(Bytes::from_static(&[0, 255, 128])),
            }],
        })
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        anyhow::ensure!(
            state.read().await?.as_deref() == Some(b"initializing".as_slice()),
            "snapshot lost initialization state"
        );
        self.snapshot().await
    }
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        Ok(vec![BootstrapWatermark {
            stream: StreamId::try_new("fixture/other")?,
            source_id: Some("source".into()),
            sequence: u64::MAX,
            position: Some(Bytes::from_static(&[255, 0])),
        }])
    }
    fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(Some(if self.config.mode == "bad-completion" {
            Bytes::from(vec![1; MAX_BOOTSTRAP_STATE_BYTES + 1])
        } else {
            Bytes::from_static(b"complete")
        }))
    }
    async fn stop(&self) -> anyhow::Result<()> {
        let mut worker = self.worker.lock().await;
        if let Some(worker) = worker.as_mut() {
            worker.cancel.send_replace(true);
            (&mut worker.join).await?;
        }
        *worker = None;
        Ok(())
    }
}
struct CancelStream {
    inner: Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEnvelope>> + Send>>,
    cancel: watch::Sender<bool>,
}
impl Stream for CancelStream {
    type Item = anyhow::Result<ChangeEnvelope>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}
impl Drop for CancelStream {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}
