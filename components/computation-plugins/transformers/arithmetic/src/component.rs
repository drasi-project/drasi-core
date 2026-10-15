// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_computation_plugin_sdk::{NativeTransactionContext, TransactionalComponent};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::computation::v1::{
    ChangeEnvelope, ComponentDescriptor, ComputationComponent, GraphChangeCodec, InputEnvelope,
    OutputEnvelope, PortId, Schema, StreamId, Transformer,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn value_field() -> String {
    "value".into()
}
fn counter_field() -> String {
    "batch_count".into()
}
fn one() -> i64 {
    1
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: StreamId,
    #[serde(default = "value_field")]
    field: String,
    #[serde(default)]
    add: i64,
    #[serde(default = "one")]
    multiply: i64,
    #[serde(default = "counter_field")]
    counter_property: String,
}

pub(super) struct Arithmetic {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    sequence: u64,
    running: bool,
}
impl Arithmetic {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
    ) -> anyhow::Result<Self> {
        let configuration: Configuration = serde_json::from_value(configuration)?;
        anyhow::ensure!(
            !configuration.field.is_empty()
                && !configuration.counter_property.is_empty()
                && configuration.field != configuration.counter_property,
            "arithmetic fields must be nonempty and distinct"
        );
        Ok(Self {
            descriptor,
            configuration,
            sequence: 0,
            running: false,
        })
    }
    fn calculate(&self, element: &mut Element, batches: i64) -> anyhow::Result<i64> {
        let properties = match element {
            Element::Node { properties, .. } | Element::Relation { properties, .. } => properties,
        };
        let Some(ElementValue::Integer(value)) = properties.get(&self.configuration.field) else {
            anyhow::bail!(
                "arithmetic property {} must be an integer",
                self.configuration.field
            );
        };
        let value = value
            .checked_add(self.configuration.add)
            .and_then(|value| value.checked_mul(self.configuration.multiply))
            .ok_or_else(|| anyhow::anyhow!("arithmetic result overflows i64"))?;
        properties.insert(&self.configuration.field, ElementValue::Integer(value));
        properties.insert(
            &self.configuration.counter_property,
            ElementValue::Integer(batches),
        );
        Ok(value)
    }
}
#[async_trait]
impl ComputationComponent for Arithmetic {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.configuration)?)
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.running, "arithmetic transformer already started");
        self.running = true;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        Ok(())
    }
}
#[async_trait]
impl Transformer for Arithmetic {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.running && input.port.as_str() == "in",
            "arithmetic requires a running input port"
        );
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("arithmetic sequence exhausted"))?;
        let batches = i64::try_from(sequence)?;
        let mut changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        for change in &mut changes {
            match change {
                SourceChange::Insert { element } | SourceChange::Update { element } => {
                    self.calculate(element, batches)?;
                }
                SourceChange::Delete { .. } => {}
                SourceChange::Future { .. } => {
                    anyhow::bail!("arithmetic does not process query scheduling control")
                }
            }
        }
        let envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            self.configuration.stream.clone(),
            sequence,
        )?;
        self.sequence = sequence;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }])
    }
}
#[async_trait]
impl TransactionalComponent for Arithmetic {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        GraphChangeCodec::schema()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &NativeTransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope> {
        anyhow::ensure!(
            !self.running,
            "transaction participant must not be independently activated"
        );
        let previous = match context.get("batches").await? {
            None => 0,
            Some(ElementValue::Integer(value)) if value >= 0 => value,
            _ => anyhow::bail!("invalid persisted arithmetic batch counter"),
        };
        let batches = previous
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("arithmetic batch counter exhausted"))?;
        context
            .put("batches", ElementValue::Integer(batches))
            .await?;
        let mut changes = GraphChangeCodec::decode_changes(&input)?;
        let mut last_value = None;
        for change in &mut changes {
            match change {
                SourceChange::Insert { element } => {
                    context.put_element(element).await?;
                    last_value = Some(self.calculate(element, batches)?);
                }
                SourceChange::Update { element } => {
                    if let Some(previous) = context.get_element(element.get_reference()).await? {
                        anyhow::ensure!(
                            std::mem::discriminant(element)
                                == std::mem::discriminant(previous.as_ref()),
                            "arithmetic update changed the element type"
                        );
                        element.merge_missing_properties(previous.as_ref());
                    }
                    context.put_element(element).await?;
                    last_value = Some(self.calculate(element, batches)?);
                }
                SourceChange::Delete { metadata } => {
                    context.remove_element(&metadata.reference).await?;
                }
                SourceChange::Future { .. } => {
                    anyhow::bail!("arithmetic does not process query scheduling control")
                }
            }
        }
        match last_value {
            Some(value) => {
                context
                    .put("last_value", ElementValue::Integer(value))
                    .await?
            }
            None => context.remove("last_value").await?,
        }
        let output = GraphChangeCodec::encode_changes(
            &changes,
            self.configuration.stream.clone(),
            input.system().sequence(),
            input.system().timestamp(),
        )?;
        context.derive(&input, output.changes().clone()).await
    }
}
