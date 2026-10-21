// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only producers using consumer-progress-v1, shared by static and cdylib tests.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::*;
use drasi_computation_plugin_sdk as sdk;
use drasi_core::{
    interface::StorageDurability,
    models::{Element, ElementReference, ElementValue, SourceChange},
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

pub fn factories() -> Vec<Arc<dyn sdk::Factory>> {
    vec![
        Arc::new(Factory(ComponentRole::Source, false)),
        Arc::new(Factory(ComponentRole::Transformer, false)),
        Arc::new(SinkFactory(SinkCompletion::Handled)),
        Arc::new(SinkFactory(SinkCompletion::Accepted)),
    ]
}

pub struct Factory(pub ComponentRole, pub bool);

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        let directions = match self.0 {
            ComponentRole::Source => vec![PortDirection::Output],
            ComponentRole::Transformer => vec![PortDirection::Input, PortDirection::Output],
            ComponentRole::Sink => vec![PortDirection::Input],
            _ => vec![],
        };
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new(
                format!("fixture/consumer-progress-{:?}", self.0).to_lowercase(),
                "1",
            )
            .expect("fixed consumer-progress fixture identity"),
            role: self.0,
            configuration_version: 1,
            configuration: sdk::ConfigSchema {
                fields: [(
                    "mode".into(),
                    sdk::ConfigField {
                        value_type: sdk::ConfigType::String,
                        required: true,
                        secret: false,
                    },
                )]
                .into(),
                allow_additional: false,
            },
            ports: directions
                .into_iter()
                .map(|direction| {
                    PortDescriptor::new(
                        PortId::try_new(if direction == PortDirection::Input {
                            "in"
                        } else {
                            "out"
                        })
                        .expect("fixed fixture port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: (self.0 == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            capabilities: sdk::Capabilities {
                control: true,
                ..Default::default()
            },
        }
    }
    fn supports_source_progress(&self) -> bool {
        self.1
    }
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(
        &self,
        _: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        anyhow::bail!("fixture requires consumer_progress")
    }
    fn create_with_consumer_progress(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        progress: sdk::NativeConsumerProgress,
    ) -> Result<sdk::CreatedComponent> {
        let mode = request.configuration["mode"]
            .as_str()
            .context("missing fixture mode")?
            .to_owned();
        anyhow::ensure!(
            matches!(
                mode.as_str(),
                "valid"
                    | "retained"
                    | "ignored"
                    | "substituted"
                    | "dual"
                    | "mutate-reader"
                    | "mutate-retention"
                    | "start-fail-once"
                    | "start-pending-once"
                    | "stop-fail-once"
                    | "stop-pending-once"
                    | "wait-recovered"
                    | "positions"
                    | "mutate-policy"
                    | "unstable-policy"
                    | "fanout"
            ),
            "unknown fixture mode"
        );
        let commands = Arc::new(Commands {
            reader: progress.reader(),
            updates: tokio::sync::Mutex::new(None),
            starts: AtomicU64::new(0),
            stops: AtomicU64::new(0),
            transforms: AtomicU64::new(0),
            completions: AtomicU64::new(0),
        });
        let policy = matches!(
            mode.as_str(),
            "positions" | "mutate-policy" | "unstable-policy"
        )
        .then(position_policy)
        .transpose()?;
        let producer = Producer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            mode,
            commands: commands.clone(),
            started: false,
            starts: 0,
            stops: 0,
            policy,
        };
        let component = match self.0 {
            ComponentRole::Source => sdk::Component::Source(Box::new(producer)),
            ComponentRole::Transformer => sdk::Component::Transformer(Box::new(producer)),
            _ => anyhow::bail!("fixture requires a producer role"),
        };
        Ok(sdk::CreatedComponent {
            component,
            control_handler: Some(commands),
        })
    }
}

struct Alternate(ConsumerProgressView);
impl ConsumerProgressProvider for Alternate {
    fn graph_id(&self) -> &str {
        self.0.graph_id()
    }
    fn producer_id(&self) -> &ComponentId {
        self.0.producer_id()
    }
    fn snapshot(&self) -> Result<ConsumerProgressSnapshot> {
        self.0.snapshot()
    }
    fn subscribe(&self) -> Result<Box<dyn ConsumerProgressUpdates>> {
        self.0.subscribe()
    }
}

struct SingleOwner(ComponentId);
impl SourceProgressProvider for SingleOwner {
    fn graph_id(&self) -> &str {
        "native-consumer-progress"
    }
    fn component_id(&self) -> &ComponentId {
        &self.0
    }
    fn snapshot(&self) -> Result<Arc<SourceProgressSnapshot>> {
        Ok(Arc::new(SourceProgressSnapshot::default()))
    }
    fn subscribe(&self) -> Result<Box<dyn SourceProgressUpdates>> {
        anyhow::bail!("unbound single-owner fixture")
    }
}

