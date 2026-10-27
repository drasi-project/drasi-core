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

//! Producer-neutral view of standard graph changes, for sinks.
//!
//! A sink that accepts the default graph-change schema decodes the same way
//! whether a source, a transformer or a query produced the envelope. The view
//! uses only the envelope's records and its own system metadata. It never
//! needs optional query annotations, and it never invents a query identity.
//!
//! The view keeps the three update semantics distinct (full replacement,
//! explicit patch, removal), keeps the element identity, and keeps typed
//! values until a protocol projection asks for JSON. Scheduling controls and
//! query snapshot/progress recovery frames are not data; they fail clearly
//! instead of being dropped.
//!
//! Protocol bodies are built from the typed changes by [`GraphChange::to_json`]
//! and [`ChangeNotification::to_json`]. They state the real operation (`ADD`,
//! `REPLACE`, `PATCH`, `DELETE`), the canonical element identity, and only
//! images the record actually carries: an unknown `before` is absent, never
//! `null`, and a patch lists the properties it sets apart from the names it
//! removes. No evaluator row signature, query identity or query row shape is
//! involved.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use drasi_core_types::{
    data_value::{self, DataValue},
    evaluation::context::QueryVariables,
    models::ElementReference,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    internal::typed_change, ChangeEnvelope, ChangeOperation, ComponentId, ElementBeforeImage,
    ElementImage, GraphChangeCodec, GraphCodecError, InputBinding, OutputEndpoint, PortId,
    QueryChangeCodec, Record, RecordImage, StreamId, UpdateSemantics,
};

/// Why an envelope could not be viewed as ordinary graph data.
#[derive(Debug, thiserror::Error)]
pub enum GraphViewError {
    #[error(transparent)]
    Graph(Box<GraphCodecError>),
    #[error("graph scheduling control is not data and is not supported by this consumer")]
    Control,
    #[error("query snapshot and progress frames are recovery controls, not ordinary graph data")]
    RecoveryFrame,
    #[error("graph change {ordinal} uses unsupported record images: {detail}")]
    Unsupported { ordinal: u64, detail: &'static str },
    #[error("output bindings are invalid: {0}")]
    Bindings(String),
    #[error("input stream {0} is not bound to a configured output")]
    UnboundStream(String),
    #[error("invalid graph property value: {0}")]
    Value(#[from] data_value::DataValueError),
}

impl From<GraphCodecError> for GraphViewError {
    fn from(error: GraphCodecError) -> Self {
        Self::Graph(Box::new(error))
    }
}

/// How the element's stored state changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphChangeKind {
    /// The element joins the dataset; `after` is its complete image.
    Add,
    /// The complete after-image replaces the previous element.
    Replace,
    /// Only the named properties change; omitted properties are untouched.
    Patch,
    /// The element leaves the dataset.
    Delete,
}

/// Node or relation, when the record states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphElementKind {
    Node,
    Relation {
        in_node: ElementReference,
        out_node: ElementReference,
    },
}

/// Which operation-template family a change selects. A replacement and a patch
/// are both changes to an existing element; the operation name still tells them
/// apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeRoute {
    Added,
    Updated,
    Deleted,
}

/// One validated element change.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphChange {
    ordinal: u64,
    kind: GraphChangeKind,
    reference: ElementReference,
    element: Option<GraphElementKind>,
    labels: Vec<String>,
    effective_from: Option<u64>,
    before: Option<ElementBeforeImage>,
    after: Option<BTreeMap<String, DataValue>>,
    removed_properties: BTreeSet<String>,
}

impl GraphChange {
    pub fn ordinal(&self) -> u64 {
        self.ordinal
    }

    pub fn kind(&self) -> GraphChangeKind {
        self.kind
    }

    /// Output-scoped element identity: namespace plus opaque key.
    pub fn reference(&self) -> &ElementReference {
        &self.reference
    }

    /// `None` when only an identity-only deletion states no element kind.
    pub fn element(&self) -> Option<&GraphElementKind> {
        self.element.as_ref()
    }

    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// Business time in milliseconds carried by the element metadata. An
    /// identity-only deletion record states none.
    pub fn effective_from(&self) -> Option<u64> {
        self.effective_from
    }

    /// Complete properties for add/replace; the properties to set for a patch;
    /// absent for a deletion.
    pub fn after_properties(&self) -> Option<&BTreeMap<String, DataValue>> {
        self.after.as_ref()
    }

    pub fn removed_properties(&self) -> &BTreeSet<String> {
        &self.removed_properties
    }

    pub fn before(&self) -> Option<&ElementBeforeImage> {
        self.before.as_ref()
    }

    /// Properties of the known full before image. A metadata-only before image
    /// states only identity.
    pub fn before_properties(&self) -> Option<&BTreeMap<String, DataValue>> {
        match &self.before {
            Some(ElementBeforeImage::Element(image)) => Some(image.properties()),
            _ => None,
        }
    }

