// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Producer-neutral `ChangeService` items for the native gRPC sink.
//!
//! A change states its real operation, its canonical element identity and only
//! the images the record carries. Nothing here names a query or invents an
//! evaluator row signature.

use std::collections::HashMap;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ChangeNotification, ChangeRoute, GraphChangeKind, GraphElementKind,
};
use drasi_reaction_common::OperationType;
use prost::Message;
use serde_json::Value;

use crate::convert_json_to_proto_struct;
use crate::output::config::{GrpcReactionConfig, OutputFormat};
use crate::output::templates::{resolve_spec, TemplateEngine};
use crate::reaction::{
    element_shape::Kind, Change as ProtoChange, ChangeBatch, ChangeOperation as ProtoOperation,
    ElementIdentity, ElementShape, OutputIdentity, ProcessChangesRequest,
};

/// A protobuf change plus the per-change request metadata rendered from the
/// selected template extension.
#[derive(Debug, Clone)]
pub struct BuiltChange {
    pub change: ProtoChange,
    pub request_metadata: HashMap<String, String>,
}

fn identity(value: &Value) -> Option<ElementIdentity> {
    Some(ElementIdentity {
        namespace: value.get("namespace")?.as_str()?.to_owned(),
        id: value.get("id")?.as_str()?.to_owned(),
    })
}

fn shape(notification: &ChangeNotification) -> Option<ElementShape> {
    let element = notification.change().element_json()?;
    let kind = match notification.change().element() {
        Some(GraphElementKind::Node) => Kind::Node,
        Some(GraphElementKind::Relation { .. }) => Kind::Relation,
        None => Kind::Unspecified,
    };
    Some(ElementShape {
        kind: kind as i32,
        labels: notification
            .change()
            .labels()
            .iter()
            .map(ToString::to_string)
            .collect(),
        in_node: element.get("inNode").and_then(identity),
        out_node: element.get("outNode").and_then(identity),
    })
}

/// Build one `ChangeService` item and its rendered request metadata.
///
/// Routes are keyed by the output alias; `added`, `updated` and `deleted`
/// select the template (an update covers both a replacement and a patch, which
/// the context's `operation` distinguishes). The body template context and the
/// canonical payload are the same change object.
pub fn build_proto_change(
    config: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    notification: &ChangeNotification,
) -> Result<BuiltChange> {
    let change = notification.change();
    let body = notification.to_json()?;
    let context = body
        .as_object()
        .context("change notification is an object")?;
    let operation = match change.route() {
        ChangeRoute::Added => OperationType::Add,
        ChangeRoute::Updated => OperationType::Update,
        ChangeRoute::Deleted => OperationType::Delete,
    };
    let spec = resolve_spec(config, notification.alias(), operation);
    let label = change.operation();
    let request_metadata = match (engine, spec) {
        (Some(engine), Some(spec)) => {
            engine.render_metadata_from_context(context, spec, notification.alias(), label)
        }
        _ => HashMap::new(),
    };
    let rendered = match (engine, spec) {
        (Some(engine), Some(spec)) if !spec.template.trim().is_empty() => {
            engine.render_body_from_context(context, spec, notification.alias(), label)
        }
        _ => None,
    };
    let payload = rendered
        .or_else(|| (config.output_format == OutputFormat::CanonicalJson).then(|| body.clone()))
        .map(|value| convert_json_to_proto_struct(&value));
    let operation = match change.kind() {
        GraphChangeKind::Add => ProtoOperation::Add,
        GraphChangeKind::Replace => ProtoOperation::Replace,
        GraphChangeKind::Patch => ProtoOperation::Patch,
        GraphChangeKind::Delete => ProtoOperation::Delete,
    };
    let to_struct = |value: Option<Value>| value.map(|value| convert_json_to_proto_struct(&value));
    Ok(BuiltChange {
        change: ProtoChange {
            operation: operation as i32,
            identity: identity(&change.identity_json()),
            element: shape(notification),
            before: to_struct(change.before_json()?),
            after: to_struct(change.after_json()?),
            removed_properties: change.removed_properties().iter().cloned().collect(),
            sequence: notification.sequence(),
            timestamp: notification
                .timestamp()
                .map(|timestamp| prost_types::Timestamp {
                    seconds: timestamp.timestamp(),
                    nanos: timestamp.timestamp_subsec_nanos() as i32,
                }),
            payload,
        },
        request_metadata,
    })
}

/// Output (alias, component and port) and rendered request metadata: changes
/// with the same key share one RPC and are never reordered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeBatchKey {
    pub alias: String,
    pub component: String,
    pub port: String,
    metadata: Vec<(String, String)>,
}

impl ChangeBatchKey {
    pub fn new(output: &ChangeNotification, metadata: HashMap<String, String>) -> Self {
        let mut metadata: Vec<(String, String)> = metadata.into_iter().collect();
        metadata.sort();
        Self {
            alias: output.alias().to_owned(),
            component: output.output().component.as_str().to_owned(),
            port: output.output().port.as_str().to_owned(),
            metadata,
        }
    }

    /// `component/port`, the label used in logs.
    pub fn endpoint(&self) -> String {
        format!("{}/{}", self.component, self.port)
    }

    pub fn output_identity(&self) -> OutputIdentity {
        OutputIdentity {
            alias: self.alias.clone(),
            component: self.component.clone(),
            port: self.port.clone(),
        }
    }

    pub fn metadata_map(&self) -> HashMap<String, String> {
        self.metadata.iter().cloned().collect()
    }
}

