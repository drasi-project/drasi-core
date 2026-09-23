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

use super::{entities::encode_entity_key as encode, *};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, Weak},
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

impl ComputationInspection {
    /// Unified component, resource, plugin version/family and pipe entities from
    /// this publication. A family disappears with its last represented version.
    pub fn topology(&self) -> ComputationTopology {
        ComputationTopology::new(&self.desired, &self.observed)
    }
}

struct History {
    sequence: u64,
    entries: VecDeque<Arc<ComputationInspection>>,
}
struct InspectionState {
    latest: watch::Sender<Arc<ComputationInspection>>,
    history: Mutex<History>,
    observers: Mutex<Vec<Weak<dyn PublicationObserver>>>,
}

/// Synchronous, read-only delivery at the publication boundary. Observers must
/// not call back into the controller or inspector while handling a publication.
pub(crate) trait PublicationObserver: Send + Sync {
    fn publish(&self, previous: &ComputationInspection, current: &ComputationInspection);
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
            observers: Mutex::new(Vec::new()),
        }))
    }
    pub(crate) fn observe(&self, observer: &Arc<dyn PublicationObserver>) {
        self.0
            .observers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::downgrade(observer));
    }
    pub fn snapshot(&self) -> Arc<ComputationInspection> {
        self.0.latest.borrow().clone()
    }
    pub fn topology(&self) -> ComputationTopology {
        self.snapshot().topology()
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
        let previous = history
            .entries
            .back()
            .cloned()
            .expect("initial publication");
        history.entries.push_back(snapshot.clone());
        while history.entries.len() > 256 {
            history.entries.pop_front();
        }
        self.0.latest.send_replace(snapshot.clone());
        self.0
            .observers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|observer| {
                if let Some(observer) = observer.upgrade() {
                    observer.publish(&previous, &snapshot);
                    true
                } else {
                    false
                }
            });
    }
}

pub struct ComputationInspectionResource(pub ComputationInspector);