    /// Typed after properties as one JSON object, using the established
    /// query/row JSON value representation.
    pub fn after_json(&self) -> Result<Option<Value>, GraphViewError> {
        self.after.as_ref().map(properties_json).transpose()
    }

    pub fn before_json(&self) -> Result<Option<Value>, GraphViewError> {
        self.before_properties().map(properties_json).transpose()
    }

    /// The operation name used on the wire and in template contexts.
    pub fn operation(&self) -> &'static str {
        match self.kind {
            GraphChangeKind::Add => "ADD",
            GraphChangeKind::Replace => "REPLACE",
            GraphChangeKind::Patch => "PATCH",
            GraphChangeKind::Delete => "DELETE",
        }
    }

    pub fn route(&self) -> ChangeRoute {
        match self.kind {
            GraphChangeKind::Add => ChangeRoute::Added,
            GraphChangeKind::Replace | GraphChangeKind::Patch => ChangeRoute::Updated,
            GraphChangeKind::Delete => ChangeRoute::Deleted,
        }
    }

    /// The canonical element identity: output-scoped namespace and opaque key,
    /// exactly as the record states them.
    pub fn identity_json(&self) -> Value {
        reference_json(&self.reference)
    }

    /// Labels and, when the record states it, node or relation shape. `kind`
    /// is absent for a deletion that carries only identity metadata; the whole
    /// object is absent when nothing beyond the identity is known.
    pub fn element_json(&self) -> Option<Value> {
        if self.element.is_none() && self.labels.is_empty() {
            return None;
        }
        let mut element = json!({ "labels": self.labels });
        let object = element.as_object_mut().expect("object literal");
        match &self.element {
            Some(GraphElementKind::Node) => {
                object.insert("kind".into(), json!("node"));
            }
            Some(GraphElementKind::Relation { in_node, out_node }) => {
                object.insert("kind".into(), json!("relation"));
                object.insert("inNode".into(), reference_json(in_node));
                object.insert("outNode".into(), reference_json(out_node));
            }
            None => {}
        }
        Some(element)
    }

    /// One truthful change object.
    ///
    /// `operation` and `identity` are always present. `element`, `before` and
    /// `after` appear only when the record carries them, so an unknown image is
    /// absent and a property whose value is null stays `null`. `after` is the
    /// complete element for `ADD`/`REPLACE` and only the properties that a
    /// `PATCH` sets; `removedProperties` names what a patch removes.
    pub fn to_json(&self) -> Result<Value, GraphViewError> {
        let mut change = serde_json::Map::new();
        change.insert("operation".into(), json!(self.operation()));
        change.insert("identity".into(), self.identity_json());
        if let Some(element) = self.element_json() {
            change.insert("element".into(), element);
        }
        if let Some(before) = self.before_json()? {
            change.insert("before".into(), before);
        }
        if let Some(after) = self.after_json()? {
            change.insert("after".into(), after);
        }
        if self.kind == GraphChangeKind::Patch {
            change.insert(
                "removedProperties".into(),
                json!(self.removed_properties.iter().collect::<Vec<_>>()),
            );
        }
        if let Some(effective_from) = self.effective_from {
            change.insert("effectiveFrom".into(), json!(effective_from));
        }
        Ok(Value::Object(change))
    }
}

/// One change with its producer binding and the envelope's sequence and time.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeNotification {
    alias: String,
    binding: SinkOutput,
    sequence: u64,
    timestamp: Option<DateTime<Utc>>,
    change: GraphChange,
}

impl ChangeNotification {
    /// The configured route/display alias of the output.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// The actual component output that produced the data.
    pub fn output(&self) -> &SinkOutput {
        &self.binding
    }

    /// `{alias, component, port}`: the identity every protocol body carries.
    pub fn output_json(&self) -> Value {
        json!({
            "alias": self.alias,
            "component": self.binding.component.as_str(),
            "port": self.binding.port.as_str(),
        })
    }

    /// The producing stream's own sequence for the envelope.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The envelope's event time, or else the element's own business time;
    /// `None` when neither is stated.
    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.timestamp
    }

    pub fn change(&self) -> &GraphChange {
        &self.change
    }

    /// The default HTTP body and the template context: the change object plus
    /// `output` (the alias with the real `component` and `port`), `sequence` and,
    /// when the envelope carries one, an RFC 3339 `timestamp` (event time, never
    /// derived from the element's business time or the sequence).
    pub fn to_json(&self) -> Result<Value, GraphViewError> {
        let Value::Object(change) = self.change.to_json()? else {
            unreachable!("a change is an object")
        };
        let mut notification = serde_json::Map::new();
        notification.insert("operation".into(), change["operation"].clone());
        notification.insert("output".into(), self.output_json());
        notification.insert("sequence".into(), json!(self.sequence));
        if let Some(timestamp) = self.timestamp {
            notification.insert("timestamp".into(), json!(timestamp.to_rfc3339()));
        }
        for (name, value) in change {
            notification.entry(name).or_insert(value);
        }
        Ok(Value::Object(notification))
    }
}