struct Producer {
    descriptor: ComponentDescriptor,
    mode: String,
    commands: Arc<Commands>,
    started: bool,
    starts: usize,
    stops: usize,
    policy: Option<Arc<InputPositionPolicy>>,
}
#[async_trait]
impl sdk::NativeComponent for Producer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"mode": self.mode}))
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        match self.mode.as_str() {
            "ignored" => None,
            "substituted" => Some(ConsumerProgressView::new(Arc::new(Alternate(
                self.commands.reader.clone(),
            )))),
            "mutate-reader" if self.started => Some(ConsumerProgressView::new(Arc::new(
                Alternate(self.commands.reader.clone()),
            ))),
            _ => Some(self.commands.reader.clone()),
        }
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        (self.mode == "retained" || (self.mode == "mutate-retention" && !self.started))
            .then_some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.policy.as_ref().map(|policy| {
            if self.mode == "unstable-policy" || (self.mode == "mutate-policy" && self.started) {
                Arc::new(policy.as_ref().clone())
            } else {
                policy.clone()
            }
        })
    }
    async fn start(&mut self) -> Result<()> {
        self.starts += 1;
        self.commands.starts.fetch_add(1, Ordering::SeqCst);
        self.started = true;
        self.commands.reader.snapshot()?;
        match self.mode.as_str() {
            "start-fail-once" if self.starts == 1 => anyhow::bail!("fixture start failed"),
            "start-pending-once" if self.starts == 1 => std::future::pending().await,
            "wait-recovered" => {
                self.commands.reader.wait_recovered().await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    async fn stop(&mut self) -> Result<()> {
        self.stops += 1;
        self.commands.stops.fetch_add(1, Ordering::SeqCst);
        if self.mode.starts_with("stop-") {
            self.commands.reader.snapshot()?;
        }
        match self.mode.as_str() {
            "stop-fail-once" if self.stops == 1 => anyhow::bail!("fixture cleanup failed"),
            "stop-pending-once" if self.stops == 1 => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                self.commands.reader.snapshot()?;
            }
            _ => {}
        }
        self.started = false;
        Ok(())
    }
}
#[async_trait]
impl sdk::NativeSource for Producer {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        (self.mode == "dual").then(|| {
            SourceProgressView::new(Arc::new(SingleOwner(
                ComponentId::try_new("first").expect("fixed fixture consumer"),
            )))
        })
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.commands.reader.snapshot()?;
        if self.mode == "positions" {
            return std::future::pending().await;
        }
        Ok(None)
    }
}
#[async_trait]
impl sdk::NativeTransformer for Producer {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        self.commands.reader.snapshot()?;
        let sequence = self.commands.transforms.fetch_add(1, Ordering::SeqCst) + 1;
        if self.mode == "positions" {
            return std::future::pending().await;
        }
        let envelope = if self.mode == "fanout" {
            GraphChangeCodec::derive_changes(
                &input.envelope,
                &GraphChangeCodec::decode_changes(&input.envelope)?,
                StreamId::try_new(format!("{}/out", self.descriptor.id()))?,
                sequence,
            )?
        } else {
            input.envelope
        };
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }])
    }

    async fn delivery_completed(&mut self, _: &[OutputEnvelope]) -> Result<()> {
        self.commands.completions.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn position_policy() -> Result<Arc<InputPositionPolicy>> {
    Ok(Arc::new(InputPositionPolicy::new(
        SourceProgressKey::Source("source".into()),
        Arc::new(Partitioned),
    )?))
}
struct Partitioned;
impl InputPositionComparator for Partitioned {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        assert_ne!(
            event.first(),
            Some(&255),
            "fixture position comparator panic"
        );
        let [partition, offset] = event else {
            anyhow::bail!("invalid event position")
        };
        let prior = handled
            .get(usize::from(*partition))
            .context("unknown partition")?;
        Ok(offset > prior)
    }
}

pub fn snapshot_json(snapshot: &ConsumerProgressSnapshot) -> Result<serde_json::Value> {
    Ok(serde_json::Value::Object(
        snapshot
            .consumers
            .iter()
            .map(|(id, state)| {
                Ok((
                    id.to_string(),
                    serde_json::json!(sdk::progress::encode_snapshot(state)?),
                ))
            })
            .collect::<Result<_>>()?,
    ))
}