/// A graph-as-data source over coherent computation state publications. It
/// converges to the latest state; callers needing every transition use history().
/// Raw configuration, secrets, failure messages and handles are not emitted.
/// Component `kind` is the semantic category; `role` remains its execution role.
/// Semantic kind comes from explicit descriptor metadata, otherwise the execution
/// role. No adapter implementation name or configuration field is interpreted.
/// Plugin nodes use authoritative host observations, then explicit factory or
/// descriptor provenance; repeated provenance counts a dependent component once.
/// Version-specific `ComputationPlugin` nodes retain their existing keys and
/// `USES_PLUGIN` links. `VERSION_OF` links connect them to unversioned
/// `ComputationPluginFamily` nodes with version and distinct-component counts.
///
/// `ComputationPipe` nodes and `INPUT_TO_PIPE`/`OUTPUT_FROM_PIPE` links describe
/// each data path. `representation` distinguishes `nativeProvider` from
/// `hostSubscription`. Host subscriptions have observed endpoint generations,
/// startup/lifecycle state and readiness policies, not native ports, capabilities
/// or binding guarantees. Current peer-readiness flags are not in these snapshots.
/// Existing `FLOWS_TO` relations summarize native pipes, not additional delivery
/// paths. Unbound pipes retain their endpoints even when a component is absent;
/// endpoint links are emitted only for present components. Legacy unbound
/// `ComputationRelationship` nodes remain available as compatibility summaries.
/// `CONTROL_CONNECTION` links describe control-only host wiring and never create
/// pipe nodes. `USES_RESOURCE` includes factory and attached-provider dependencies.
/// `DEPENDS_ON_DATA` normalizes consumer -> pipe -> producer dependencies; these
/// links describe the existing path, not another delivery or activation policy.
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
        let topology = view.topology();
        elements.insert("graph".into(), self.node("graph", "ComputationGraph", serde_json::json!({
            "id": topology.graph_id, "revision": topology.revision.0, "runEpoch": topology.run_epoch,
        })));
        for (id, entity) in &topology.nodes {
            let key = id.key();
            match entity {
                GraphEntity::Component(component) => {
                    let node = &component.desired;
                    let observed = component.observed.as_ref();
                    let plugin = component.plugin_identity();
                    let (construction, implementation, configuration_version) =
                        match &component.construction {
                            ComponentEntityConstruction::Factory {
                                implementation,
                                configuration_version,
                            } => (
                                "Factory",
                                Some(implementation),
                                Some(*configuration_version),
                            ),
                            ComponentEntityConstruction::External => ("External", None, None),
                        };
                    let ports: Vec<_> = node.descriptor.ports().iter().map(|port| {
                        serde_json::json!({
                            "id": port.id().as_str(), "direction": format!("{:?}", port.direction()),
                            "schemaId": port.schema().id().as_str(),
                            "schemaVersion": port.schema().version().value(),
                            "encoding": port.schema().encoding(),
                        })
                    }).collect();
                    elements.insert(key.clone(), self.node(&key, "ComputationComponent", serde_json::json!({
                        "id": node.descriptor.id().as_str(), "role": format!("{:?}", node.role),
                        "kind": format!("{:?}", component.kind),
                        "construction": construction,
                        "implementation": implementation.map(|identity| identity.name.as_ref()),
                        "implementationVersion": implementation.map(|identity| identity.version.as_ref()),
                        "configurationVersion": configuration_version,
                        "pluginId": plugin.map(|identity| identity.id.as_ref()),
                        "pluginVersion": plugin.map(|identity| identity.version.as_ref()),
                        "ports": ports,
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
                }
                GraphEntity::Resource(resource) => {
                    let observed = resource.observed.as_ref();
                    elements.insert(key.clone(), self.node(&key, "ComputationResource", serde_json::json!({
                        "id": resource.desired.id.as_str(), "role": format!("{:?}", resource.desired.role),
                        "ownership": format!("{:?}", resource.desired.ownership),
                        "realization": observed.map(|state| format!("{:?}", state.realization)),
                        "generation": observed.map(|state| state.generation),
                        "failurePhase": observed.and_then(|state| state.failure.as_ref()).map(|failure| format!("{:?}", failure.phase)),
                    })));
                    let relation = format!("v1:declares:{key}");
                    elements.insert(
                        relation.clone(),
                        self.relation(
                            &relation,
                            "HAS_RESOURCE",
                            "graph",
                            &key,
                            serde_json::json!({}),
                        ),
                    );
                }
                GraphEntity::Plugin(plugin) => {
                    elements.insert(
                        key.clone(),
                        self.node(
                            &key,
                            "ComputationPlugin",
                            serde_json::json!({
                                "id": plugin.identity.id, "version": plugin.identity.version,
                                "dependentComponentCount": plugin.dependent_component_count(),
                            }),
                        ),
                    );
                }
                GraphEntity::PluginFamily(family) => {
                    elements.insert(
                        key.clone(),
                        self.node(
                            &key,
                            "ComputationPluginFamily",
                            serde_json::json!({
                                "id": family.id,
                                "versionCount": family.version_count(),
                                "dependentComponentCount": family.dependent_component_count(),
                            }),
                        ),
                    );
                }
                GraphEntity::Pipe(pipe) => {
                    let definition = &pipe.relationship.definition;
                    let profile = match &pipe.relationship.pipe {
                        DesiredPipe::Bounded { .. } => "Bounded",
                        DesiredPipe::Broadcast { .. } => "Broadcast",
                        DesiredPipe::Retained(_) => "Retained",
                        DesiredPipe::Ranked(_) => "Ranked",
                        DesiredPipe::External { .. } => "External",
                    };
                    elements.insert(key.clone(), self.node(&key, "ComputationPipe", serde_json::json!({
                        "fromComponent": definition.from.component.as_str(), "fromPort": definition.from.port.as_str(),
                        "toComponent": definition.to.component.as_str(), "toPort": definition.to.port.as_str(),
                        "representation": pipe.representation(),
                        "profile": profile, "declaredOnly": pipe.declared_only,
                        "binding": format!("{:?}", pipe.binding()),
                        "availability": format!("{:?}", pipe.availability()),
                        "generation": pipe.generation(),
                        "capabilities": pipe.capabilities.as_ref().map(PipeCapabilities::supported),
                        "capacity": pipe.capabilities.as_ref().and_then(PipeCapabilities::capacity).map(|capacity| capacity.get()),
                        "orphanPermitted": pipe.relationship.policy.orphan_permitted,
                        "failurePhase": pipe.failure().map(|failure| format!("{:?}", failure.phase)),
                    })));
                    let relation = format!("v1:owns:{key}");
                    elements.insert(
                        relation.clone(),
                        self.relation(&relation, "HAS_PIPE", "graph", &key, serde_json::json!({})),
                    );
                }
                GraphEntity::SubscriptionPipe(pipe) => {
                    let producer = pipe.producer_observed.as_ref();
                    let consumer = pipe.consumer_observed.as_ref();
                    elements.insert(key.clone(), self.node(&key, "ComputationPipe", serde_json::json!({
                        "representation": pipe.representation(),
                        "fromComponent": pipe.from.as_str(), "toComponent": pipe.to.as_str(),
                        "producerGeneration": pipe.producer_generation().map(|generation| generation.0),
                        "consumerGeneration": pipe.consumer_generation().map(|generation| generation.0),
                        "producerReadiness": {
                            "started": pipe.producer_started(),
                            "lifecycle": producer.map(|state| format!("{:?}", state.lifecycle)),
                            "requiresDownstreamReady": pipe.producer_requires_downstream_ready,
                        },
                        "consumerReadiness": {
                            "started": pipe.consumer_started(),
                            "lifecycle": consumer.map(|state| format!("{:?}", state.lifecycle)),
                            "requiresDownstreamReady": pipe.consumer_requires_downstream_ready,
                        },
                        "producerFailurePhase": producer.and_then(|state| state.failure.as_ref()).map(|failure| format!("{:?}", failure.phase)),
                        "consumerFailurePhase": consumer.and_then(|state| state.failure.as_ref()).map(|failure| format!("{:?}", failure.phase)),
                    })));
                    let relation = format!("v1:owns:{key}");
                    elements.insert(
                        relation.clone(),
                        self.relation(&relation, "HAS_PIPE", "graph", &key, serde_json::json!({})),
                    );
                }
            }
        }
        for link in &topology.links {
            let from = link.from.key();
            let to = link.to.key();
            let (key, label, properties) = match &link.kind {
                GraphEntityLinkKind::UsesResource => (
                    format!("uses:{from}:{to}"),
                    "USES_RESOURCE",
                    serde_json::json!({}),
                ),
                GraphEntityLinkKind::UsesComponent => (
                    format!("v1:uses-component:{from}:{to}"),
                    "USES_COMPONENT",
                    serde_json::json!({}),
                ),
                GraphEntityLinkKind::DependsOnData => {
                    if !topology.nodes.contains_key(&link.from)
                        || !topology.nodes.contains_key(&link.to)
                    {
                        continue;
                    }
                    (
                        format!("v1:depends-on-data:{from}:{to}"),
                        "DEPENDS_ON_DATA",
                        serde_json::json!({}),
                    )
                }
                GraphEntityLinkKind::DependsOnPlugin => (
                    format!("v1:uses-plugin:{from}:{to}"),
                    "USES_PLUGIN",
                    serde_json::json!({}),
                ),
                GraphEntityLinkKind::VersionOfPlugin => (
                    format!("v1:version-of:{from}:{to}"),
                    "VERSION_OF",
                    serde_json::json!({}),
                ),
                GraphEntityLinkKind::ControlConnection => {
                    if !topology.nodes.contains_key(&link.from)
                        || !topology.nodes.contains_key(&link.to)
                    {
                        continue;
                    }
                    (
                        format!("v1:control-connection:{from}:{to}"),
                        "CONTROL_CONNECTION",
                        serde_json::json!({ "representation": "subscriptionAdjacency" }),
                    )
                }
                GraphEntityLinkKind::PipeInput { port } => {
                    if !topology.nodes.contains_key(&link.from) {
                        continue;
                    }
                    (
                        format!("v1:pipe-input:{to}"),
                        "INPUT_TO_PIPE",
                        serde_json::json!({ "port": port.as_str() }),
                    )
                }
                GraphEntityLinkKind::PipeOutput { port } => {
                    if !topology.nodes.contains_key(&link.to) {
                        continue;
                    }
                    (
                        format!("v1:pipe-output:{from}"),
                        "OUTPUT_FROM_PIPE",
                        serde_json::json!({ "port": port.as_str() }),
                    )
                }
                GraphEntityLinkKind::SubscriptionInput => {
                    if !topology.nodes.contains_key(&link.from) {
                        continue;
                    }
                    (
                        format!("v1:pipe-input:{to}"),
                        "INPUT_TO_PIPE",
                        serde_json::json!({ "representation": PipeRepresentation::HostSubscription }),
                    )
                }
                GraphEntityLinkKind::SubscriptionOutput => {
                    if !topology.nodes.contains_key(&link.to) {
                        continue;
                    }
                    (
                        format!("v1:pipe-output:{from}"),
                        "OUTPUT_FROM_PIPE",
                        serde_json::json!({ "representation": PipeRepresentation::HostSubscription }),
                    )
                }
            };
            elements.insert(
                key.clone(),
                self.relation(&key, label, &from, &to, properties),
            );
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
                "pipeId": GraphEntityId::Pipe(edge.definition.clone()).key(), "representation": "pipeSummary",
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
                "pipeId": GraphEntityId::Pipe(edge.clone()).key(), "representation": "pipeSummary",
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