/// One configured output: a real component output port and the stream that
/// carries its data into the sink.
///
/// The port is explicit. A sink cannot infer it from the stream name or choose a
/// component's first port; resolving a component's primary output for an
/// omitted port belongs to the host configuration layer that knows every
/// component's ports. Two outputs of one component are different bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkOutput {
    pub component: ComponentId,
    pub port: PortId,
    pub stream: StreamId,
}

impl SinkOutput {
    pub fn endpoint(&self) -> OutputEndpoint {
        OutputEndpoint::new(self.component.clone(), self.port.clone())
    }
}

/// A sink's optional naming of one incoming output. With no `port`, the alias
/// selects the only incoming output of `component`; it never picks among
/// several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputAlias {
    pub component: ComponentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<PortId>,
}

/// Optional route/display aliases for a sink's incoming outputs. Producers,
/// ports and streams are never written here; they come from the graph's
/// declared edges ([`InputBinding`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputAliases(BTreeMap<String, OutputAlias>);

impl OutputAliases {
    pub fn new(aliases: BTreeMap<String, OutputAlias>) -> Self {
        Self(aliases)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &OutputAlias)> {
        self.0.iter().map(|(alias, value)| (alias.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Alias names must be valid identifiers; whether they select an incoming
    /// edge is only known once the graph supplies its inputs.
    pub fn validate(&self) -> Result<(), GraphViewError> {
        for alias in self.0.keys() {
            crate::validate_identifier("output alias", alias)
                .map_err(|error| GraphViewError::Bindings(error.to_string()))?;
        }
        Ok(())
    }
}

/// The outputs a sink accepts, keyed by a route/display alias.
///
/// The alias names a configured binding for template routes, URLs and browser
/// subscriptions. It is not an identity: protocol bodies, snapshot lookup and
/// element identity all retain the actual component and port. A binding is
/// configured, never recovered from annotations on the data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SinkOutputs(BTreeMap<String, SinkOutput>);

impl SinkOutputs {
    pub fn new(bindings: BTreeMap<String, SinkOutput>) -> Result<Self, GraphViewError> {
        let bindings = Self(bindings);
        bindings.validate()?;
        Ok(bindings)
    }

    /// At least one binding; valid aliases; one binding per stream and per
    /// component output.
    pub fn validate(&self) -> Result<(), GraphViewError> {
        if self.0.is_empty() {
            return Err(GraphViewError::Bindings(
                "at least one output is required".into(),
            ));
        }
        let mut streams = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for (alias, binding) in &self.0 {
            crate::validate_identifier("output alias", alias)
                .map_err(|error| GraphViewError::Bindings(error.to_string()))?;
            if !streams.insert(&binding.stream) {
                return Err(GraphViewError::Bindings(format!(
                    "stream {} is bound to more than one output",
                    binding.stream
                )));
            }
            if !endpoints.insert(binding.endpoint()) {
                return Err(GraphViewError::Bindings(format!(
                    "output {} is bound more than once",
                    binding.endpoint()
                )));
            }
        }
        Ok(())
    }

    /// Outputs from the graph's declared incoming edges. An alias selects one
    /// incoming output; every other incoming output is named after its
    /// producing component, which must then be unambiguous. An alias that
    /// selects no incoming edge, or an input with no declared edge at all, is an
    /// error rather than an empty or guessed binding.
    pub fn from_inputs(
        inputs: &[InputBinding],
        aliases: &OutputAliases,
    ) -> Result<Self, GraphViewError> {
        let mut outputs: BTreeMap<OutputEndpoint, &StreamId> = BTreeMap::new();
        for input in inputs {
            if let Some(existing) = outputs.insert(input.output.clone(), &input.stream) {
                if existing != &input.stream {
                    return Err(GraphViewError::Bindings(format!(
                        "output {} is declared with more than one stream",
                        input.output
                    )));
                }
            }
        }
        if outputs.is_empty() {
            return Err(GraphViewError::Bindings(
                "the sink has no declared incoming edge".into(),
            ));
        }
        let mut named = BTreeMap::new();
        let mut aliased = BTreeSet::new();
        for (alias, selector) in &aliases.0 {
            let mut matching = outputs.keys().filter(|endpoint| {
                endpoint.component == selector.component
                    && selector
                        .port
                        .as_ref()
                        .map_or(true, |port| &endpoint.port == port)
            });
            let endpoint = match (matching.next(), matching.next()) {
                (Some(endpoint), None) => endpoint,
                (None, _) => {
                    return Err(GraphViewError::Bindings(format!(
                        "output alias {alias} selects {}{}, which is not an incoming edge",
                        selector.component,
                        selector
                            .port
                            .as_ref()
                            .map(|port| format!("/{port}"))
                            .unwrap_or_default()
                    )))
                }
                (Some(_), Some(_)) => {
                    return Err(GraphViewError::Bindings(format!(
                        "output alias {alias} selects several outputs of {}; name the port",
                        selector.component
                    )))
                }
            };
            if !aliased.insert(endpoint.clone()) {
                return Err(GraphViewError::Bindings(format!(
                    "output {endpoint} is aliased more than once"
                )));
            }
            named.insert(
                alias.clone(),
                SinkOutput {
                    component: endpoint.component.clone(),
                    port: endpoint.port.clone(),
                    stream: outputs[endpoint].clone(),
                },
            );
        }
        for (endpoint, stream) in &outputs {
            if aliased.contains(endpoint) {
                continue;
            }
            let alias = endpoint.component.to_string();
            let siblings = outputs
                .keys()
                .filter(|other| other.component == endpoint.component && !aliased.contains(*other))
                .count();
            if siblings > 1 {
                return Err(GraphViewError::Bindings(format!(
                    "{} has several incoming outputs; alias each one explicitly",
                    endpoint.component
                )));
            }
            if named
                .insert(
                    alias.clone(),
                    SinkOutput {
                        component: endpoint.component.clone(),
                        port: endpoint.port.clone(),
                        stream: (*stream).clone(),
                    },
                )
                .is_some()
            {
                return Err(GraphViewError::Bindings(format!(
                    "output alias {alias} is already used by another output"
                )));
            }
        }
        Self::new(named)
    }

    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn contains(&self, alias: &str) -> bool {
        self.0.contains_key(alias)
    }

    pub fn get(&self, alias: &str) -> Option<&SinkOutput> {
        self.0.get(alias)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SinkOutput)> {
        self.0
            .iter()
            .map(|(alias, binding)| (alias.as_str(), binding))
    }

    /// The configured output whose stream carried an input.
    pub fn output_for(&self, stream: &StreamId) -> Result<BoundOutput<'_>, GraphViewError> {
        self.0
            .iter()
            .find(|(_, binding)| binding.stream == *stream)
            .map(|(alias, binding)| BoundOutput { alias, binding })
            .ok_or_else(|| GraphViewError::UnboundStream(stream.to_string()))
    }
}

/// A configured output selected for one input.
#[derive(Debug, Clone, Copy)]
pub struct BoundOutput<'a> {
    pub alias: &'a str,
    pub binding: &'a SinkOutput,
}

/// The ordered data of one standard graph-change envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphChangeBatch {
    stream: StreamId,
    sequence: u64,
    timestamp: Option<DateTime<Utc>>,
    changes: Vec<GraphChange>,
}