/// Encoded size of an RPC around its changes, at the widest timestamp.
pub fn request_overhead(key: &ChangeBatchKey) -> usize {
    ProcessChangesRequest {
        changes: Some(ChangeBatch {
            output: Some(key.output_identity()),
            changes: Vec::new(),
            timestamp: Some(prost_types::Timestamp {
                seconds: i64::MIN,
                nanos: i32::MIN,
            }),
        }),
        metadata: key.metadata_map(),
    }
    .encoded_len()
        + 10
}

pub fn change_wire_size(change: &ProtoChange) -> usize {
    change.encoded_len() + 6
}

pub fn latest_change_timestamp(changes: &[ProtoChange]) -> Option<prost_types::Timestamp> {
    changes
        .iter()
        .filter_map(|change| change.timestamp)
        .max_by(|a, b| a.seconds.cmp(&b.seconds).then(a.nanos.cmp(&b.nanos)))
}

/// An ordered run of changes for one [`ChangeBatchKey`].
#[derive(Debug, Clone)]
pub struct PendingChanges {
    pub key: ChangeBatchKey,
    pub changes: Vec<ProtoChange>,
    pub seen_sequence: u64,
}

impl PendingChanges {
    pub fn new(key: ChangeBatchKey, change: ProtoChange) -> Self {
        Self {
            key,
            seen_sequence: change.sequence,
            changes: vec![change],
        }
    }

    pub fn push(&mut self, change: ProtoChange) {
        self.seen_sequence = self.seen_sequence.max(change.sequence);
        self.changes.push(change);
    }

    pub fn can_accept(&self, key: &ChangeBatchKey) -> bool {
        &self.key == key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use drasi_computation_contracts::{
        ComponentId, GraphChangeBatch, GraphChangeCodec, PortId, SinkOutput, SinkOutputs, StreamId,
    };
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn metadata() -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("db", "r1"),
            labels: Arc::from([Arc::<str>::from("Room")]),
            effective_from: 1_000,
        }
    }

    fn room(name: &str) -> Element {
        let mut properties = ElementPropertyMap::new();
        properties.insert("name", ElementValue::String(name.into()));
        properties.insert("nickname", ElementValue::Null);
        Element::Node {
            metadata: metadata(),
            properties,
        }
    }

    fn notifications() -> Vec<ChangeNotification> {
        let envelope = GraphChangeCodec::encode_changes(
            &[
                SourceChange::Insert {
                    element: room("one"),
                },
                SourceChange::Update {
                    element: room("two"),
                    removed_properties: ["colour".to_owned()].into(),
                },
                SourceChange::Delete {
                    metadata: metadata(),
                },
            ],
            StreamId::try_new("db/out").unwrap(),
            9,
            DateTime::from_timestamp(10, 0),
        )
        .unwrap();
        let outputs = SinkOutputs::new(BTreeMap::from([(
            "db".to_owned(),
            SinkOutput {
                component: ComponentId::try_new("facilities-db").unwrap(),
                port: PortId::try_new("out").unwrap(),
                stream: StreamId::try_new("db/out").unwrap(),
            },
        )]))
        .unwrap();
        let output = outputs
            .output_for(&StreamId::try_new("db/out").unwrap())
            .unwrap();
        GraphChangeBatch::from_envelope(&envelope)
            .unwrap()
            .notifications(output)
    }

    #[test]
    fn changes_state_operation_identity_and_only_known_images() {
        let config = GrpcReactionConfig::default();
        let built: Vec<_> = notifications()
            .iter()
            .map(|notification| build_proto_change(&config, None, notification).unwrap())
            .collect();
        let operations: Vec<_> = built.iter().map(|built| built.change.operation).collect();
        assert_eq!(
            operations,
            [
                ProtoOperation::Add as i32,
                ProtoOperation::Patch as i32,
                ProtoOperation::Delete as i32
            ]
        );
        for built in &built {
            let change = &built.change;
            assert_eq!(
                change.identity,
                Some(ElementIdentity {
                    namespace: "db".into(),
                    id: "r1".into()
                })
            );
            assert_eq!(change.sequence, 9);
            assert!(change.before.is_none(), "unknown before stays absent");
            assert!(change.payload.is_some());
        }
        let patch = &built[1].change;
        assert_eq!(patch.removed_properties, ["colour"]);
        let after = patch.after.as_ref().unwrap();
        assert!(
            matches!(
                after.fields["nickname"].kind,
                Some(prost_types::value::Kind::NullValue(_))
            ),
            "an explicit null is not a removal"
        );
        assert!(built[2].change.after.is_none());
        assert_eq!(
            built[2].change.element.as_ref().unwrap().kind,
            Kind::Unspecified as i32,
            "an identity-only delete states no node/relation kind"
        );
    }

    #[test]
    fn batches_split_on_output_or_metadata_without_reordering() {
        let notifications = notifications();
        let key = ChangeBatchKey::new(&notifications[0], [("x".into(), "1".into())].into());
        let built: Vec<_> = notifications
            .iter()
            .map(|notification| {
                build_proto_change(&GrpcReactionConfig::default(), None, notification)
                    .unwrap()
                    .change
            })
            .collect();
        let mut pending = PendingChanges::new(key.clone(), built[0].clone());
        pending.push(built[1].clone());
        assert!(pending.can_accept(&key));
        let mut other_port = key.clone();
        other_port.port = "errors".into();
        assert!(
            !pending.can_accept(&other_port),
            "a second port of one component is another output"
        );
        assert_eq!(key.endpoint(), "facilities-db/out");
        assert_eq!(
            key.output_identity(),
            OutputIdentity {
                alias: "db".into(),
                component: "facilities-db".into(),
                port: "out".into()
            }
        );
        assert_eq!(pending.seen_sequence, 9);
        assert!(request_overhead(&key) > 0);
        assert!(change_wire_size(&built[0]) > 6);
        assert!(latest_change_timestamp(&pending.changes).is_some());
    }
}
