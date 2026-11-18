// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The template context of one operation, identical in every
//! stored-procedure sink.

use anyhow::Result;
use drasi_computation_contracts::{ChangeRoute, DeliveryItem, GraphChangeBatch, SinkOutputs};
use drasi_reaction_common::OperationType;
use serde_json::{Map, Value};

/// The route and template context of one operation.
#[derive(Debug, Clone)]
pub struct OperationContext {
    /// The configured output alias that selects the route.
    pub alias: String,
    pub operation: OperationType,
    /// The change notification (`operation`, `identity`, `sequence`,
    /// `timestamp`, `element`, `before`, `after`, `removedProperties`, as the
    /// record carries them, and the `output` binding) plus `deliveryId`: the
    /// host's stable idempotency key of this operation, the same across
    /// retries and replays, so a procedure with external effects can
    /// deduplicate them.
    pub context: Value,
}

impl OperationContext {
    pub fn new(item: &DeliveryItem<'_>, outputs: &SinkOutputs) -> Result<Self> {
        let output = outputs.output_for(item.envelope.system().stream())?;
        let notification =
            GraphChangeBatch::operation_notification(item.envelope, item.operation, output)?;
        let operation = match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        };
        let Value::Object(mut context) = notification.to_json()? else {
            anyhow::bail!("the change notification is not an object");
        };
        context.insert("deliveryId".into(), Value::String(item.id.to_string()));
        Ok(Self {
            alias: output.alias.to_owned(),
            operation,
            context: Value::Object(context),
        })
    }

    /// The context as a JSON object.
    pub fn object(&self) -> &Map<String, Value> {
        self.context.as_object().expect("the context is an object")
    }
}
