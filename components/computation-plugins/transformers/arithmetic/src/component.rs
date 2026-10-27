// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{NativeComponent, NativeTransformer};

use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentDescriptor, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, InputEnvelope, OutputEnvelope, PortId, Schema, StreamId,
};
use drasi_computation_plugin_sdk::{NativeTransactionContext, TransactionalComponent};
use drasi_core_types::models::{Element, ElementValue, SourceChange};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

fn value_field() -> String {
    "value".into()
}
fn counter_field() -> String {
    "batch_count".into()
}
fn one() -> i64 {
    1
}

pub(super) fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    ConfigDefinition::try_new([
        Field::new("stream", String, true, "Nonempty output stream identifier."),
        Field::new(
            "field",
            String,
            false,
            "Nonempty integer property to transform; must differ from counter_property.",
        )
        .default(value_field()),
        Field::new(
            "add",
            Integer,
            false,
            "Signed amount added before multiplying; processing rejects arithmetic overflow.",
        )
        .default(0)
        .minimum(i64::MIN)
        .maximum(i64::MAX as u64),
        Field::new(
            "multiply",
            Integer,
            false,
            "Signed multiplier applied after addition; processing rejects arithmetic overflow.",
        )
        .default(one())
        .minimum(i64::MIN)
        .maximum(i64::MAX as u64),
        Field::new(
            "counter_property",
            String,
            false,
            "Nonempty output property for the batch count; must differ from field.",
        )
        .default(counter_field()),
    ])
    .expect("constant arithmetic configuration")
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
    identity: GraphProducerIdentity,
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
        let identity = GraphProducerIdentity::volatile(
            "volatile".into(),
            "volatile".into(),
            descriptor.id().clone(),
            configuration.stream.clone(),
        )?;
        Ok(Self {
            descriptor,
            configuration,
            identity,
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
    /// Applies the calculation to an update. A removed field has no value to
    /// calculate from, so its removal passes through unchanged.
    fn calculate_update(
        &self,
        element: &mut Element,
        removed_properties: &mut BTreeSet<String>,
        batches: i64,
    ) -> anyhow::Result<Option<i64>> {
        if removed_properties.contains(&self.configuration.field) {
            return Ok(None);
        }
        let value = self.calculate(element, batches)?;
        removed_properties.remove(&self.configuration.counter_property);
        Ok(Some(value))
    }
}
#[async_trait]
impl NativeComponent for Arithmetic {
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
impl NativeTransformer for Arithmetic {
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
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    self.calculate(element, batches)?;
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    self.calculate_update(element, removed_properties, batches)?;
                }
                SourceChange::Delete { .. } => {}
                SourceChange::Future { .. } => {
                    anyhow::bail!("arithmetic does not process query scheduling control")
                }
            }
        }
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            self.configuration.stream.clone(),
            sequence,
        )?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
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
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    context.put_element(element).await?;
                    last_value = Some(self.calculate(element, batches)?);
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    if let Some(previous) = context.get_element(element.get_reference()).await? {
                        anyhow::ensure!(
                            std::mem::discriminant(element)
                                == std::mem::discriminant(previous.as_ref()),
                            "arithmetic update changed the element type"
                        );
                        element
                            .merge_missing_properties_except(previous.as_ref(), removed_properties);
                    }
                    context.put_element(element).await?;
                    if let Some(value) =
                        self.calculate_update(element, removed_properties, batches)?
                    {
                        last_value = Some(value);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::ComponentId;
    use drasi_computation_plugin_sdk::Factory;
    use drasi_core_types::models::{ElementMetadata, ElementPropertyMap, ElementReference};

    fn subject() -> Arithmetic {
        let descriptor = crate::ArithmeticFactory
            .metadata()
            .descriptor(ComponentId::try_new("arithmetic").unwrap())
            .unwrap();
        Arithmetic::new(
            descriptor,
            serde_json::json!({"stream": "out/arithmetic", "add": 1, "multiply": 2}),
        )
        .unwrap()
    }

    fn node(properties: &[(&str, ElementValue)]) -> Element {
        let mut map = ElementPropertyMap::new();
        for (name, value) in properties {
            map.insert(name, value.clone());
        }
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "one"),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1,
            },
            properties: map,
        }
    }

    #[test]
    fn calculating_an_update_that_sets_the_counter_drops_its_removal() {
        let mut element = node(&[("value", ElementValue::Integer(3))]);
        let mut removed = BTreeSet::from(["batch_count".to_string(), "other".to_string()]);
        let value = subject()
            .calculate_update(&mut element, &mut removed, 7)
            .unwrap();
        assert_eq!(value, Some(8));
        assert_eq!(element.get_property("value"), &ElementValue::Integer(8));
        assert_eq!(
            element.get_property("batch_count"),
            &ElementValue::Integer(7)
        );
        assert_eq!(removed, BTreeSet::from(["other".to_string()]));
        assert!(SourceChange::Update {
            element,
            removed_properties: removed
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn removing_the_calculated_field_passes_the_removal_through_unchanged() {
        let mut element = node(&[("other", ElementValue::Null)]);
        let mut removed = BTreeSet::from(["value".to_string()]);
        let value = subject()
            .calculate_update(&mut element, &mut removed, 7)
            .unwrap();
        assert_eq!(value, None);
        assert_eq!(removed, BTreeSet::from(["value".to_string()]));
        assert!(!element.get_properties().contains_key("batch_count"));
        assert!(!element.get_properties().contains_key("value"));
    }
}
