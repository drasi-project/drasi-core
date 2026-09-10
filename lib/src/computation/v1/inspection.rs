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

use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// A coherent controller publication, not separately sampled desired/observed watches.
#[derive(Debug, Clone)]
pub struct ComputationInspection {
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub desired: Arc<GraphSnapshot>,
    pub observed: Arc<ObservedGraph>,
}

struct History {
    sequence: u64,
    entries: VecDeque<Arc<ComputationInspection>>,
}
struct InspectionState {
    latest: watch::Sender<Arc<ComputationInspection>>,
    history: Mutex<History>,
}

/// Read-only inspection and bounded controller history. Snapshots have no live
/// component/provider handles. Full history is bounded to the latest 256 publications.
#[derive(Clone)]
pub struct ComputationInspector(Arc<InspectionState>);

impl ComputationInspector {
    pub(super) fn new(desired: &GraphSnapshot, observed: Arc<ObservedGraph>) -> Self {
        let initial = Arc::new(ComputationInspection {
            sequence: 0,
            timestamp: Utc::now(),
            desired: Arc::new(desired.clone()),
            observed,
        });
        Self(Arc::new(InspectionState {
            latest: watch::channel(initial.clone()).0,
            history: Mutex::new(History {
                sequence: 0,
                entries: VecDeque::from([initial]),
            }),
        }))
    }
    pub fn snapshot(&self) -> Arc<ComputationInspection> {
        self.0.latest.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<ComputationInspection>> {
        self.0.latest.subscribe()
    }
    pub fn history(&self, after: u64) -> GraphResult<Vec<Arc<ComputationInspection>>> {
        let history = self.0.history.lock().map_err(|_| GraphError::Topology {
            reason: "inspection history ownership poisoned".into(),
        })?;
        if history
            .entries
            .front()
            .is_some_and(|oldest| after.saturating_add(1) < oldest.sequence)
        {
            return Err(GraphError::Topology {
                reason: "requested inspection history has been evicted; read a fresh snapshot"
                    .into(),
            });
        }
        Ok(history
            .entries
            .iter()
            .filter(|entry| entry.sequence > after)
            .cloned()
            .collect())
    }
    pub(super) fn publish(&self, desired: &GraphSnapshot, observed: Arc<ObservedGraph>) {
        let mut history = self.0.history.lock().unwrap_or_else(|error| {
            log::error!("Publishing through poisoned computation history: {error}");
            error.into_inner()
        });
        history.sequence = history
            .sequence
            .checked_add(1)
            .expect("controller observation sequence exhausted");
        let snapshot = Arc::new(ComputationInspection {
            sequence: history.sequence,
            timestamp: Utc::now(),
            desired: Arc::new(desired.clone()),
            observed,
        });
        history.entries.push_back(snapshot.clone());
        while history.entries.len() > 256 {
            history.entries.pop_front();
        }
        self.0.latest.send_replace(snapshot);
    }
}

pub struct ComputationInspectionResource(pub ComputationInspector);

/// A graph-as-data source over coherent computation state publications. It
/// converges to the latest state; callers needing every transition use history().
/// Configuration values, secrets, failure messages and handles are not emitted.
pub struct ComputationTopologySource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    inspector: ComputationInspector,
    changes: watch::Receiver<Arc<ComputationInspection>>,
    previous: BTreeMap<String, Element>,
    pending: VecDeque<(String, Option<Element>)>,
    sequence: u64,
}