struct Commands {
    reader: ConsumerProgressView,
    updates: tokio::sync::Mutex<Option<Box<dyn ConsumerProgressUpdates>>>,
    starts: AtomicU64,
    stops: AtomicU64,
    transforms: AtomicU64,
    completions: AtomicU64,
}
#[async_trait]
impl sdk::NativeControlHandler for Commands {
    async fn on_message(&self, message: sdk::ControlMessage, _: sdk::ControlSender) -> Result<()> {
        let sdk::ControlNotification::Custom { kind, payload } = message.notification else {
            anyhow::bail!("unsupported fixture command");
        };
        match kind.as_str() {
            "expect-calls" => {
                let actual = serde_json::json!({
                    "starts": self.starts.load(Ordering::SeqCst),
                    "stops": self.stops.load(Ordering::SeqCst),
                    "transforms": self.transforms.load(Ordering::SeqCst),
                    "completions": self.completions.load(Ordering::SeqCst),
                });
                anyhow::ensure!(
                    actual == payload,
                    "native fixture calls differ: {actual} != {payload}"
                );
            }
            "expect" => {
                anyhow::ensure!(
                    snapshot_json(&self.reader.snapshot()?)? == payload,
                    "consumer snapshots differ"
                );
            }
            "snapshot" => {
                self.reader.snapshot()?;
            }
            "subscribe" => {
                let mut updates = self.reader.subscribe()?;
                updates.snapshot()?;
                *self.updates.lock().await = Some(updates);
            }
            "wait" => {
                self.updates
                    .lock()
                    .await
                    .as_mut()
                    .context("no subscription")?
                    .changed()
                    .await?;
            }
            "old-snapshot" => {
                self.updates
                    .lock()
                    .await
                    .as_mut()
                    .context("no subscription")?
                    .snapshot()?;
            }
            _ => anyhow::bail!("unknown fixture command"),
        }
        Ok(())
    }
}

pub struct SinkFactory(pub SinkCompletion);

impl sdk::Factory for SinkFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        let mut metadata = sdk::Factory::metadata(&Factory(ComponentRole::Sink, false));
        metadata.implementation = ImplementationIdentity::try_new(
            match self.0 {
                SinkCompletion::Handled => "fixture/input-progress-handled",
                SinkCompletion::Accepted => "fixture/input-progress-accepted",
            },
            "1",
        )
        .expect("fixture sink identity");
        metadata.completion = Some(self.0);
        metadata
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> Result<sdk::CreatedComponent> {
        anyhow::ensure!(
            request.configuration["mode"] == "fanout",
            "unexpected sink fixture mode"
        );
        let state = Arc::new(SinkState::default());
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Sink(Box::new(HandledSink {
                descriptor: sdk::Factory::metadata(self).descriptor(request.id.clone())?,
                completion: self.0,
                state: state.clone(),
            })),
            control_handler: Some(state),
        })
    }
}

#[derive(Default)]
struct SinkCalls {
    starts: u64,
    stops: u64,
    effects: Vec<serde_json::Value>,
}
#[derive(Default)]
struct SinkState(Mutex<SinkCalls>);
impl SinkState {
    fn calls(&self) -> Result<std::sync::MutexGuard<'_, SinkCalls>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("fixture sink state poisoned"))
    }
}
#[async_trait]
impl sdk::NativeControlHandler for SinkState {
    async fn on_message(&self, message: sdk::ControlMessage, _: sdk::ControlSender) -> Result<()> {
        let sdk::ControlNotification::Custom { kind, payload } = message.notification else {
            anyhow::bail!("unsupported sink fixture command");
        };
        anyhow::ensure!(kind == "expect-handled", "unknown sink fixture command");
        let calls = self.calls()?;
        let actual = serde_json::json!({
            "starts": calls.starts, "stops": calls.stops, "effects": calls.effects,
        });
        anyhow::ensure!(
            actual == payload,
            "native sink effects differ: {actual} != {payload}"
        );
        Ok(())
    }
}

struct HandledSink {
    descriptor: ComponentDescriptor,
    completion: SinkCompletion,
    state: Arc<SinkState>,
}
#[async_trait]
impl sdk::NativeComponent for HandledSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"mode":"fanout"}))
    }
    async fn start(&mut self) -> Result<()> {
        self.state.calls()?.starts += 1;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.state.calls()?.stops += 1;
        Ok(())
    }
}
#[async_trait]
impl sdk::NativeSink for HandledSink {
    fn completion(&self) -> SinkCompletion {
        self.completion
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        let (operation, metadata, value) = match changes.as_slice() {
            [SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            }]
            | [SourceChange::Update {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            }] => {
                let Some(ElementValue::Integer(value)) = properties.get("value") else {
                    anyhow::bail!("missing integer sink value")
                };
                (
                    if matches!(changes[0], SourceChange::Insert { .. }) {
                        "insert"
                    } else {
                        "update"
                    },
                    metadata,
                    Some(*value),
                )
            }
            [SourceChange::Delete { metadata }] => ("delete", metadata, None),
            _ => anyhow::bail!("unexpected sink graph input"),
        };
        anyhow::ensure!(metadata.reference == ElementReference::new("input", "one"));
        let mut calls = self.state.calls()?;
        anyhow::ensure!(calls.effects.len() < 3, "fixture effect bound exceeded");
        let sequence = calls.effects.len() as u64 + 1;
        anyhow::ensure!(
            input.envelope.system().sequence() == sequence && metadata.effective_from == sequence
        );
        calls.effects.push(serde_json::json!({
            "operation": operation, "value": value, "sequence": sequence,
        }));
        Ok(())
    }
}
