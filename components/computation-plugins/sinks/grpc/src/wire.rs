// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::config::OutputFormat;
use anyhow::Result;
pub use drasi_computation_network_common::wire::{notifications, Notification, Operation};
use drasi_grpc_common::{convert_json_to_proto_struct, reaction};
use serde_json::Value;

/// Convert a native notification using this sink's selected output format.
pub fn proto_item(
    notification: &Notification,
    format: OutputFormat,
) -> Result<reaction::QueryResultItem> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(&notification.timestamp)?;
    let payload = if matches!(format, OutputFormat::CanonicalJson) {
        let mut canonical = serde_json::to_value(notification)?;
        canonical["rowSignature"] = Value::from(notification.row_signature);
        Some(convert_json_to_proto_struct(&canonical))
    } else {
        None
    };
    Ok(reaction::QueryResultItem {
        item_type: match notification.operation {
            Operation::Add => reaction::QueryResultItemType::Add,
            Operation::Update => reaction::QueryResultItemType::Update,
            Operation::Delete => reaction::QueryResultItemType::Delete,
        } as i32,
        row_signature: notification.row_signature,
        before: notification
            .before
            .as_ref()
            .map(convert_json_to_proto_struct),
        after: notification
            .after
            .as_ref()
            .map(convert_json_to_proto_struct),
        sequence: notification.sequence_id,
        timestamp: Some(prost_types::Timestamp {
            seconds: timestamp.timestamp(),
            nanos: timestamp.timestamp_subsec_nanos() as i32,
        }),
        metadata: notification
            .metadata
            .as_ref()
            .map(convert_json_to_proto_struct),
        payload,
    })
}