impl ComputationTopologySource {
    pub fn new(id: ComponentId, stream: StreamId, inspector: ComputationInspector) -> Self {
        let changes = inspector.subscribe();
        Self {
            descriptor: ComponentDescriptor::try_new(
                id,
                vec![PortDescriptor::new(
                    PortId::try_new("out").expect("port"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("topology descriptor"),
            stream,
            inspector,
            changes,
            previous: BTreeMap::new(),
            pending: VecDeque::new(),
            sequence: 0,
        }
    }
    fn reference(&self, key: &str) -> ElementReference {
        ElementReference::new(self.descriptor.id().as_str(), key)
    }
    fn node(&self, id: &str, label: &str, properties: serde_json::Value) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: self.reference(id),
                labels: Arc::from([Arc::from(label)]),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(properties),
        }
    }
    fn relation(
        &self,
        id: &str,
        label: &str,
        from: &str,
        to: &str,
        properties: serde_json::Value,
    ) -> Element {
        Element::Relation {
            metadata: ElementMetadata {
                reference: self.reference(id),
                labels: Arc::from([Arc::from(label)]),
                effective_from: 0,
            },
            in_node: self.reference(from),
            out_node: self.reference(to),
            properties: ElementPropertyMap::from(properties),
        }
    }
    fn project(&self, view: &ComputationInspection) -> BTreeMap<String, Element> {
        let mut elements = BTreeMap::new();
        let encode = |value: &str| {
            value
                .bytes()
                .map(|value| format!("{value:02x}"))
                .collect::<String>()
        };
        elements.insert("graph".into(), self.node("graph", "ComputationGraph", serde_json::json!({
            "id": view.desired.id, "revision": view.desired.revision.0, "runEpoch": view.observed.run_epoch,
        })));
        for node in view.desired.nodes.iter() {
            let id = node.descriptor.id();
            let key = format!("component:{}", encode(id.as_str()));
            let observed = view.observed.components.get(id);
            elements.insert(key.clone(), self.node(&key, "ComputationComponent", serde_json::json!({
                "id": id.as_str(), "role": format!("{:?}", node.role),
                "realization": observed.map(|state| format!("{:?}", state.realization)),
                "lifecycle": observed.map(|state| format!("{:?}", state.lifecycle)),
                "health": observed.map(|state| format!("{:?}", state.health)),
                "generation": observed.map(|state| state.generation.0),
                "operation": observed.map(|state| state.operation.0),
                "failurePhase": observed.and_then(|state| state.failure.as_ref()).map(|failure| format!("{:?}", failure.phase)),
            })));
            let relation = format!("owns:{key}");
            elements.insert(
                relation.clone(),
                self.relation(
                    &relation,
                    "HAS_COMPONENT",
                    "graph",
                    &key,
                    serde_json::json!({}),
                ),
            );
            if let Some(spec) = view.desired.specifications.get(id) {
                for resource in spec.dependencies.values().flatten().chain(
                    spec.configuration.values().filter_map(|value| {
                        if let ConfigurationValue::Reference { resource, .. } = value {
                            Some(resource)
                        } else {
                            None
                        }
                    }),
                ) {
                    let target = format!("resource:{}", encode(resource.as_str()));
                    let relation = format!("uses:{key}:{target}");
                    elements.insert(
                        relation.clone(),
                        self.relation(
                            &relation,
                            "USES_RESOURCE",
                            &key,
                            &target,
                            serde_json::json!({}),
                        ),
                    );
                }
            }
        }
        for (id, resource) in &view.desired.resources {
            let key = format!("resource:{}", encode(id.as_str()));
            elements.insert(key.clone(), self.node(&key, "ComputationResource", serde_json::json!({
                "id": id.as_str(), "role": format!("{:?}", resource.role), "ownership": format!("{:?}", resource.ownership),
                "realization": view.observed.resources.get(id).map(|state| format!("{:?}", state.realization)),
            })));
        }
        for edge in view.desired.edges.iter() {
            let from = format!(
                "component:{}",
                encode(edge.definition.from.component.as_str())
            );
            let to = format!(
                "component:{}",
                encode(edge.definition.to.component.as_str())
            );
            let key = format!(
                "flow:{from}:{}:{to}:{}",
                encode(edge.definition.from.port.as_str()),
                encode(edge.definition.to.port.as_str())
            );
            let observed = view.observed.relationships.get(&edge.definition);
            elements.insert(key.clone(), self.relation(&key, "FLOWS_TO", &from, &to, serde_json::json!({
                "fromPort": edge.definition.from.port.as_str(), "toPort": edge.definition.to.port.as_str(),
                "binding": observed.map(|state| format!("{:?}", state.binding)),
                "availability": observed.map(|state| format!("{:?}", state.availability)),
            })));
        }
        for relationship in view.desired.unbound_relationships.iter() {
            let edge = &relationship.definition;
            let from = format!("component:{}", encode(edge.from.component.as_str()));
            let to = format!("component:{}", encode(edge.to.component.as_str()));
            let key = format!(
                "unbound:{from}:{}:{to}:{}",
                encode(edge.from.port.as_str()),
                encode(edge.to.port.as_str())
            );
            let observed = view.observed.relationships.get(edge);
            elements.insert(key.clone(), self.node(&key, "ComputationRelationship", serde_json::json!({
                "fromComponent": edge.from.component.as_str(), "fromPort": edge.from.port.as_str(),
                "toComponent": edge.to.component.as_str(), "toPort": edge.to.port.as_str(),
                "binding": observed.map(|state| format!("{:?}", state.binding)).unwrap_or_else(|| "Declared".into()),
                "orphanPermitted": relationship.policy.orphan_permitted,
            })));
            let owner = format!("declares:{key}");
            elements.insert(
                owner.clone(),
                self.relation(
                    &owner,
                    "HAS_RELATIONSHIP",
                    "graph",
                    &key,
                    serde_json::json!({}),
                ),
            );
            for (label, target) in [("FROM_COMPONENT", from), ("TO_COMPONENT", to)] {
                if elements.contains_key(&target) {
                    let relation = format!("{label}:{key}");
                    elements.insert(
                        relation.clone(),
                        self.relation(&relation, label, &key, &target, serde_json::json!({})),
                    );
                }
            }
        }
        elements
    }
    fn refresh(&mut self) {
        let current = self.changes.borrow_and_update().clone();
        let next = self.project(&current);
        self.pending.clear();
        for (id, element) in &next {
            if self.previous.get(id) != Some(element) {
                self.pending.push_back((id.clone(), Some(element.clone())));
            }
        }
        for id in self.previous.keys() {
            if !next.contains_key(id) {
                self.pending.push_back((id.clone(), None));
            }
        }
    }
}

#[async_trait]
impl ComputationComponent for ComputationTopologySource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.changes = self.inspector.subscribe();
        self.refresh();
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for ComputationTopologySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        loop {
            if let Some((id, element)) = self.pending.pop_front() {
                let timestamp = Utc::now();
                let effective_from = u64::try_from(timestamp.timestamp_millis())?;
                let change = if let Some(element) = &element {
                    let mut emitted = element.clone();
                    match &mut emitted {
                        Element::Node { metadata, .. } | Element::Relation { metadata, .. } => {
                            metadata.effective_from = effective_from
                        }
                    }
                    if self.previous.contains_key(&id) {
                        SourceChange::Update { element: emitted }
                    } else {
                        SourceChange::Insert { element: emitted }
                    }
                } else {
                    let old = self
                        .previous
                        .get(&id)
                        .ok_or_else(|| anyhow::anyhow!("missing topology deletion"))?;
                    let mut metadata = old.get_metadata().clone();
                    metadata.effective_from = effective_from;
                    SourceChange::Delete { metadata }
                };
                let sequence = self
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("topology producer sequence exhausted"))?;
                let envelope = GraphChangeCodec::encode_change(
                    change,
                    self.stream.clone(),
                    sequence,
                    Some(timestamp),
                )?;
                self.sequence = sequence;
                if let Some(element) = element {
                    self.previous.insert(id, element);
                } else {
                    self.previous.remove(&id);
                }
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope,
                }));
            }
            self.changes.changed().await?;
            self.refresh();
        }
    }
}

pub struct ComputationTopologyFactory {
    descriptor: FactoryDescriptor,
}

impl Default for ComputationTopologyFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/computation-topology", "1")
                    .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("stream"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("inspection"),
                    ResourceRequirement::exactly_one::<ComputationInspectionResource>(
                        ResourceRole::Inspection,
                    ),
                )]),
            },
        }
    }
}
#[async_trait]
impl ComponentFactory for ComputationTopologyFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = ComponentDescriptor::try_new(
            spec.descriptor.id().clone(),
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        if spec.descriptor != expected {
            anyhow::bail!("topology source requires its typed graph output");
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid topology stream"))?,
            )?;
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let inspector = context
            .resources::<ComputationInspectionResource>("inspection")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!(
                    "missing graph inspection resource"
                ))
            })?;
        let stream = context
            .configuration()
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing topology stream"))
            })?;
        Ok(ConstructedComponent::source(Box::new(
            ComputationTopologySource::new(
                context.component_id.clone(),
                StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?,
                inspector.0.clone(),
            ),
        )))
    }
}