impl GraphChangeBatch {
    /// View a standard graph-change envelope as ordinary data.
    ///
    /// Fails clearly for a different schema, an event-local scheduling control,
    /// a query snapshot/progress recovery frame, or an unsupported record
    /// image. An empty data envelope is valid and has no changes.
    pub fn from_envelope(envelope: &ChangeEnvelope) -> Result<Self, GraphViewError> {
        Self::check_envelope(envelope)?;
        let changes = envelope
            .changes()
            .operations()
            .iter()
            .map(GraphChange::from_operation)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            stream: envelope.system().stream().clone(),
            sequence: envelope.system().sequence(),
            timestamp: envelope.system().timestamp(),
            changes,
        })
    }

    /// Reject anything that is not ordinary graph data: a different schema, an
    /// event-local scheduling control, or a query snapshot/progress recovery frame.
    pub fn check_envelope(envelope: &ChangeEnvelope) -> Result<(), GraphViewError> {
        crate::data::validate_schema(
            GraphChangeCodec::schema().descriptor(),
            envelope.changes().schema(),
        )
        .map_err(GraphCodecError::from)?;
        if envelope.system().control().is_some() {
            return Err(GraphViewError::Control);
        }
        if QueryChangeCodec::is_snapshot(envelope) || QueryChangeCodec::is_progress_only(envelope) {
            return Err(GraphViewError::RecoveryFrame);
        }
        Ok(())
    }

    /// The notification for one operation of a checked envelope, for consumers
    /// that deliver operations individually.
    pub fn operation_notification(
        envelope: &ChangeEnvelope,
        operation: &ChangeOperation,
        output: BoundOutput<'_>,
    ) -> Result<ChangeNotification, GraphViewError> {
        Self::check_envelope(envelope)?;
        Ok(notification(
            output,
            envelope.system().sequence(),
            envelope.system().timestamp(),
            GraphChange::from_operation(operation)?,
        ))
    }

    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// The producing stream's own sequence for this envelope.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.timestamp
    }

    pub fn changes(&self) -> &[GraphChange] {
        &self.changes
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Every change as a notification for the configured output.
    pub fn notifications(&self, output: BoundOutput<'_>) -> Vec<ChangeNotification> {
        self.changes
            .iter()
            .map(|change| notification(output, self.sequence, self.timestamp, change.clone()))
            .collect()
    }
}

fn notification(
    output: BoundOutput<'_>,
    sequence: u64,
    envelope_time: Option<DateTime<Utc>>,
    change: GraphChange,
) -> ChangeNotification {
    ChangeNotification {
        alias: output.alias.to_owned(),
        binding: output.binding.clone(),
        sequence,
        timestamp: envelope_time,
        change,
    }
}

