// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Everything derived from one configuration that turns an input element into
//! its decision element.

use anyhow::{anyhow, Result};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;

use crate::{
    config::{
        OnError, PolicyConfig, Templates, ERROR_PROPERTY, FINGERPRINT_PROPERTY, HASH_PROPERTY,
        RESERVED, STATUS_PROPERTY,
    },
    digest::{canonical, sha256_hex},
    policy::{CompiledPolicy, Outcome},
};

/// Replaced as a whole by an in-place update.
pub(crate) struct Live {
    pub(crate) config: PolicyConfig,
    templates: Templates,
    policy: CompiledPolicy,
    label: Arc<[Arc<str>]>,
}

impl Live {
    pub(crate) fn new(config: PolicyConfig) -> Result<Self> {
        Ok(Self {
            templates: config.templates()?,
            policy: CompiledPolicy::compile(&config)?,
            label: Arc::from([Arc::from(config.output_label.as_str())]),
            config,
        })
    }

    /// Whether the element carries one of the configured labels.
    pub(crate) fn selects(&self, element: &Element) -> bool {
        self.config.labels.is_empty()
            || element
                .get_metadata()
                .labels
                .iter()
                .any(|label| self.config.labels.iter().any(|wanted| **wanted == **label))
    }

    /// Evaluates the policy for one retained input. Errors are returned only
    /// when `on_error` is `fail`; otherwise they become error decisions.
    pub(crate) fn decide(&self, element: &Element, effective_from: u64) -> Result<Element> {
        let reference = element.get_reference();
        let document = element_document(element);
        let (input, outcome) = match self.templates.input.resolve(&document) {
            Some(input) => {
                let text = canonical(&input);
                let outcome = self.policy.evaluate(&text);
                (Some((input, sha256_hex(text.as_bytes()))), outcome)
            }
            None => (
                None,
                Outcome::Error("the input template did not resolve for this element".into()),
            ),
        };
        if let (OnError::Fail, Outcome::Error(message)) = (self.config.on_error, &outcome) {
            return Err(anyhow!(
                "policy evaluation failed for {reference}: {message}"
            ));
        }
        let mut result = Map::new();
        if let Outcome::Decided(decision) = &outcome {
            result.insert("decision".into(), decision.clone());
        }
        if let Some((input, _)) = &input {
            result.insert("input".into(), input.clone());
        }
        result.insert("element".into(), document);
        result.insert("status".into(), json!(outcome.status()));
        if let Outcome::Error(message) = &outcome {
            result.insert("error".into(), json!(message));
        }
        let result = Value::Object(result);

        let mut properties = ElementPropertyMap::new();
        if self.config.copy_input_properties {
            for (name, value) in element
                .get_properties()
                .map_iter(|name, value| (name, value))
                .filter(|(name, _)| !RESERVED.contains(&&***name))
            {
                properties.insert(name, value.clone());
            }
        }
        for (name, template) in &self.templates.output {
            if let Some(value) = template.resolve(&result) {
                properties.insert(name, ElementValue::from(&value));
            }
        }
        properties.insert(
            STATUS_PROPERTY,
            ElementValue::String(outcome.status().into()),
        );
        if let Outcome::Error(message) = &outcome {
            properties.insert(
                ERROR_PROPERTY,
                ElementValue::String(message.as_str().into()),
            );
        }
        if self.config.provenance {
            properties.insert(
                HASH_PROPERTY,
                ElementValue::String(self.policy.hash().into()),
            );
            if let Some((_, fingerprint)) = &input {
                properties.insert(
                    FINGERPRINT_PROPERTY,
                    ElementValue::String(fingerprint.as_str().into()),
                );
            }
        }
        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: reference.clone(),
                labels: self.label.clone(),
                effective_from,
            },
            properties,
        })
    }
}

fn reference_document(reference: &ElementReference) -> Value {
    json!({"namespace": &*reference.namespace, "id": &*reference.element_id})
}

/// The document `input` selectors read. Date-times become ISO-8601 strings.
pub(crate) fn element_document(element: &Element) -> Value {
    let metadata = element.get_metadata();
    let mut document = json!({
        "id": &*metadata.reference.element_id,
        "namespace": &*metadata.reference.namespace,
        "labels": metadata.labels.iter().map(|label| &**label).collect::<Vec<_>>(),
        "properties": properties_json(element.get_properties()),
    });
    let members = document.as_object_mut().expect("object literal");
    match element {
        Element::Node { .. } => {
            members.insert("kind".into(), json!("node"));
        }
        Element::Relation {
            in_node, out_node, ..
        } => {
            members.insert("kind".into(), json!("relation"));
            members.insert("in_node".into(), reference_document(in_node));
            members.insert("out_node".into(), reference_document(out_node));
        }
    }
    document
}

fn properties_json(properties: &ElementPropertyMap) -> Value {
    Value::Object(
        properties
            .map_iter(|name, value| (name.to_string(), value_json(value)))
            .collect(),
    )
}

fn value_json(value: &ElementValue) -> Value {
    match value {
        ElementValue::List(items) => Value::Array(items.iter().map(value_json).collect()),
        ElementValue::Object(members) => properties_json(members),
        ElementValue::LocalDateTime(time) => json!(time.to_string()),
        ElementValue::ZonedDateTime(time) => json!(time.to_rfc3339()),
        other => Value::from(other),
    }
}
