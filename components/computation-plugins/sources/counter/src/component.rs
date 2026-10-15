// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_plugin_sdk::{
    ControlMessage, ControlNotification, ControlSender, NativeControlHandler,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComputationComponent, ContextEntry, ContextValue, EnvelopeSource,
    GraphChangeCodec, OutputEnvelope, PortId, StreamId,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

fn count_default() -> u64 {
    10
}
fn step_default() -> i64 {
    1
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: StreamId,
    #[serde(default = "count_default")]
    count: u64,
    #[serde(default)]
    start: i64,
    #[serde(default = "step_default")]
    step: i64,
    #[serde(default)]
    interval_ms: u64,
    #[serde(default)]
    paused: bool,
}

pub(super) struct Counter {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    paused: watch::Receiver<bool>,
    emitted: u64,
    running: bool,
}
pub(super) struct CounterControl(watch::Sender<bool>);
impl Counter {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
    ) -> anyhow::Result<(Self, CounterControl)> {
        let configuration: Configuration = serde_json::from_value(configuration)?;
        let last = i128::from(configuration.start)
            + i128::from(configuration.step) * i128::from(configuration.count.saturating_sub(1));
        anyhow::ensure!(
            (i128::from(i64::MIN)..=i128::from(i64::MAX)).contains(&last),
            "counter range overflows i64"
        );
        anyhow::ensure!(
            std::time::Instant::now()
                .checked_add(Duration::from_millis(configuration.interval_ms))
                .is_some(),
            "counter interval is not representable"
        );
        let (sender, paused) = watch::channel(configuration.paused);
        Ok((
            Self {
                descriptor,
                configuration,
                paused,
                emitted: 0,
                running: false,
            },
            CounterControl(sender),
        ))
    }
}
#[async_trait]
impl ComputationComponent for Counter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.configuration)?)
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.running, "counter already started");
        self.running = true;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Counter {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::ensure!(self.running, "counter is not running");
        if self.emitted == self.configuration.count {
            return Ok(None);
        }
        while *self.paused.borrow_and_update() {
            self.paused.changed().await?;
        }
        if self.configuration.interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.configuration.interval_ms)).await;
        }
        let value = i64::try_from(
            i128::from(self.configuration.start)
                + i128::from(self.configuration.step) * i128::from(self.emitted),
        )?;
        let sequence = self
            .emitted
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("counter sequence exhausted"))?;
        let mut properties = ElementPropertyMap::new();
        properties.insert("value", ElementValue::Integer(value));
        let mut envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(
                            self.descriptor.id().as_str(),
                            &sequence.to_string(),
                        ),
                        labels: Arc::from([Arc::from("Counter")]),
                        effective_from: sequence,
                    },
                    properties,
                },
            },
            self.configuration.stream.clone(),
            sequence,
            None,
        )?;
        envelope.append_annotation(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "drasi.standard.volatile-counter",
            ContextValue::Bool(true),
        )?)?;
        self.emitted = sequence;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}
#[async_trait]
impl NativeControlHandler for CounterControl {
    async fn on_message(
        &self,
        message: ControlMessage,
        _control: ControlSender,
    ) -> anyhow::Result<()> {
        match message.notification {
            ControlNotification::Custom { kind, payload } => {
                anyhow::ensure!(payload.is_null(), "counter control payload must be null");
                match kind.as_str() {
                    "drasi.counter.pause" => self.0.send_replace(true),
                    "drasi.counter.resume" => self.0.send_replace(false),
                    _ => anyhow::bail!("unsupported counter control notification {kind}"),
                };
            }
            // Peer status is advisory; it does not change explicit counter pause.
            ControlNotification::Ready
            | ControlNotification::NotReady
            | ControlNotification::Available
            | ControlNotification::Unavailable { .. } => {}
        }
        Ok(())
    }
}