fn reference_json(reference: &ElementReference) -> Value {
    json!({"namespace": &*reference.source_id, "id": &*reference.element_id})
}

/// One JSON object of named values, using the established row JSON
/// representation.
pub fn properties_json(properties: &BTreeMap<String, DataValue>) -> Result<Value, GraphViewError> {
    let variables: QueryVariables = data_value::decode_variables(properties.clone())?;
    Ok(typed_change::query_variables_to_json(&variables))
}

fn full_before(record: &Record) -> Result<ElementBeforeImage, GraphViewError> {
    Ok(match record.image() {
        RecordImage::Full => ElementBeforeImage::Element(GraphChangeCodec::decode_element(record)?),
        RecordImage::Partial => GraphChangeCodec::decode_partial(record)?,
        RecordImage::Patch => {
            return Err(GraphViewError::Unsupported {
                ordinal: 0,
                detail: "a before image cannot be a patch",
            })
        }
    })
}

fn shape(image: &ElementImage) -> (Option<GraphElementKind>, Vec<String>, Option<u64>) {
    let metadata = image.metadata();
    let element = match image {
        ElementImage::Node { .. } => GraphElementKind::Node,
        ElementImage::Relation {
            in_node, out_node, ..
        } => GraphElementKind::Relation {
            in_node: in_node.clone(),
            out_node: out_node.clone(),
        },
    };
    (
        Some(element),
        metadata
            .labels
            .iter()
            .map(|label| label.to_string())
            .collect(),
        Some(metadata.effective_from),
    )
}

impl GraphChange {
    /// Decode one operation of a standard graph-change record set.
    pub fn from_operation(operation: &ChangeOperation) -> Result<Self, GraphViewError> {
        change(operation)
    }
}

fn change(operation: &ChangeOperation) -> Result<GraphChange, GraphViewError> {
    let ordinal = operation.ordinal();
    let before_of =
        |record: &Option<Record>| -> Result<Option<ElementBeforeImage>, GraphViewError> {
            record
                .as_ref()
                .map(|record| {
                    full_before(record).map_err(|error| match error {
                        GraphViewError::Unsupported { detail, .. } => {
                            GraphViewError::Unsupported { ordinal, detail }
                        }
                        other => other,
                    })
                })
                .transpose()
        };
    Ok(match operation {
        ChangeOperation::Added { after, .. } => {
            let image = GraphChangeCodec::decode_element(after)?;
            let (element, labels, effective_from) = shape(&image);
            GraphChange {
                ordinal,
                kind: GraphChangeKind::Add,
                reference: image.metadata().reference.clone(),
                element,
                labels,
                effective_from,
                before: None,
                after: Some(image.properties().clone()),
                removed_properties: BTreeSet::new(),
            }
        }
        ChangeOperation::Updated {
            before,
            after,
            semantics: UpdateSemantics::Replace,
            ..
        } => {
            let image = GraphChangeCodec::decode_element(after)?;
            let (element, labels, effective_from) = shape(&image);
            GraphChange {
                ordinal,
                kind: GraphChangeKind::Replace,
                reference: image.metadata().reference.clone(),
                element,
                labels,
                effective_from,
                before: before_of(before)?,
                after: Some(image.properties().clone()),
                removed_properties: BTreeSet::new(),
            }
        }
        ChangeOperation::Updated {
            before,
            after,
            semantics: UpdateSemantics::Patch,
            ..
        } => {
            let patch = GraphChangeCodec::decode_patch(after)?;
            let (element, labels, effective_from) = shape(patch.image());
            GraphChange {
                ordinal,
                kind: GraphChangeKind::Patch,
                reference: patch.reference().clone(),
                element,
                labels,
                effective_from,
                before: before_of(before)?,
                after: Some(patch.image().properties().clone()),
                removed_properties: patch.removed_properties().clone(),
            }
        }
        ChangeOperation::Deleted {
            identity,
            before: None,
            ..
        } => GraphChange {
            ordinal,
            kind: GraphChangeKind::Delete,
            reference: crate::binary_codec::decode_bounded_messagepack(
                identity.identity().value(),
                identity.identity().value().len(),
            )
            .map_err(GraphCodecError::from)?,
            element: None,
            labels: Vec::new(),
            effective_from: None,
            before: None,
            after: None,
            removed_properties: BTreeSet::new(),
        },
        ChangeOperation::Deleted { before, .. } => {
            let before = before_of(before)?.expect("deletion with a before record");
            let (element, labels, effective_from, reference) = match &before {
                ElementBeforeImage::Element(image) => {
                    let (element, labels, effective_from) = shape(image);
                    (
                        element,
                        labels,
                        effective_from,
                        image.metadata().reference.clone(),
                    )
                }
                ElementBeforeImage::Metadata(metadata) => (
                    None,
                    metadata
                        .labels
                        .iter()
                        .map(|label| label.to_string())
                        .collect(),
                    Some(metadata.effective_from),
                    metadata.reference.clone(),
                ),
            };
            GraphChange {
                ordinal,
                kind: GraphChangeKind::Delete,
                reference,
                element,
                labels,
                effective_from,
                before: Some(before),
                after: None,
                removed_properties: BTreeSet::new(),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use drasi_core_types::{
        evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
        models::{Element, ElementMetadata, ElementPropertyMap, ElementValue, SourceChange},
    };

    use super::*;
    use crate::{ComponentId, QueryOutputMetadata, ResultNodeProjector, SystemMetadata};

    fn stream(name: &str) -> StreamId {
        StreamId::try_new(name).unwrap()
    }

    fn binding(alias: &str, component: &str, port: &str, stream_id: &str) -> (String, SinkOutput) {
        (
            alias.to_owned(),
            SinkOutput {
                component: ComponentId::try_new(component).unwrap(),
                port: PortId::try_new(port).unwrap(),
                stream: stream(stream_id),
            },
        )
    }

    fn db_output() -> SinkOutputs {
        SinkOutputs::new(BTreeMap::from([binding(
            "db",
            "facilities-db",
            "out",
            "db/out",
        )]))
        .unwrap()
    }

    fn metadata(id: &str) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("db", id),
            labels: Arc::from([Arc::from("Room")]),
            effective_from: 1_000,
        }
    }

    fn node(id: &str, name: &str) -> Element {
        let mut properties = ElementPropertyMap::new();
        properties.insert("name", ElementValue::String(name.into()));
        Element::Node {
            metadata: metadata(id),
            properties,
        }
    }

    fn timestamp() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(5_000).unwrap()
    }

    fn source_envelope(changes: &[SourceChange]) -> ChangeEnvelope {
        GraphChangeCodec::encode_changes(changes, stream("db/out"), 7, Some(timestamp())).unwrap()
    }

    #[test]
    fn source_changes_state_the_real_operation_identity_and_only_known_images() {
        let mut with_null = ElementPropertyMap::new();
        with_null.insert("name", ElementValue::String("three".into()));
        with_null.insert("nickname", ElementValue::Null);
        let envelope = source_envelope(&[
            SourceChange::Insert {
                element: node("a", "one"),
            },
            SourceChange::Replace {
                element: node("a", "two"),
            },
            SourceChange::Update {
                element: Element::Node {
                    metadata: metadata("a"),
                    properties: with_null,
                },
                removed_properties: BTreeSet::from(["colour".to_owned()]),
            },
            SourceChange::Delete {
                metadata: metadata("a"),
            },
        ]);
        let batch = GraphChangeBatch::from_envelope(&envelope).unwrap();
        assert_eq!(batch.sequence(), 7);
        assert_eq!(batch.stream().as_str(), "db/out");
        let json: Vec<Value> = batch
            .changes()
            .iter()
            .map(|change| change.to_json().unwrap())
            .collect();
        let operations: Vec<_> = json.iter().map(|change| &change["operation"]).collect();
        assert_eq!(operations, ["ADD", "REPLACE", "PATCH", "DELETE"]);
        for change in &json {
            assert_eq!(change["identity"], json!({"namespace": "db", "id": "a"}));
            assert!(change.get("row_signature").is_none());
            assert!(change.get("before").is_none(), "unknown before is absent");
        }
        assert_eq!(json[0]["after"], json!({"name": "one"}));
        assert_eq!(
            json[0]["element"],
            json!({"kind": "node", "labels": ["Room"]})
        );
        // A patch keeps explicit null apart from a removed property.
        assert_eq!(json[2]["after"], json!({"name": "three", "nickname": null}));
        assert_eq!(json[2]["removedProperties"], json!(["colour"]));
        assert!(json[1].get("removedProperties").is_none());
        // An identity-only delete states neither before nor after.
        let delete = &json[3];
        assert!(delete.get("after").is_none());
        assert_eq!(delete["element"], json!({"labels": ["Room"]}));
        assert_eq!(batch.changes()[3].route(), ChangeRoute::Deleted);
        assert_eq!(batch.changes()[2].route(), ChangeRoute::Updated);
    }

    #[test]
    fn notifications_carry_the_binding_sequence_and_time_but_no_query_identity() {
        let envelope = source_envelope(&[
            SourceChange::Update {
                element: node("a", "x"),
                removed_properties: BTreeSet::new(),
            },
            SourceChange::Delete {
                metadata: metadata("a"),
            },
        ]);
        let batch = GraphChangeBatch::from_envelope(&envelope).unwrap();
        let bindings = db_output();
        let output = bindings.output_for(&stream("db/out")).unwrap();
        let notifications = batch.notifications(output);
        assert_eq!(notifications.len(), 2);
        let body = notifications[0].to_json().unwrap();
        assert_eq!(
            body["output"],
            json!({"alias":"db","component":"facilities-db","port":"out"})
        );
        assert!(body.get("producer").is_none());
        assert_eq!(body["sequence"], 7);
        assert_eq!(body["timestamp"], timestamp().to_rfc3339());
        assert_eq!(body["operation"], "PATCH");
        assert!(body.get("queryId").is_none() && body.get("query_id").is_none());
        // Without an envelope time there is no timestamp: the element's business
        // time is `effectiveFrom`, a different fact that is never relabelled.
        let untimed = GraphChangeCodec::encode_changes(
            &[SourceChange::Insert {
                element: node("a", "x"),
            }],
            stream("db/out"),
            1,
            None,
        )
        .unwrap();
        let untimed = GraphChangeBatch::from_envelope(&untimed)
            .unwrap()
            .notifications(output);
        assert_eq!(untimed[0].timestamp(), None);
        let untimed_body = untimed[0].to_json().unwrap();
        assert!(untimed_body.get("timestamp").is_none());
        assert_eq!(untimed_body["effectiveFrom"], 1_000);
        let per_operation = GraphChangeBatch::operation_notification(
            &envelope,
            &envelope.changes().operations()[1],
            output,
        )
        .unwrap();
        assert_eq!(per_operation, notifications[1]);
    }

    #[test]
    fn query_results_are_ordinary_nodes_and_need_no_query_annotations() {
        let projector = ResultNodeProjector::try_new("q/out", Some("RoomComfort")).unwrap();
        let row = |level: i64| {
            QueryVariables::from([
                ("RoomId".into(), VariableValue::from("r1")),
                ("ComfortLevel".into(), VariableValue::from(level)),
            ])
        };
        let annotated = QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("q").unwrap(),
            SystemMetadata::new(stream("q/out"), 3),
            &projector,
            &[
                QueryPartEvaluationContext::Adding {
                    after: row(40),
                    row_signature: 9,
                },
                QueryPartEvaluationContext::Updating {
                    before: row(40),
                    after: row(50),
                    row_signature: 9,
                },
                QueryPartEvaluationContext::Removing {
                    before: row(50),
                    row_signature: 9,
                },
            ],
            QueryOutputMetadata {
                query_id: "q".into(),
                source_id: None,
                timestamp: timestamp(),
                metadata: HashMap::new(),
                profiling: None,
            },
        )
        .unwrap()
        .unwrap();
        // The same records and system metadata, without any query annotation.
        let plain = ChangeEnvelope::new(
            annotated.id().clone(),
            annotated.changes().clone(),
            (**annotated.system()).clone(),
        );
        assert!(!QueryChangeCodec::is_query_output(&plain));
        let from_plain = GraphChangeBatch::from_envelope(&plain).unwrap();
        assert_eq!(
            from_plain,
            GraphChangeBatch::from_envelope(&annotated).unwrap()
        );
        let json: Vec<Value> = from_plain
            .changes()
            .iter()
            .map(|change| change.to_json().unwrap())
            .collect();
        // The evaluator's row identity is the node identity, as an opaque key.
        let identity = json!({"namespace": "q/out", "id": "0000000000000009"});
        assert!(json.iter().all(|change| change["identity"] == identity));
        assert_eq!(json[0]["operation"], "ADD");
        assert_eq!(json[0]["after"]["ComfortLevel"], 40);
        assert_eq!(json[1]["operation"], "REPLACE");
        assert_eq!(json[1]["before"]["ComfortLevel"], 40);
        assert_eq!(json[1]["after"]["ComfortLevel"], 50);
        assert_eq!(json[2]["operation"], "DELETE");
        assert_eq!(json[2]["before"]["ComfortLevel"], 50);
        assert_eq!(json[0]["element"]["labels"], json!(["RoomComfort"]));
        let node = &from_plain.changes()[0];
        assert_eq!(node.element(), Some(&GraphElementKind::Node));
    }

    #[test]
    fn controls_and_foreign_schemas_fail_instead_of_being_dropped() {
        let due = GraphChangeCodec::encode_futures_due(stream("db/out"), 1, timestamp()).unwrap();
        assert!(matches!(
            GraphChangeBatch::from_envelope(&due),
            Err(GraphViewError::Control)
        ));
        let empty = source_envelope(&[]);
        assert!(GraphChangeBatch::from_envelope(&empty).unwrap().is_empty());
    }

    #[test]
    fn untimed_metadata_free_identity_delete_is_valid_standard_data() {
        let identity =
            GraphChangeCodec::element_reference(&ElementReference::new("db", "gone")).unwrap();
        let changes = crate::ChangeSet::try_new(
            crate::ChangeSetId::try_new("test", bytes::Bytes::from_static(&[1])).unwrap(),
            GraphChangeCodec::schema().descriptor().clone(),
            vec![ChangeOperation::Deleted {
                ordinal: 0,
                identity,
                before: None,
            }],
        )
        .unwrap();
        let envelope = ChangeEnvelope::new(
            crate::emission_id(&stream("db/out"), 3).unwrap(),
            changes,
            SystemMetadata::new(stream("db/out"), 3),
        );
        assert!(envelope.system().timestamp().is_none());
        let bindings = db_output();
        let output = bindings.output_for(&stream("db/out")).unwrap();
        let notifications = GraphChangeBatch::from_envelope(&envelope)
            .unwrap()
            .notifications(output);
        assert_eq!(notifications.len(), 1);
        let body = notifications[0].to_json().unwrap();
        assert_eq!(body["operation"], "DELETE");
        assert_eq!(body["identity"], json!({"namespace": "db", "id": "gone"}));
        assert_eq!(body["sequence"], 3);
        for absent in ["timestamp", "effectiveFrom", "before", "after", "element"] {
            assert!(body.get(absent).is_none(), "{absent} is not invented");
        }
        assert_eq!(notifications[0].timestamp(), None);
    }

    fn input(port: &str, component: &str, output: &str) -> InputBinding {
        InputBinding {
            input_port: PortId::try_new(port).expect("port"),
            output: OutputEndpoint::new(
                ComponentId::try_new(component).expect("component"),
                PortId::try_new(output).expect("output port"),
            ),
            stream: StreamId::try_new(format!("{component}/{output}")).expect("stream"),
        }
    }

    fn aliases(value: serde_json::Value) -> OutputAliases {
        serde_json::from_value(value).expect("aliases")
    }

    #[test]
    fn outputs_come_from_declared_inputs_and_aliases_are_optional() -> Result<(), GraphViewError> {
        let inputs = [
            input("in", "facilities", "out"),
            input("in", "rooms", "out"),
            input("in", "rooms", "errors"),
        ];
        let named = SinkOutputs::from_inputs(
            &inputs,
            &aliases(json!({"ok": {"component": "rooms", "port": "out"},
                "bad": {"component": "rooms", "port": "errors"}})),
        )?;
        assert_eq!(
            named.aliases().collect::<Vec<_>>(),
            ["bad", "facilities", "ok"]
        );
        let ok = named.get("ok").expect("ok");
        assert_eq!(ok.endpoint().to_string(), "rooms/out");
        assert_eq!(ok.stream.as_str(), "rooms/out");
        assert_eq!(named.get("bad").expect("bad").port.as_str(), "errors");
        // Several outputs of one component are never guessed between.
        assert!(SinkOutputs::from_inputs(&inputs, &OutputAliases::default()).is_err());
        assert!(SinkOutputs::from_inputs(
            &inputs,
            &aliases(json!({"rooms": {"component": "rooms"}}))
        )
        .is_err());
        // An alias must select a declared incoming edge, once.
        for bad in [
            json!({"x": {"component": "other"}}),
            json!({"x": {"component": "rooms", "port": "missing"}}),
            json!({"x": {"component": "facilities"}, "y": {"component": "facilities", "port": "out"}}),
            json!({"facilities": {"component": "rooms", "port": "out"}}),
        ] {
            assert!(
                SinkOutputs::from_inputs(&inputs, &aliases(bad.clone())).is_err(),
                "{bad}"
            );
        }
        // A sink with no declared edge has nothing to accept.
        assert!(SinkOutputs::from_inputs(&[], &OutputAliases::default()).is_err());
        Ok(())
    }

    #[test]
    fn output_bindings_keep_component_and_port_and_reject_ambiguity() {
        let bindings = SinkOutputs::new(BTreeMap::from([
            binding("rooms-all", "rooms", "out", "rooms/out"),
            binding("rooms-errors", "rooms", "errors", "rooms/errors"),
            binding("facilities", "facilities-db", "out", "facilities-db/out"),
        ]))
        .unwrap();
        let errors = bindings.output_for(&stream("rooms/errors")).unwrap();
        assert_eq!(errors.alias, "rooms-errors");
        assert_eq!(errors.binding.component.as_str(), "rooms");
        assert_eq!(errors.binding.port.as_str(), "errors");
        assert_eq!(errors.binding.endpoint().to_string(), "rooms/errors");
        assert_ne!(
            bindings.get("rooms-all").unwrap().endpoint(),
            bindings.get("rooms-errors").unwrap().endpoint()
        );
        assert!(matches!(
            bindings.output_for(&stream("other/out")),
            Err(GraphViewError::UnboundStream(_))
        ));
        assert!(bindings.contains("facilities"));
        assert!(SinkOutputs::default().validate().is_err());
        assert!(SinkOutputs::new(BTreeMap::from([
            binding("a", "shared", "out", "shared/out"),
            binding("b", "other", "out", "shared/out"),
        ]))
        .is_err());
        assert!(SinkOutputs::new(BTreeMap::from([
            binding("a", "shared", "out", "shared/a"),
            binding("b", "shared", "out", "shared/b"),
        ]))
        .is_err());
        assert!(
            SinkOutputs::new(BTreeMap::from([binding("bad id", "a", "out", "a/out")])).is_err()
        );
    }
}
