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

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

mod computation_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use computation_support::*;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::computation::v1::*;

fn plugin(id: &str, version: &str) -> PluginIdentity {
    PluginIdentity {
        id: Arc::from(id),
        version: Arc::from(version),
    }
}

fn resource(id: &str) -> ResourceId {
    ResourceId::try_new(id).expect("resource ID")
}

fn named_component(name: &str) -> ComponentId {
    let value = match name {
        "public-source" => 735,
        "public-query" => 736,
        "public-reaction" => 737,
        "control-only" => 1018,
        "external-source" => 2_005,
        "external-query" => 2_006,
        "external-reaction" => 2_007,
        "factory-source" => 2_008,
        "factory-query" => 2_009,
        "factory-reaction" => 2_010,
        "same" => 2_011,
        "a:b" => 2_012,
        "a" => 2_013,
        "b:c" => 2_014,
        "雪" => 2_015,
        "source" => 77,
        "query" => 46,
        "reaction" => 2_018,
        "dynamic-source" => 1019,
        "dynamic-reaction" => 1020,
        "transformer" => 431,
        "sink" => 101,
        "service" => 432,
        "first" => 451,
        "second" => 768,
        "missing" => 738,
        other => panic!("missing explicit fixture component ID for {other}"),
    };
    fixture_support::component_id(value)
}

struct IdleService {
    descriptor: ComponentDescriptor,
    running: bool,
}

fn service(
    id: &drasi_core_types::ComponentId,
    identity: Option<PluginIdentity>,
) -> Box<IdleService> {
    let mut descriptor = descriptor(id, &[], &[]);
    if let Some(identity) = identity {
        descriptor = descriptor
            .with_plugin_identity(identity)
            .expect("provenance");
    }
    Box::new(IdleService {
        descriptor,
        running: false,
    })
}

#[async_trait]
impl ComputationComponent for IdleService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.running = true;
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        Ok(())
    }
}

#[async_trait]
impl ComputationService for IdleService {
    async fn run(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "service was not started");
        std::future::pending().await
    }
}

fn specification(
    id: &drasi_core_types::ComponentId,
    identity: Option<PluginIdentity>,
) -> ComponentSpecification {
    let mut implementation =
        ImplementationIdentity::try_new("fixture/service", "7").expect("implementation");
    implementation.plugin = identity;
    ComponentSpecification {
        descriptor: descriptor(id, &[], &[]),
        role: ComponentRole::Service,
        completion: None,
        implementation,
        configuration_version: 3,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::new(),
    }
}

fn runtime_specification(
    id: &drasi_core_types::ComponentId,
    kind: &str,
    semantic_kind: Option<ComponentSemanticKind>,
) -> ComponentSpecification {
    let mut spec = ComponentSpecification {
        implementation: ImplementationIdentity::try_new("drasi/lib-runtime-component", "1")
            .expect("runtime adapter"),
        configuration_version: 1,
        configuration: BTreeMap::from([
            (
                Arc::from("id"),
                ConfigurationValue::Literal(id.to_string().into()),
            ),
            (Arc::from("kind"), ConfigurationValue::Literal(kind.into())),
            (
                Arc::from("record_token"),
                ConfigurationValue::Literal(serde_json::json!(918273)),
            ),
            (
                Arc::from("definition"),
                ConfigurationValue::Literal(serde_json::json!({
                    "secret": "runtime-definition-marker",
                    "pluginId": "not-descriptor-provenance",
                    "pluginVersion": "99",
                })),
            ),
        ]),
        ..specification(id, None)
    };
    if let Some(kind) = semantic_kind {
        spec.descriptor = spec.descriptor.with_semantic_kind(kind);
    }
    spec
}

struct ServiceFactory {
    descriptor: FactoryDescriptor,
    fail: bool,
}

fn factory(spec: &ComponentSpecification, fail: bool) -> Arc<ServiceFactory> {
    Arc::new(ServiceFactory {
        descriptor: FactoryDescriptor {
            implementation: spec.implementation.clone(),
            role: spec.role,
            configuration_version: spec.configuration_version,
            configuration: ConfigurationSchema {
                fields: spec
                    .configuration
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            ConfigurationField {
                                constraint: None,
                                value_type: ConfigurationType::Json,
                                required: true,
                                secret: matches!(
                                    value,
                                    ConfigurationValue::Reference { secret: true, .. }
                                ),
                            },
                        )
                    })
                    .collect(),
                allow_additional: false,
            },
            dependencies: spec
                .dependencies
                .keys()
                .map(|slot| {
                    (
                        slot.clone(),
                        ResourceRequirement::exactly_one::<QueryIndexProviderResource>(
                            ResourceRole::IndexBackend,
                        ),
                    )
                })
                .collect(),
        },
        fail,
    })
}

#[async_trait]
impl ComponentFactory for ServiceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.implementation == proposed.implementation
                && current.configuration_version == proposed.configuration_version
                && current.role == proposed.role
                && current.completion == proposed.completion
                && current.dependencies == proposed.dependencies,
            "the stateless inspection fixture cannot change identity, interface or resources"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        if context.specification.dependencies.contains_key("indexes") {
            let providers = context
                .resources::<QueryIndexProviderResource>("indexes")
                .map_err(ComponentCreationError::terminal)?;
            assert_eq!(providers.len(), 1);
        }
        if self.fail {
            assert_eq!(
                context.configuration().get("secret-field-marker"),
                Some(&serde_json::json!("resolved-secret-marker"))
            );
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "private-failure-marker"
            )));
        }
        Ok(ConstructedComponent::service(Box::new(IdleService {
            descriptor: context.specification.descriptor.clone(),
            running: false,
        })))
    }
}

fn pipeline() -> ComputationGraphBuilder {
    let capacity = NonZeroUsize::new(2).expect("capacity");
    let store = Arc::new(RetainedStoreResource(Arc::new(MemoryEnvelopeStore::new(
        capacity,
        RetentionPolicy::Backpressure,
    ))));
    ComputationGraph::builder("entities")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![],
        )))
        .fixture_sink(Box::new(CollectSink::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            &["in"],
            Received::default(),
        )))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .declare_resource(ResourceSpecification {
            id: resource("store"),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from("private-store-binding-marker"),
        })
        .expect("declare store")
        .provide_resource(
            resource("store"),
            ResourceHandle::new(ResourceRole::StateStore, store.clone()).with_cleanup(store),
        )
        .expect("provide store")
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(RetainedPipeConfig {
                resource: resource("store"),
                capacity,
                durable: false,
                retention: RetentionPolicy::Backpressure,
                gap_policy: ReplayGapPolicy::Strict,
            }),
        )
        .relationship_policy(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            RelationshipPolicy {
                required_for_binding: false,
                orphan_permitted: true,
                ..Default::default()
            },
        )
}

fn subscription_graph() -> ComputationGraph {
    let mut builder = pipeline().fixture_service(service(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003fa")
            .expect("fixture UUID"),
        None,
    ));
    for (id, kind, semantic_kind) in [
        ("public-source", "source", ComponentSemanticKind::Source),
        ("public-query", "query", ComponentSemanticKind::Query),
        (
            "public-reaction",
            "reaction",
            ComponentSemanticKind::Reaction,
        ),
    ] {
        let component_id = named_component(id);
        let spec = runtime_specification(&component_id, kind, Some(semantic_kind));
        builder = builder.fixture_component(spec.clone(), factory(&spec, false));
    }
    builder
        .require_downstream_ready(component(735))
        .require_downstream_ready(component(736))
        .build()
        .expect("subscription graph")
}

fn subscription(from: &str, to: &str) -> GraphEntityId {
    GraphEntityId::Subscription {
        from: named_component(from),
        to: named_component(to),
    }
}

fn inspected_subscription<'a>(
    topology: &'a ComputationTopology,
    id: &GraphEntityId,
) -> &'a SubscriptionPipeEntity {
    let GraphEntity::SubscriptionPipe(pipe) = &topology.nodes[id] else {
        panic!("expected host subscription pipe");
    };
    pipe
}

fn inspected_pipe<'a>(
    topology: &'a ComputationTopology,
    definition: &EdgeDefinition,
) -> &'a PipeEntity {
    let GraphEntity::Pipe(pipe) = &topology.nodes[&GraphEntityId::Pipe(definition.clone())] else {
        panic!("expected pipe entity");
    };
    pipe
}

fn inspected_plugin<'a>(
    topology: &'a ComputationTopology,
    identity: &PluginIdentity,
) -> &'a PluginEntity {
    let GraphEntity::Plugin(plugin) = &topology.nodes[&GraphEntityId::Plugin(identity.clone())]
    else {
        panic!("expected plugin entity");
    };
    plugin
}

fn inspected_family<'a>(topology: &'a ComputationTopology, id: &str) -> &'a PluginFamilyEntity {
    let GraphEntity::PluginFamily(family) =
        &topology.nodes[&GraphEntityId::PluginFamily(Arc::from(id))]
    else {
        panic!("expected plugin family");
    };
    family
}

fn kind_count(topology: &ComputationTopology, kind: GraphEntityKind) -> usize {
    topology
        .nodes
        .values()
        .filter(|entity| entity.kind() == kind)
        .count()
}

async fn drain(source: &mut ComputationTopologySource) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(20), source.next()).await {
        let Some(event) = event.expect("topology event") else {
            break;
        };
        changes.extend(GraphChangeCodec::decode_changes(&event.envelope).expect("graph changes"));
    }
    changes
}

fn apply(elements: &mut BTreeMap<String, Element>, changes: &[SourceChange]) {
    for change in changes {
        let id = change.get_reference().element_id.to_string();
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element, .. } => {
                elements.insert(id, element.clone());
            }
            SourceChange::Delete { .. } => {
                elements.remove(&id);
            }
            _ => panic!("unexpected topology change"),
        }
    }
}

fn has_label(element: &Element, label: &str) -> bool {
    element
        .get_metadata()
        .labels
        .iter()
        .any(|value| value.as_ref() == label)
}

fn label_count(elements: &BTreeMap<String, Element>, label: &str) -> usize {
    elements
        .values()
        .filter(|element| has_label(element, label))
        .count()
}

#[test]
fn descriptor_provenance_round_trips_without_weakening_validation() {
    let plain = descriptor(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001bc")
            .expect("fixture UUID"),
        &["in"],
        &["out"],
    );
    let identity = plugin("plugin:/雪", "1.2.3");
    let descriptor = plain
        .clone()
        .with_semantic_kind(ComponentSemanticKind::Source)
        .with_plugin_identity(identity.clone())
        .expect("provenance");
    assert_eq!(descriptor.plugin_identity(), Some(&identity));
    let json = serde_json::to_value(&descriptor).expect("serialize descriptor");
    assert_eq!(json["plugin_identity"]["version"], "1.2.3");
    assert_eq!(
        serde_json::from_value::<ComponentDescriptor>(json.clone()).expect("deserialize"),
        descriptor
    );
    let mut old_json = json.clone();
    old_json
        .as_object_mut()
        .expect("object")
        .remove("plugin_identity");
    old_json
        .as_object_mut()
        .expect("object")
        .remove("semantic_kind");
    let old: ComponentDescriptor = serde_json::from_value(old_json).expect("old JSON");
    assert_eq!(old, plain);
    assert!(old.plugin_identity().is_none());
    assert!(serde_json::to_value(old)
        .expect("serialize old descriptor")
        .get("plugin_identity")
        .is_none());

    for invalid in ["", "two words", "control\ncharacter"] {
        for field in ["id", "version"] {
            let mut invalid_json = json.clone();
            invalid_json["plugin_identity"][field] = serde_json::json!(invalid);
            assert!(serde_json::from_value::<ComponentDescriptor>(invalid_json).is_err());
        }
        assert!(plain
            .clone()
            .with_plugin_identity(plugin(invalid, "1"))
            .is_err());
        assert!(plain
            .clone()
            .with_plugin_identity(plugin("plugin", invalid))
            .is_err());
    }
    let mut invalid_id = json.clone();
    invalid_id["id"] = serde_json::json!("");
    assert!(serde_json::from_value::<ComponentDescriptor>(invalid_id).is_err());
    let mut invalid_port = json.clone();
    invalid_port["ports"][0]["id"] = serde_json::json!("invalid port");
    assert!(serde_json::from_value::<ComponentDescriptor>(invalid_port).is_err());
    let mut duplicate_port = json.clone();
    duplicate_port["ports"]
        .as_array_mut()
        .expect("ports")
        .push(json["ports"][0].clone());
    assert!(serde_json::from_value::<ComponentDescriptor>(duplicate_port).is_err());
}

#[test]
fn descriptor_semantic_metadata_is_optional_and_part_of_descriptor_equality() {
    let plain_json = serde_json::json!({"id": component(445), "ports": []});
    let plain: ComponentDescriptor =
        serde_json::from_value(plain_json.clone()).expect("plain descriptor");
    assert_eq!(
        plain,
        descriptor(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001bd")
                .expect("fixture UUID"),
            &[],
            &[]
        )
    );
    assert!(plain.semantic_kind().is_none());
    assert_eq!(
        serde_json::to_value(&plain).expect("serialize plain descriptor"),
        plain_json
    );
    let identity = plugin("explicit-plugin", "1");
    for original in [
        plain.clone(),
        plain
            .clone()
            .with_plugin_identity(identity)
            .expect("provenance"),
    ] {
        let json = serde_json::to_value(&original).expect("serialize descriptor");
        assert!(json.get("semantic_kind").is_none());
        assert_eq!(
            serde_json::from_value::<ComponentDescriptor>(json.clone()).expect("old descriptor"),
            original
        );
        let mut explicit_null = json.clone();
        explicit_null["semantic_kind"] = serde_json::Value::Null;
        assert_eq!(
            serde_json::from_value::<ComponentDescriptor>(explicit_null)
                .expect("optional null metadata"),
            original
        );
        for (kind, serialized) in [
            (ComponentSemanticKind::Source, "Source"),
            (ComponentSemanticKind::Transformer, "Transformer"),
            (ComponentSemanticKind::Query, "Query"),
            (ComponentSemanticKind::Sink, "Sink"),
            (ComponentSemanticKind::Reaction, "Reaction"),
            (ComponentSemanticKind::Service, "Service"),
        ] {
            let annotated = original.clone().with_semantic_kind(kind);
            assert_eq!(annotated.semantic_kind(), Some(kind));
            assert_eq!(annotated.plugin_identity(), original.plugin_identity());
            assert!(original.semantic_kind().is_none());
            assert_ne!(annotated, original);
            let json = serde_json::to_value(&annotated).expect("serialize annotation");
            assert_eq!(json["semantic_kind"], serialized);
            assert_eq!(
                serde_json::from_value::<ComponentDescriptor>(json)
                    .expect("deserialize annotation"),
                annotated
            );
        }
        for invalid in [
            serde_json::json!("unknown"),
            serde_json::json!(1),
            serde_json::json!({"kind": "Source"}),
        ] {
            let mut invalid_json = json.clone();
            invalid_json["semantic_kind"] = invalid;
            assert!(serde_json::from_value::<ComponentDescriptor>(invalid_json).is_err());
        }
    }
    assert_ne!(
        plain
            .clone()
            .with_semantic_kind(ComponentSemanticKind::Source),
        plain.with_semantic_kind(ComponentSemanticKind::Reaction)
    );
}

#[test]
fn explicit_semantic_metadata_is_independent_of_construction_and_survives_import() {
    let cases = [
        ("source", ComponentSemanticKind::Source),
        ("query", ComponentSemanticKind::Query),
        ("reaction", ComponentSemanticKind::Reaction),
    ];
    let mut builder = ComputationGraph::builder("explicit-semantics");
    for (name, kind) in cases {
        let external_id = named_component(&format!("external-{name}"));
        let factory_id = named_component(&format!("factory-{name}"));
        let mut instance = service(&external_id, None);
        instance.descriptor = instance.descriptor.clone().with_semantic_kind(kind);
        let mut spec = specification(&factory_id, None);
        spec.descriptor = spec.descriptor.with_semantic_kind(kind);
        spec.configuration.insert(
            Arc::from("kind"),
            ConfigurationValue::Literal(serde_json::json!("service")),
        );
        builder = builder
            .fixture_service(instance)
            .fixture_component(spec.clone(), factory(&spec, false));
    }
    let graph = builder.build().expect("graph");
    let topology = graph.inspector().topology();
    let desired = graph
        .snapshot()
        .select(GraphSelection::All)
        .expect("selection");
    let imported = DesiredTopology::from_json(&desired.to_json().expect("export")).expect("import");
    for (name, kind) in cases {
        for construction in ["external", "factory"] {
            let id = named_component(&format!("{construction}-{name}"));
            let GraphEntity::Component(node) =
                &topology.nodes[&GraphEntityId::Component(id.clone())]
            else {
                panic!("component");
            };
            assert_eq!(node.desired.role, ComponentRole::Service);
            assert_eq!(node.desired.descriptor.semantic_kind(), Some(kind));
            assert_eq!(node.kind, kind);
            let imported = imported
                .components
                .iter()
                .find(|component| component.descriptor.id() == &id)
                .expect("imported component");
            assert_eq!(imported.descriptor, node.desired.descriptor);
            assert_eq!(imported.role, ComponentRole::Service);
        }
    }
}

#[test]
fn entity_keys_are_namespaced_and_encode_complete_identity_tuples() {
    let names = ["same", "a:b", "a", "b:c", "雪"];
    let mut ids = Vec::new();
    for first in names {
        ids.push(GraphEntityId::Component(named_component(first)));
        ids.push(GraphEntityId::Resource(resource(first)));
        ids.push(GraphEntityId::PluginFamily(Arc::from(first)));
        for second in names {
            ids.push(GraphEntityId::Plugin(plugin(first, second)));
            ids.push(subscription(first, second));
            for third in names {
                for fourth in names {
                    ids.push(GraphEntityId::Pipe(EdgeDefinition::new(
                        endpoint(&named_component(first), second),
                        endpoint(&named_component(third), fourth),
                    )));
                }
            }
        }
    }
    let keys: BTreeSet<_> = ids.iter().map(GraphEntityId::key).collect();
    assert_eq!(keys.len(), ids.len());
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    assert_eq!(
        GraphEntityId::Plugin(plugin("postgres", "1.2.0")).key(),
        "v1:plugin:706f737467726573:312e322e30",
        "existing version-node keys remain stable",
    );
}

#[test]
fn subscription_pipes_preserve_endpoint_observations_without_native_binding_claims() {
    let graph = subscription_graph();
    let mut inspection = graph.inspector().snapshot().as_ref().clone();
    let source_query = subscription("public-source", "public-query");
    let query_reaction = subscription("public-query", "public-reaction");
    let boundary = subscription("public-query", "missing");
    Arc::make_mut(&mut inspection.desired).subscriptions = Arc::from([
        (component(735), component(736)),
        (component(736), component(737)),
        (component(735), component(736)),
        (component(736), component(738)),
    ]);
    let cause = Arc::new(GraphError::Topology {
        reason: "private-endpoint-failure".into(),
    });
    let observed = Arc::make_mut(&mut inspection.observed);
    let producer = observed
        .components
        .get_mut(&component(735))
        .expect("producer");
    producer.generation = ComponentGeneration(41);
    producer.started = true;
    producer.lifecycle = ComponentLifecycle::Failed;
    producer.failure = Some(ComponentFailure {
        phase: FailurePhase::Processing,
        disposition: FailureDisposition::Retryable,
        cause: cause.clone(),
        timestamp: Utc::now(),
    });
    let consumer = observed
        .components
        .get_mut(&component(736))
        .expect("consumer");
    consumer.generation = ComponentGeneration(42);
    consumer.started = false;
    consumer.lifecycle = ComponentLifecycle::Starting;
    let topology = inspection.topology();
    assert_eq!(
        kind_count(&topology, GraphEntityKind::Pipe),
        4,
        "one native provider, two ordinary subscriptions and one explicit boundary"
    );
    assert_eq!(topology.nodes[&source_query].kind(), GraphEntityKind::Pipe);
    let pipe = inspected_subscription(&topology, &source_query);
    assert_eq!(pipe.representation(), PipeRepresentation::HostSubscription);
    assert_eq!((&pipe.from, &pipe.to), (&component(735), &component(736)));
    assert_eq!(pipe.producer_generation(), Some(ComponentGeneration(41)));
    assert_eq!(pipe.consumer_generation(), Some(ComponentGeneration(42)));
    assert_eq!(pipe.producer_started(), Some(true));
    assert_eq!(pipe.consumer_started(), Some(false));
    assert_eq!(
        pipe.producer_observed.as_ref().expect("producer").lifecycle,
        ComponentLifecycle::Failed
    );
    assert!(Arc::ptr_eq(
        &pipe
            .producer_observed
            .as_ref()
            .expect("producer")
            .failure
            .as_ref()
            .expect("failure")
            .cause,
        &cause,
    ));
    assert!(pipe.producer_requires_downstream_ready);
    assert!(pipe.consumer_requires_downstream_ready);
    assert!(inspected_subscription(&topology, &query_reaction).producer_requires_downstream_ready);
    assert!(!inspected_subscription(&topology, &query_reaction).consumer_requires_downstream_ready);
    assert_eq!(
        inspected_subscription(&topology, &boundary).consumer_generation(),
        None
    );
    assert_eq!(
        inspected_subscription(&topology, &boundary).consumer_started(),
        None
    );
    assert!(!topology
        .nodes
        .contains_key(&GraphEntityId::Component(component(738))));
    assert_eq!(
        topology
            .links
            .iter()
            .filter(|link| link.to == source_query
                && link.kind == GraphEntityLinkKind::SubscriptionInput)
            .count(),
        1
    );
    let native = inspected_pipe(
        &topology,
        &edge(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
        ),
    );
    assert_eq!(native.representation(), PipeRepresentation::NativeProvider);
    assert!(native.capabilities.is_some());
    let keys: BTreeSet<_> = topology.nodes.keys().cloned().collect();
    Arc::make_mut(&mut inspection.observed)
        .components
        .get_mut(&component(736))
        .expect("consumer")
        .generation = ComponentGeneration(100);
    let updated = inspection.topology();
    assert_eq!(updated.nodes.keys().cloned().collect::<BTreeSet<_>>(), keys);
    assert_eq!(
        inspected_subscription(&updated, &source_query).consumer_generation(),
        Some(ComponentGeneration(100))
    );
    assert_eq!(
        inspected_subscription(&updated, &query_reaction).producer_generation(),
        Some(ComponentGeneration(100))
    );
}

#[tokio::test]
async fn ordinary_source_query_reaction_subscriptions_are_first_class_pipe_nodes() {
    let mut graph = subscription_graph();
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        control
            .set_control_connections(vec![(component(735), component(1018))])
            .await
            .expect("control-only wiring");
        let subscriptions =
            vec![(component(735), component(736)), (component(736), component(737))];
        control
            .set_subscriptions(subscriptions.clone())
            .await
            .expect("configured data subscriptions");
        let topology = inspector.topology();
        let native_id = GraphEntityId::Pipe(edge(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
        ));
        assert_eq!(kind_count(&topology, GraphEntityKind::Pipe), 3);
        assert!(matches!(topology.nodes[&native_id], GraphEntity::Pipe(_)));
        assert!(!topology
            .nodes
            .contains_key(&subscription("public-source", "control-only")));
        for (id, kind) in [
            ("public-source", ComponentSemanticKind::Source),
            ("public-query", ComponentSemanticKind::Query),
            ("public-reaction", ComponentSemanticKind::Reaction),
        ] {
            let GraphEntity::Component(node) =
                &topology.nodes[&GraphEntityId::Component(named_component(id))]
            else {
                panic!("ordinary wrapper");
            };
            assert_eq!(node.kind, kind);
            assert_eq!(node.desired.role, ComponentRole::Service);
        }
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        let mut subscription_keys = BTreeSet::new();
        for (from, to) in &subscriptions {
            let id = GraphEntityId::Subscription {
                from: from.clone(),
                to: to.clone(),
            };
            subscription_keys.insert(id.key());
            assert_ne!(
                id.key(),
                GraphEntityId::Pipe(EdgeDefinition::new(
                    Endpoint::new(from.clone(), port("out")),
                    Endpoint::new(to.clone(), port("in")),
                ))
                .key()
            );
            let pipe = inspected_subscription(&topology, &id);
            let node = &elements[&id.key()];
            assert!(has_label(node, "ComputationPipe"));
            assert_eq!(
                node.get_property("representation"),
                &ElementValue::String(Arc::from("hostSubscription"))
            );
            assert_eq!(
                node.get_property("fromComponent"),
                &ElementValue::String(Arc::from(from.as_str()))
            );
            assert_eq!(
                node.get_property("toComponent"),
                &ElementValue::String(Arc::from(to.as_str()))
            );
            assert_eq!(
                node.get_property("producerGeneration"),
                &ElementValue::Integer(
                    i64::try_from(pipe.producer_generation().expect("producer").0)
                        .expect("generation")
                )
            );
            assert_eq!(
                node.get_property("consumerGeneration"),
                &ElementValue::Integer(
                    i64::try_from(pipe.consumer_generation().expect("consumer").0)
                        .expect("generation")
                )
            );
            for (name, started, required) in [
                (
                    "producerReadiness",
                    pipe.producer_started(),
                    pipe.producer_requires_downstream_ready,
                ),
                (
                    "consumerReadiness",
                    pipe.consumer_started(),
                    pipe.consumer_requires_downstream_ready,
                ),
            ] {
                let ElementValue::Object(readiness) = node.get_property(name) else {
                    panic!("endpoint readiness");
                };
                assert_eq!(
                    readiness.get("started"),
                    Some(&ElementValue::Bool(started.expect("observed startup")))
                );
                assert_eq!(
                    readiness.get("requiresDownstreamReady"),
                    Some(&ElementValue::Bool(required))
                );
                assert_eq!(
                    readiness.get("ready"),
                    None,
                    "current peer readiness is not published"
                );
            }
            for absent in [
                "capabilities",
                "capacity",
                "binding",
                "profile",
                "fromPort",
                "toPort",
                "configuration",
            ] {
                assert!(
                    node.get_properties().get(absent).is_none(),
                    "invented {absent}"
                );
            }
            for (label, kind, start, end) in [
                (
                    "INPUT_TO_PIPE",
                    GraphEntityLinkKind::SubscriptionInput,
                    GraphEntityId::Component(from.clone()),
                    id.clone(),
                ),
                (
                    "OUTPUT_FROM_PIPE",
                    GraphEntityLinkKind::SubscriptionOutput,
                    id.clone(),
                    GraphEntityId::Component(to.clone()),
                ),
            ] {
                assert!(topology.links.contains(&GraphEntityLink {
                    from: start.clone(),
                    to: end.clone(),
                    kind
                }));
                let links: Vec<_> = elements.values().filter(|element| {
                    has_label(element, label) && matches!(element, Element::Relation { in_node, out_node, .. }
                        if in_node.element_id.as_ref() == start.key() && out_node.element_id.as_ref() == end.key())
                }).collect();
                assert_eq!(links.len(), 1);
                assert!(links[0].get_properties().get("port").is_none());
                assert_eq!(
                    links[0].get_property("representation"),
                    &ElementValue::String(Arc::from("hostSubscription"))
                );
            }
        }
        assert_eq!(
            elements[&native_id.key()].get_property("representation"),
            &ElementValue::String(Arc::from("nativeProvider"))
        );
        assert!(elements[&native_id.key()]
            .get_properties()
            .get("capabilities")
            .is_some());
        assert_eq!(label_count(&elements, "ComputationPipe"), 3);
        assert_eq!(label_count(&elements, "INPUT_TO_PIPE"), 3);
        assert_eq!(label_count(&elements, "OUTPUT_FROM_PIPE"), 3);
        assert_eq!(
            label_count(&elements, "FLOWS_TO"),
            1,
            "legacy flows summarize native pipes only"
        );
        assert_eq!(label_count(&elements, "CONTROL_CONNECTION"), 1);
        for marker in [
            "runtime-definition-marker",
            "not-descriptor-provenance",
            "record_token",
            "private-store-binding-marker",
        ] {
            assert!(!format!("{elements:?}").contains(marker), "leaked {marker}");
        }
        control
            .set_subscriptions(subscriptions.iter().rev().cloned().collect())
            .await
            .expect("reorder subscriptions");
        let changes = drain(&mut source).await;
        assert!(
            !changes.iter().any(
                |change| subscription_keys.contains(change.get_reference().element_id.as_ref())
            ),
            "subscription identity does not depend on declaration order"
        );
        apply(&mut elements, &changes);
        control
            .set_control_connections(vec![])
            .await
            .expect("remove control-only wiring");
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(label_count(&elements, "CONTROL_CONNECTION"), 0);
        assert_eq!(label_count(&elements, "ComputationPipe"), 3);
        control
            .set_subscriptions(vec![subscriptions[0].clone()])
            .await
            .expect("remove query/reaction subscription");
        let removed = subscription("public-query", "public-reaction");
        let changes = drain(&mut source).await;
        assert!(changes.iter().any(|change| matches!(
            change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == removed.key()
        )));
        apply(&mut elements, &changes);
        assert!(!elements.contains_key(&removed.key()));
        assert!(elements.contains_key(&subscription("public-source", "public-query").key()));
        assert!(elements.contains_key(&native_id.key()));
        assert_eq!(label_count(&elements, "ComputationPipe"), 2);
        assert_eq!(label_count(&elements, "INPUT_TO_PIPE"), 2);
        assert_eq!(label_count(&elements, "OUTPUT_FROM_PIPE"), 2);
        assert_eq!(label_count(&elements, "FLOWS_TO"), 1);
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
}

#[test]
fn external_provenance_is_not_a_reconstruction_recipe() {
    let identity = plugin("explicit-plugin", "1");
    let graph = ComputationGraph::builder("external")
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b7")
                .expect("fixture UUID"),
            Some(identity.clone()),
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002dc")
                .expect("fixture UUID"),
            None,
        ))
        .build()
        .expect("graph");
    let topology = graph.inspector().topology();
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 1);
    assert_eq!(
        inspected_plugin(&topology, &identity).dependent_components,
        BTreeSet::from([component(439)])
    );
    let GraphEntity::Component(opaque) = &topology.nodes[&GraphEntityId::Component(component(732))]
    else {
        panic!("component");
    };
    assert!(opaque.plugin_identity().is_none());
    assert_eq!(opaque.kind, ComponentSemanticKind::Service);
    assert_eq!(opaque.construction, ComponentEntityConstruction::External);

    let desired = graph
        .snapshot()
        .select(GraphSelection::All)
        .expect("selection");
    assert!(desired
        .components
        .iter()
        .all(|node| matches!(node.construction, ComponentConstruction::External { .. })));
    let imported = DesiredTopology::from_json(&desired.to_json().expect("export")).expect("import");
    assert_eq!(
        imported
            .components
            .iter()
            .find(|node| node.descriptor.id() == &component(439))
            .expect("known instance")
            .descriptor
            .plugin_identity(),
        Some(&identity)
    );
    assert!(imported.build(TopologyBindings::default()).is_err());
}

#[tokio::test]
async fn ordinary_runtime_kinds_and_descriptor_provenance_are_projected() {
    let identity = plugin("ordinary-plugin", "1.2.3");
    let cases = [
        (
            "public-source",
            "source",
            ComponentSemanticKind::Source,
            true,
        ),
        ("public-query", "query", ComponentSemanticKind::Query, false),
        (
            "public-reaction",
            "reaction",
            ComponentSemanticKind::Reaction,
            true,
        ),
    ];
    let mut builder = ComputationGraph::builder("ordinary-entities");
    for (id, kind, semantic_kind, known_plugin) in cases {
        let component_id = named_component(id);
        let mut spec = runtime_specification(&component_id, kind, Some(semantic_kind));
        if known_plugin {
            spec.descriptor = spec
                .descriptor
                .with_plugin_identity(identity.clone())
                .expect("provenance");
        }
        builder = builder.fixture_component(spec.clone(), factory(&spec, false));
    }
    let graph = builder.build().expect("graph");
    let topology = graph.inspector().topology();
    for (id, _, expected_kind, known_plugin) in cases {
        let GraphEntity::Component(node) =
            &topology.nodes[&GraphEntityId::Component(named_component(id))]
        else {
            panic!("component");
        };
        assert_eq!(node.desired.descriptor.id(), &named_component(id));
        assert_eq!(node.kind, expected_kind);
        assert_eq!(node.desired.role, ComponentRole::Service);
        assert_eq!(node.plugin_identity(), known_plugin.then_some(&identity));
        let ComponentEntityConstruction::Factory { implementation, .. } = &node.construction else {
            panic!("factory");
        };
        assert_eq!(implementation.name.as_ref(), "drasi/lib-runtime-component");
        assert!(
            implementation.plugin.is_none(),
            "provenance belongs to the wrapped instance"
        );
    }
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 1);
    assert_eq!(
        inspected_plugin(&topology, &identity).dependent_components,
        BTreeSet::from([component(735), component(737)])
    );

    let mut source =
        ComputationTopologySource::new(component(733), stream("topology"), graph.inspector());
    source.start().await.expect("start topology source");
    let mut elements = BTreeMap::new();
    apply(&mut elements, &drain(&mut source).await);
    for (id, _, expected_kind, known_plugin) in cases {
        let node = &elements[&GraphEntityId::Component(named_component(id)).key()];
        assert_eq!(
            node.get_property("id"),
            &ElementValue::String(Arc::from(named_component(id).as_str()))
        );
        assert_eq!(
            node.get_property("role"),
            &ElementValue::String(Arc::from("Service"))
        );
        assert_eq!(
            node.get_property("kind"),
            &ElementValue::String(Arc::from(format!("{expected_kind:?}")))
        );
        assert_eq!(
            node.get_property("pluginId"),
            &if known_plugin {
                ElementValue::String(identity.id.clone())
            } else {
                ElementValue::Null
            }
        );
    }
    let plugin_id = GraphEntityId::Plugin(identity.clone());
    assert_eq!(topology.dependents(&plugin_id).count(), 2);
    assert_eq!(
        elements[&plugin_id.key()].get_property("dependentComponentCount"),
        &ElementValue::Integer(2)
    );
    assert_eq!(label_count(&elements, "USES_PLUGIN"), 2);
    for marker in [
        "runtime-definition-marker",
        "not-descriptor-provenance",
        "record_token",
        "918273",
    ] {
        assert!(!format!("{elements:?}").contains(marker), "leaked {marker}");
    }
    source.stop().await.expect("stop topology source");
}

#[test]
fn explicit_factory_provenance_takes_precedence_over_descriptor_provenance() {
    let factory_identity = plugin("factory-plugin", "7");
    let descriptor_identity = plugin("descriptor-plugin", "2");
    let mut spec = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000033")
            .expect("fixture UUID"),
        Some(factory_identity.clone()),
    );
    spec.descriptor = spec
        .descriptor
        .with_plugin_identity(descriptor_identity.clone())
        .expect("provenance");
    let graph = ComputationGraph::builder("precedence")
        .fixture_component(spec.clone(), factory(&spec, false))
        .build()
        .expect("graph");
    let topology = graph.inspector().topology();
    let GraphEntity::Component(node) = &topology.nodes[&GraphEntityId::Component(component(51))]
    else {
        panic!("component");
    };
    assert_eq!(node.plugin_identity(), Some(&factory_identity));
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 1);
    assert_eq!(kind_count(&topology, GraphEntityKind::PluginFamily), 1);
    assert_eq!(
        inspected_family(&topology, "factory-plugin").version_count(),
        1
    );
    assert!(!topology
        .nodes
        .contains_key(&GraphEntityId::PluginFamily(descriptor_identity.id.clone())));
    assert_eq!(
        inspected_plugin(&topology, &factory_identity).dependent_component_count(),
        1
    );
    assert!(!topology
        .nodes
        .contains_key(&GraphEntityId::Plugin(descriptor_identity)));
}

#[test]
fn host_origins_are_authoritative_and_count_each_dependent_once() {
    let declared = plugin("shared-library", "1");
    let actual = plugin("shared-library", "2");
    let mut spec = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e3")
            .expect("fixture UUID"),
        Some(declared.clone()),
    );
    spec.descriptor = spec
        .descriptor
        .with_plugin_identity(declared.clone())
        .expect("descriptor provenance");
    let graph = ComputationGraph::builder("host-origins")
        .fixture_component(spec.clone(), factory(&spec, false))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e4")
                .expect("fixture UUID"),
            None,
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002dc")
                .expect("fixture UUID"),
            None,
        ))
        .build()
        .expect("graph");
    let mut inspection = graph.inspector().snapshot().as_ref().clone();
    Arc::make_mut(&mut inspection.desired).component_plugins = BTreeMap::from([
        (component(739), declared.clone()),
        (component(740), declared.clone()),
        (component(741), plugin("unused-origin", "1")),
    ]);
    let topology = inspection.topology();
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 1);
    assert_eq!(kind_count(&topology, GraphEntityKind::PluginFamily), 1);
    assert_eq!(
        inspected_family(&topology, "shared-library").dependent_component_count(),
        2,
    );
    assert_eq!(
        inspected_plugin(&topology, &declared).dependent_components,
        BTreeSet::from([component(739), component(740)])
    );
    assert_eq!(
        topology
            .dependents(&GraphEntityId::Plugin(declared.clone()))
            .count(),
        2,
        "host, factory and descriptor provenance do not multiply dependencies"
    );

    Arc::make_mut(&mut inspection.desired)
        .component_plugins
        .insert(component(739), actual.clone());
    let topology = inspection.topology();
    let GraphEntity::Component(node) = &topology.nodes[&GraphEntityId::Component(component(739))]
    else {
        panic!("component");
    };
    assert_eq!(node.host_plugin_identity.as_ref(), Some(&actual));
    assert_eq!(node.plugin_identity(), Some(&actual));
    assert_eq!(node.desired.descriptor, spec.descriptor);
    assert_eq!(
        inspection.desired.specifications[&component(739)].implementation,
        spec.implementation,
        "inspection does not rewrite construction metadata"
    );
    assert_eq!(
        inspected_plugin(&topology, &actual).dependent_component_count(),
        1
    );
    assert_eq!(
        inspected_plugin(&topology, &declared).dependent_component_count(),
        1
    );
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 2);
    let family = inspected_family(&topology, "shared-library");
    assert_eq!(
        family.versions,
        BTreeSet::from([declared.clone(), actual.clone()])
    );
    assert_eq!(
        family.dependent_components,
        BTreeSet::from([component(739), component(740),])
    );

    Arc::make_mut(&mut inspection.desired)
        .component_plugins
        .remove(&component(739));
    let topology = inspection.topology();
    assert!(!topology.nodes.contains_key(&GraphEntityId::Plugin(actual)));
    assert_eq!(
        inspected_plugin(&topology, &declared).dependent_component_count(),
        2
    );
    Arc::make_mut(&mut inspection.desired)
        .component_plugins
        .remove(&component(740));
    let topology = inspection.topology();
    let GraphEntity::Component(node) = &topology.nodes[&GraphEntityId::Component(component(740))]
    else {
        panic!("component");
    };
    assert!(node.host_plugin_identity.is_none());
    assert!(node.plugin_identity().is_none());
    assert_eq!(
        inspected_plugin(&topology, &declared).dependent_component_count(),
        1
    );
}

#[tokio::test]
async fn late_host_plugin_observations_update_topology_without_mutating_descriptors() {
    use drasi_lib::context::{ComponentResourceObserver, PluginOrigin};

    let identity = plugin("dynamic-library", "9.4.2");
    let source_spec = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003fb")
            .expect("fixture UUID"),
        "source",
        Some(ComponentSemanticKind::Source),
    );
    let reaction_spec = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003fc")
            .expect("fixture UUID"),
        "reaction",
        Some(ComponentSemanticKind::Reaction),
    );
    let mut graph = ComputationGraph::builder("late-origins")
        .fixture_component(source_spec.clone(), factory(&source_spec, false))
        .fixture_component(reaction_spec.clone(), factory(&reaction_spec, false))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002dc")
                .expect("fixture UUID"),
            None,
        ))
        .build()
        .expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        assert_eq!(
            kind_count(&inspector.topology(), GraphEntityKind::Plugin),
            0
        );
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(label_count(&elements, "ComputationPlugin"), 0);
        assert_eq!(label_count(&elements, "ComputationPluginFamily"), 0);
        let plugin_id = GraphEntityId::Plugin(identity.clone());
        let family_id = GraphEntityId::PluginFamily(identity.id.clone());
        for (id, count) in [("dynamic-source", 1), ("dynamic-reaction", 2)] {
            let component_id = named_component(id);
            let generation = control
                .component_handle_by_id(&component_id)
                .expect("component handle")
                .generation();
            let mut publications = inspector.subscribe();
            GraphResourceObserver::new(control.clone(), component_id.clone(), generation)
                .observe_plugin(PluginOrigin {
                    id: identity.id.to_string(),
                    version: identity.version.to_string(),
                })
                .await
                .expect("report host origin");
            let publication = tokio::time::timeout(
                Duration::from_secs(2),
                publications.wait_for(|view| {
                    view.desired.component_plugins.get(&component_id) == Some(&identity)
                }),
            )
            .await
            .expect("origin publication deadline")
            .expect("origin publication")
            .clone();
            let topology = publication.topology();
            let entity_id = GraphEntityId::Component(component_id);
            let GraphEntity::Component(node) = &topology.nodes[&entity_id] else {
                panic!("component");
            };
            assert_eq!(node.plugin_identity(), Some(&identity));
            assert_eq!(node.host_plugin_identity.as_ref(), Some(&identity));
            assert!(node.desired.descriptor.plugin_identity().is_none());
            assert_eq!(
                inspected_plugin(&topology, &identity).dependent_component_count(),
                count
            );
            assert_eq!(topology.dependents(&plugin_id).count(), count);
            apply(&mut elements, &drain(&mut source).await);
            assert_eq!(label_count(&elements, "ComputationPlugin"), 1);
            assert_eq!(label_count(&elements, "ComputationPluginFamily"), 1);
            assert_eq!(label_count(&elements, "VERSION_OF"), 1);
            assert_eq!(
                inspected_family(&topology, &identity.id).dependent_component_count(),
                count,
            );
            assert_eq!(label_count(&elements, "USES_PLUGIN"), count);
            assert_eq!(
                elements[&entity_id.key()].get_property("pluginId"),
                &ElementValue::String(identity.id.clone())
            );
            assert_eq!(
                elements[&entity_id.key()].get_property("pluginVersion"),
                &ElementValue::String(identity.version.clone())
            );
        }
        assert_eq!(
            elements[&GraphEntityId::Component(component(732)).key()].get_property("pluginId"),
            &ElementValue::Null
        );

        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![component(1019)]),
                    policy: RemovalPolicy::Reject,
                }],
            )
            .await
            .expect("preview source removal");
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("remove source")
            .ensure_committed()
            .expect("desired topology committed");
        assert!(!inspector
            .snapshot()
            .desired
            .component_plugins
            .contains_key(&component(1019)));
        assert_eq!(
            inspected_plugin(&inspector.topology(), &identity).dependent_component_count(),
            1
        );
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(
            elements[&plugin_id.key()].get_property("dependentComponentCount"),
            &ElementValue::Integer(1)
        );

        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::Exact(vec![component(1020)]))
            .expect("reaction description");
        let replacement = desired.components.pop().expect("reaction");
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::ReplaceComponent(replacement)],
            )
            .await
            .expect("preview reaction replacement");
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("replace reaction")
            .ensure_committed()
            .expect("desired topology committed");
        let topology = inspector.topology();
        assert!(!topology.nodes.contains_key(&plugin_id));
        assert!(!topology.nodes.contains_key(&family_id));
        assert_eq!(topology.dependents(&plugin_id).count(), 0);
        let entity_id = GraphEntityId::Component(component(1020));
        let GraphEntity::Component(node) = &topology.nodes[&entity_id] else {
            panic!("replacement");
        };
        assert!(node.plugin_identity().is_none());
        assert!(node.host_plugin_identity.is_none());
        assert_eq!(node.desired.descriptor, reaction_spec.descriptor);
        let changes = drain(&mut source).await;
        assert!(changes.iter().any(|change| matches!(
            change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == plugin_id.key()
        )));
        apply(&mut elements, &changes);
        assert_eq!(label_count(&elements, "ComputationPlugin"), 0);
        assert_eq!(label_count(&elements, "ComputationPluginFamily"), 0);
        assert_eq!(label_count(&elements, "VERSION_OF"), 0);
        assert!(changes.iter().any(|change| matches!(
            change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == family_id.key()
        )));
        assert_eq!(label_count(&elements, "USES_PLUGIN"), 0);
        assert_eq!(
            elements[&entity_id.key()].get_property("pluginId"),
            &ElementValue::Null
        );
        assert_eq!(kind_count(&topology, GraphEntityKind::Pipe), 0);
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn semantic_kinds_do_not_interpret_arbitrary_or_unresolved_configuration() {
    let mut custom = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000429")
            .expect("fixture UUID"),
        "source",
        None,
    );
    custom.implementation =
        ImplementationIdentity::try_new("custom/runtime", "1").expect("implementation");
    let mut missing = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042a")
            .expect("fixture UUID"),
        "source",
        None,
    );
    missing.configuration.remove("kind");
    let mut unknown = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042b")
            .expect("fixture UUID"),
        "private-kind-marker",
        None,
    );
    let mut non_string = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042c")
            .expect("fixture UUID"),
        "source",
        None,
    );
    non_string.configuration.insert(
        Arc::from("kind"),
        ConfigurationValue::Literal(serde_json::json!({"source": "private-kind-marker"})),
    );
    let mut reference = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042d")
            .expect("fixture UUID"),
        "source",
        None,
    );
    reference.configuration.insert(
        Arc::from("kind"),
        ConfigurationValue::Reference {
            resource: resource("kind-resolver"),
            key: Arc::from("secret-reference-marker"),
            secret: true,
        },
    );
    let mut future_implementation = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042e")
            .expect("fixture UUID"),
        "source",
        None,
    );
    future_implementation.implementation.version = Arc::from("2");
    let mut future_schema = runtime_specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000042f")
            .expect("fixture UUID"),
        "source",
        None,
    );
    future_schema.configuration_version = 2;
    unknown.configuration.insert(
        Arc::from("pluginId"),
        ConfigurationValue::Literal(serde_json::json!("not-descriptor-provenance")),
    );
    unknown.configuration.insert(
        Arc::from("pluginVersion"),
        ConfigurationValue::Literal(serde_json::json!("99")),
    );
    let mut builder = ComputationGraph::builder("unknown-semantics")
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            None,
        ))
        .declare_resource(ResourceSpecification {
            id: resource("kind-resolver"),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("private-resolver-binding"),
        })
        .expect("declare resolver")
        .provide_resource(
            resource("kind-resolver"),
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(SecretResolver))),
            ),
        )
        .expect("provide resolver");
    for spec in [
        runtime_specification(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000430")
                .expect("fixture UUID"),
            "source",
            None,
        ),
        runtime_specification(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000431")
                .expect("fixture UUID"),
            "query",
            None,
        ),
        runtime_specification(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000432")
                .expect("fixture UUID"),
            "reaction",
            None,
        ),
        custom,
        missing,
        unknown,
        non_string,
        reference,
        future_implementation,
        future_schema,
    ] {
        builder = builder.fixture_component(spec.clone(), factory(&spec, false));
    }
    let graph = builder.build().expect("graph");
    let topology = graph.inspector().topology();
    for entity in topology.nodes.values() {
        if let GraphEntity::Component(node) = entity {
            assert!(node.desired.descriptor.semantic_kind().is_none());
            assert_eq!(node.kind, ComponentSemanticKind::Service);
            assert!(node.plugin_identity().is_none());
        }
    }
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 0);
    let mut source =
        ComputationTopologySource::new(component(733), stream("topology"), graph.inspector());
    source.start().await.expect("start topology source");
    let mut elements = BTreeMap::new();
    apply(&mut elements, &drain(&mut source).await);
    for node in elements
        .values()
        .filter(|element| has_label(element, "ComputationComponent"))
    {
        assert_eq!(
            node.get_property("kind"),
            &ElementValue::String(Arc::from("Service"))
        );
    }
    for marker in [
        "private-kind-marker",
        "secret-reference-marker",
        "resolved-secret-marker",
        "runtime-definition-marker",
        "private-resolver-binding",
        "not-descriptor-provenance",
    ] {
        assert!(!format!("{elements:?}").contains(marker), "leaked {marker}");
    }
    source.stop().await.expect("stop topology source");
}

#[test]
fn observed_component_provider_dependencies_are_explicit_even_without_a_factory() {
    let graph = pipeline().build().expect("graph");
    let mut inspection = graph.inspector().snapshot().as_ref().clone();
    let source = GraphEntityId::Component(component(77));
    let store = GraphEntityId::Resource(resource("store"));
    assert_eq!(inspection.topology().dependencies(&source).count(), 0);
    Arc::make_mut(&mut inspection.desired)
        .component_resources
        .insert(component(77), BTreeSet::from([resource("store")]));
    let topology = inspection.topology();
    assert_eq!(topology.dependencies(&source).count(), 1);
    assert!(topology.links.contains(&GraphEntityLink {
        from: source.clone(),
        to: store.clone(),
        kind: GraphEntityLinkKind::UsesResource,
    }));
    assert_eq!(
        topology.dependents(&store).count(),
        2,
        "component and actual retained pipe"
    );
    assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 0);
    let GraphEntity::Component(node) = &topology.nodes[&source] else {
        panic!("source");
    };
    assert_eq!(node.kind, ComponentSemanticKind::Source);
    assert_eq!(
        node.desired.descriptor.ports()[0].schema(),
        &schema_descriptor()
    );
    assert!(matches!(
        node.construction,
        ComponentEntityConstruction::External
    ));
}

#[test]
fn native_semantic_kinds_follow_declared_execution_roles() {
    let mut builder = ComputationGraph::builder("native-kinds")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![],
        )))
        .fixture_transformer(Box::new(NativeTransform::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001af")
                .expect("fixture UUID"),
            |_| Ok(vec![]),
        )))
        .fixture_transformer(Box::new(
            NativeTransform::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                |_| Ok(vec![]),
            )
            .with_semantic_kind(ComponentSemanticKind::Query)
            .with_output_progress(OutputProgress::Origin),
        ))
        .fixture_sink(Box::new(CollectSink::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            &["in"],
            Received::default(),
        )))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b0")
                .expect("fixture UUID"),
            None,
        ));
    for (producer, consumer) in
        [("source", "transformer"), ("transformer", "query"), ("query", "sink")]
    {
        builder = builder
            .bind_stream(
                endpoint(&named_component(producer), "out"),
                stream(producer),
            )
            .connect(
                edge(&named_component(producer), &named_component(consumer)),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    let graph = builder.build().expect("graph");
    let topology = graph.inspector().topology();
    for (id, role, kind) in [
        (
            "source",
            ComponentRole::Source,
            ComponentSemanticKind::Source,
        ),
        (
            "transformer",
            ComponentRole::Transformer,
            ComponentSemanticKind::Transformer,
        ),
        (
            "query",
            ComponentRole::Transformer,
            ComponentSemanticKind::Query,
        ),
        ("sink", ComponentRole::Sink, ComponentSemanticKind::Sink),
        (
            "service",
            ComponentRole::Service,
            ComponentSemanticKind::Service,
        ),
    ] {
        let GraphEntity::Component(node) =
            &topology.nodes[&GraphEntityId::Component(named_component(id))]
        else {
            panic!("component");
        };
        assert_eq!(node.desired.role, role);
        assert_eq!(
            node.desired.descriptor.semantic_kind().is_some(),
            id == "query",
            "only the query declares a semantic kind beyond its execution role"
        );
        assert_eq!(node.kind, kind);
        assert!(node.plugin_identity().is_none());
    }

    let GraphEntity::Component(query) = &topology.nodes[&GraphEntityId::Component(component(46))]
    else {
        panic!("query");
    };
    let specification = ComponentSpecification {
        descriptor: query.desired.descriptor.clone(),
        role: query.desired.role,
        completion: None,
        implementation: ImplementationIdentity::try_new("drasi/continuous-query", "1")
            .expect("implementation"),
        configuration_version: 1,
        configuration: Default::default(),
        dependencies: Default::default(),
    };
    let desired = DesiredComponent {
        name: drasi_lib::ComponentName::try_new("query").expect("fixture component name"),
        descriptor: specification.descriptor.clone(),
        role: specification.role,
        completion: None,
        streams: Default::default(),
        lifecycle: LifecyclePolicy { auto_start: false },
        input_merge: InputMergePolicy::default(),
        construction: ComponentConstruction::Factory(specification),
    };
    let serialized = serde_json::to_value(&desired).expect("desired component");
    assert_eq!(serialized["role"], "Transformer");
    assert_eq!(serialized["descriptor"]["semantic_kind"], "Query");
    assert_eq!(serialized["descriptor"]["output_progress"], "Origin");
    assert_eq!(serialized["construction"]["Factory"]["role"], "Transformer");
    let restored: DesiredComponent =
        serde_json::from_value(serialized).expect("current query transformer");
    assert_eq!(restored, desired);
}

#[test]
fn output_progress_must_be_valid_for_the_component_role() {
    let source = FiniteSource::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        vec![],
    )
    .with_output_progress(OutputProgress::Relay);
    let error = match ComputationGraph::builder("invalid-progress")
        .fixture_source(Box::new(source))
        .build()
    {
        Ok(_) => panic!("a source cannot relay input progress"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid output progress"));

    for progress in [OutputProgress::Origin, OutputProgress::Relay] {
        let mut component = service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b0")
                .expect("fixture UUID"),
            None,
        );
        component.descriptor = component.descriptor.clone().with_output_progress(progress);
        let error = match ComputationGraph::builder("invalid-progress")
            .fixture_service(component)
            .build()
        {
            Ok(_) => panic!("a service has no outputs"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("invalid output progress"));
    }
}

#[tokio::test]
async fn attached_provider_observations_are_queryable_without_invented_provenance() {
    use drasi_lib::{
        context::{ComponentResource, ComponentResourceObserver},
        state_store::{MemoryStateStoreProvider, StateStoreProvider},
    };

    let provider: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
    provider
        .set(
            &drasi_core_types::StorePartition::component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
            ),
            "private-provider-key",
            b"private-provider-value".to_vec(),
        )
        .await
        .expect("store state");
    let mut graph = pipeline().build().expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        let generation = control
            .component_handle_by_id(&component(77))
            .expect("component handle")
            .generation();
        let owner = component(77)
            .as_str()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let attached_id = resource(&format!("attached/{owner}/{}/state", generation.0));
        let mut publications = inspector.subscribe();
        GraphResourceObserver::new(control.clone(), component(77), generation)
            .observe(vec![ComponentResource::StateStore(provider.clone())])
            .await
            .expect("report actual provider");
        let publication = tokio::time::timeout(
            Duration::from_secs(2),
            publications.wait_for(|view| {
                view.desired
                    .component_resources
                    .get(&component(77))
                    .is_some_and(|resources| resources.contains(&attached_id))
            }),
        )
        .await
        .expect("provider publication deadline")
        .expect("provider publication")
        .clone();
        let topology = publication.topology();
        let source_id = GraphEntityId::Component(component(77));
        let resource_id = GraphEntityId::Resource(attached_id.clone());
        let GraphEntity::Resource(attached) = &topology.nodes[&resource_id] else {
            panic!("attached provider");
        };
        assert_eq!(attached.desired.role, ResourceRole::StateStore);
        assert_eq!(attached.desired.ownership, ResourceOwnership::Borrowed);
        assert_eq!(
            attached.observed.as_ref().expect("observation").realization,
            ResourceRealization::Created
        );
        assert!(topology.links.contains(&GraphEntityLink {
            from: source_id.clone(),
            to: resource_id.clone(),
            kind: GraphEntityLinkKind::UsesResource,
        }));
        assert_eq!(topology.dependencies(&source_id).count(), 1);
        assert_eq!(kind_count(&topology, GraphEntityKind::Resource), 2);
        assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 0);
        apply(&mut elements, &drain(&mut source).await);
        let node = &elements[&resource_id.key()];
        assert!(has_label(node, "ComputationResource"));
        assert_eq!(
            node.get_property("id"),
            &ElementValue::String(Arc::from(attached_id.as_str()))
        );
        assert_eq!(
            node.get_property("ownership"),
            &ElementValue::String(Arc::from("Borrowed"))
        );
        assert_eq!(
            node.get_property("realization"),
            &ElementValue::String(Arc::from("Created"))
        );
        assert_eq!(elements.values().filter(|element| {
            has_label(element, "USES_RESOURCE") && matches!(element, Element::Relation { in_node, out_node, .. }
                if in_node.element_id.as_ref() == source_id.key() && out_node.element_id.as_ref() == resource_id.key())
        }).count(), 1);
        for marker in ["private-provider-key", "private-provider-value", "MemoryStateStoreProvider"]
        {
            assert!(!format!("{elements:?}").contains(marker), "leaked {marker}");
        }
        assert_eq!(label_count(&elements, "ComputationPlugin"), 0);
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
    assert_eq!(
        provider
            .get(
                &drasi_core_types::StorePartition::component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID")
                ),
                "private-provider-key"
            )
            .await
            .expect("borrowed state"),
        Some(b"private-provider-value".to_vec())
    );
}

#[tokio::test]
async fn internal_subscription_adjacency_is_not_a_native_pipe() {
    let mut graph = pipeline()
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003fd")
                .expect("fixture UUID"),
            None,
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001a8")
                .expect("fixture UUID"),
            None,
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c9")
                .expect("fixture UUID"),
            None,
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003fe")
                .expect("fixture UUID"),
            None,
        ))
        .build()
        .expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let connections =
            vec![(component(1021), component(424)), (component(457), component(1022))];
        control
            .set_control_connections(connections.clone())
            .await
            .expect("host subscription adjacency");
        let topology = inspector.topology();
        assert_eq!(
            topology
                .links
                .iter()
                .filter(|link| link.kind == GraphEntityLinkKind::ControlConnection)
                .count(),
            2
        );
        for (from, to) in &connections {
            let from = GraphEntityId::Component(from.clone());
            assert!(topology.links.contains(&GraphEntityLink {
                from: from.clone(),
                to: GraphEntityId::Component(to.clone()),
                kind: GraphEntityLinkKind::ControlConnection,
            }));
            assert_eq!(
                topology.dependencies(&from).count(),
                0,
                "subscription adjacency does not imply a construction dependency"
            );
        }
        assert_eq!(kind_count(&topology, GraphEntityKind::Pipe), 1);
        let native_pipe = GraphEntityId::Pipe(edge(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
        ));
        assert!(topology.nodes.contains_key(&native_pipe));
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        let keys: BTreeSet<_> = elements
            .iter()
            .filter_map(|(key, element)| {
                has_label(element, "CONTROL_CONNECTION").then_some(key.clone())
            })
            .collect();
        assert_eq!(keys.len(), 2, "endpoint delimiters must not collide");
        for element in elements
            .values()
            .filter(|element| has_label(element, "CONTROL_CONNECTION"))
        {
            assert_eq!(
                element.get_property("representation"),
                &ElementValue::String(Arc::from("subscriptionAdjacency"))
            );
            assert_eq!(element.get_property("pipeId"), &ElementValue::Null);
            assert_eq!(element.get_property("binding"), &ElementValue::Null);
            assert_eq!(element.get_property("capabilities"), &ElementValue::Null);
            let Element::Relation {
                in_node, out_node, ..
            } = element
            else {
                panic!("subscription adjacency");
            };
            assert!(connections.iter().any(|(from, to)| {
                in_node.element_id.as_ref() == GraphEntityId::Component(from.clone()).key()
                    && out_node.element_id.as_ref() == GraphEntityId::Component(to.clone()).key()
            }));
        }
        assert_eq!(label_count(&elements, "ComputationPipe"), 1);
        assert_eq!(label_count(&elements, "FLOWS_TO"), 1);
        assert_eq!(label_count(&elements, "INPUT_TO_PIPE"), 1);
        assert_eq!(label_count(&elements, "OUTPUT_FROM_PIPE"), 1);

        control
            .set_control_connections(connections.into_iter().rev().collect())
            .await
            .expect("reorder adjacency");
        let changes = drain(&mut source).await;
        assert!(
            !changes
                .iter()
                .any(|change| keys.contains(change.get_reference().element_id.as_ref())),
            "adjacency keys and properties are independent of declaration order"
        );
        apply(&mut elements, &changes);
        assert!(keys.iter().all(|key| elements.contains_key(key)));

        control
            .set_control_connections(vec![])
            .await
            .expect("remove adjacency");
        let changes = drain(&mut source).await;
        let deleted: BTreeSet<_> = changes
            .iter()
            .filter_map(|change| match change {
                SourceChange::Delete { metadata }
                    if keys.contains(metadata.reference.element_id.as_ref()) =>
                {
                    Some(metadata.reference.element_id.to_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(deleted, keys);
        apply(&mut elements, &changes);
        assert_eq!(label_count(&elements, "CONTROL_CONNECTION"), 0);
        assert_eq!(label_count(&elements, "ComputationPipe"), 1);
        assert_eq!(label_count(&elements, "FLOWS_TO"), 1);
        assert!(elements.contains_key(&native_pipe.key()));
        assert!(!inspector
            .topology()
            .links
            .iter()
            .any(|link| link.kind == GraphEntityLinkKind::ControlConnection));
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
}

#[tokio::test]
async fn native_provider_nodes_keep_real_ownership_realization_and_dependencies() {
    let identity = plugin("service-plugin", "1");
    let mut spec = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b0")
            .expect("fixture UUID"),
        Some(identity.clone()),
    );
    spec.dependencies
        .insert(Arc::from("indexes"), vec![resource("source")]);
    let mut graph = pipeline()
        .fixture_component(spec.clone(), factory(&spec, false))
        .declare_resource(ResourceSpecification {
            id: resource("source"),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("index-provider"),
        })
        .expect("declare indexes")
        .provide_resource(
            resource("source"),
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(Arc::new(
                    drasi_core::computation::InMemoryComputationProvider,
                ))),
            ),
        )
        .expect("provide indexes")
        .build()
        .expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deploy");
        let topology = inspector.topology();
        assert_eq!(kind_count(&topology, GraphEntityKind::Component), 3);
        assert_eq!(kind_count(&topology, GraphEntityKind::Resource), 2);
        assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 1);
        assert_eq!(kind_count(&topology, GraphEntityKind::Pipe), 1);
        for (id, role, ownership) in [
            (
                "source",
                ResourceRole::IndexBackend,
                ResourceOwnership::Borrowed,
            ),
            ("store", ResourceRole::StateStore, ResourceOwnership::Graph),
        ] {
            let GraphEntity::Resource(provider) =
                &topology.nodes[&GraphEntityId::Resource(resource(id))]
            else {
                panic!("provider");
            };
            assert_eq!(provider.desired.role, role);
            assert_eq!(provider.desired.ownership, ownership);
            assert_eq!(
                provider.observed.as_ref().expect("observation").realization,
                ResourceRealization::Created
            );
        }
        let component_id = GraphEntityId::Component(component(432));
        let GraphEntity::Component(service) = &topology.nodes[&component_id] else {
            panic!("service");
        };
        assert!(matches!(
            &service.construction,
            ComponentEntityConstruction::Factory { implementation, configuration_version: 3 }
                if implementation.name.as_ref() == "fixture/service" && implementation.version.as_ref() == "7"
        ));
        assert_eq!(topology.dependencies(&component_id).count(), 2);
        let mut inspection = inspector.snapshot().as_ref().clone();
        Arc::make_mut(&mut inspection.desired)
            .component_resources
            .insert(component(432), BTreeSet::from([resource("source")]));
        assert_eq!(
            inspection.topology().dependencies(&component_id).count(),
            2,
            "the same declared and observed provider is one dependency"
        );
        assert!(topology.links.contains(&GraphEntityLink {
            from: component_id,
            to: GraphEntityId::Resource(resource("source")),
            kind: GraphEntityLinkKind::UsesResource,
        }));
        let pipe = inspected_pipe(
            &topology,
            &edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
        );
        assert_eq!(pipe.binding(), BindingState::Bound);
        assert_eq!(pipe.availability(), DataAvailability::Idle);
        assert_eq!(
            pipe.generation(),
            Some(
                control.observed().relationships[&edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID")
                )]
                    .generation
            )
        );
        assert_eq!(
            pipe.resources,
            BTreeMap::from([(resource("store"), ResourceRole::StateStore)])
        );
        assert!(topology.links.contains(&GraphEntityLink {
            from: GraphEntityId::Pipe(edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID")
            )),
            to: GraphEntityId::Resource(resource("store")),
            kind: GraphEntityLinkKind::UsesResource,
        }));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
    let topology = inspector.topology();
    let GraphEntity::Resource(store) = &topology.nodes[&GraphEntityId::Resource(resource("store"))]
    else {
        panic!("store");
    };
    assert_eq!(
        store.observed.as_ref().expect("observation").realization,
        ResourceRealization::Released
    );
}

#[tokio::test]
async fn plugin_versions_group_dependents_and_remove_the_last_derived_node() {
    let first_version = plugin("shared-plugin", "1");
    let other_version = plugin("shared-plugin", "2");
    let first = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c3")
            .expect("fixture UUID"),
        Some(first_version.clone()),
    );
    let other = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000029a")
            .expect("fixture UUID"),
        Some(other_version.clone()),
    );
    let mut graph = ComputationGraph::builder("plugins")
        .fixture_component(first.clone(), factory(&first, false))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000300")
                .expect("fixture UUID"),
            Some(first_version.clone()),
        ))
        .fixture_component(other.clone(), factory(&other, false))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002dc")
                .expect("fixture UUID"),
            None,
        ))
        .build()
        .expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deploy");
        let topology = inspector.topology();
        assert_eq!(
            inspected_plugin(&topology, &first_version).dependent_component_count(),
            2
        );
        assert_eq!(
            inspected_plugin(&topology, &other_version).dependent_component_count(),
            1
        );
        let plugin_id = GraphEntityId::Plugin(first_version.clone());
        let family_id = GraphEntityId::PluginFamily(first_version.id.clone());
        assert_eq!(topology.dependents(&plugin_id).count(), 2);
        let family = inspected_family(&topology, "shared-plugin");
        assert_eq!(family.id.as_ref(), "shared-plugin");
        assert_eq!(
            family.versions,
            BTreeSet::from([first_version.clone(), other_version.clone()])
        );
        assert_eq!(
            family.dependent_components,
            BTreeSet::from([component(451), component(768), component(666),])
        );
        assert_eq!(family.version_count(), 2);
        assert_eq!(family.dependent_component_count(), 3);
        assert_eq!(kind_count(&topology, GraphEntityKind::PluginFamily), 1);
        assert_eq!(topology.dependents(&family_id).count(), 2);
        for version in [&first_version, &other_version] {
            let version_id = GraphEntityId::Plugin(version.clone());
            assert!(topology.links.contains(&GraphEntityLink {
                from: version_id.clone(),
                to: family_id.clone(),
                kind: GraphEntityLinkKind::VersionOfPlugin,
            }));
            assert_eq!(topology.dependencies(&version_id).count(), 1);
        }
        assert_eq!(
            topology
                .dependencies(&GraphEntityId::Component(component(732)))
                .count(),
            0
        );
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(label_count(&elements, "ComputationPlugin"), 2);
        assert_eq!(label_count(&elements, "ComputationPluginFamily"), 1);
        assert_eq!(label_count(&elements, "VERSION_OF"), 2);
        assert_eq!(label_count(&elements, "USES_PLUGIN"), 3);
        assert_eq!(
            elements[&family_id.key()].get_property("id"),
            &ElementValue::String(Arc::from("shared-plugin"))
        );
        assert_eq!(
            elements[&family_id.key()].get_property("versionCount"),
            &ElementValue::Integer(2)
        );
        assert_eq!(
            elements[&family_id.key()].get_property("dependentComponentCount"),
            &ElementValue::Integer(3)
        );
        for version in [&first_version, &other_version] {
            assert!(elements.values().any(|element| matches!(
                element, Element::Relation { in_node, out_node, .. }
                    if has_label(element, "VERSION_OF")
                        && in_node.element_id.as_ref() == GraphEntityId::Plugin(version.clone()).key()
                        && out_node.element_id.as_ref() == family_id.key()
            )));
        }
        for (id, remaining) in [("first", 1), ("second", 0)] {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![named_component(id)]),
                        policy: RemovalPolicy::Reject,
                    }],
                )
                .await
                .expect("preview removal");
            control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("remove")
                .ensure_committed()
                .expect("desired topology committed");
            let topology = inspector.topology();
            assert_eq!(
                inspected_plugin(&topology, &other_version).dependent_component_count(),
                1
            );
            let changes = drain(&mut source).await;
            apply(&mut elements, &changes);
            let family = inspected_family(&topology, "shared-plugin");
            let versions = if remaining == 0 { 1 } else { 2 };
            assert_eq!(family.version_count(), versions);
            assert_eq!(family.dependent_component_count(), remaining + 1);
            assert_eq!(topology.dependents(&family_id).count(), versions);
            assert_eq!(label_count(&elements, "ComputationPluginFamily"), 1);
            assert_eq!(label_count(&elements, "VERSION_OF"), versions);
            assert_eq!(
                elements[&family_id.key()].get_property("versionCount"),
                &ElementValue::Integer(versions as i64)
            );
            assert_eq!(
                elements[&family_id.key()].get_property("dependentComponentCount"),
                &ElementValue::Integer((remaining + 1) as i64)
            );
            assert!(changes.iter().any(|change| matches!(
                change, SourceChange::Update { element, .. } if element.get_reference().element_id.as_ref() == family_id.key()
            )));
            if remaining == 0 {
                assert!(!topology.nodes.contains_key(&plugin_id));
                assert_eq!(topology.dependents(&plugin_id).count(), 0);
                assert!(!elements.contains_key(&plugin_id.key()));
                assert!(changes.iter().any(|change| matches!(
                    change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == plugin_id.key()
                )));
            } else {
                assert_eq!(
                    inspected_plugin(&topology, &first_version).dependent_component_count(),
                    remaining
                );
                assert_eq!(
                    elements[&plugin_id.key()].get_property("dependentComponentCount"),
                    &ElementValue::Integer(1)
                );
            }
        }
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![component(666)]),
                    policy: RemovalPolicy::Reject,
                }],
            )
            .await
            .expect("preview last version removal");
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("remove last version")
            .ensure_committed()
            .expect("desired topology committed");
        let topology = inspector.topology();
        assert_eq!(kind_count(&topology, GraphEntityKind::Plugin), 0);
        assert_eq!(kind_count(&topology, GraphEntityKind::PluginFamily), 0);
        assert_eq!(topology.dependents(&family_id).count(), 0);
        let changes = drain(&mut source).await;
        apply(&mut elements, &changes);
        assert_eq!(label_count(&elements, "ComputationPlugin"), 0);
        assert_eq!(label_count(&elements, "ComputationPluginFamily"), 0);
        assert_eq!(label_count(&elements, "VERSION_OF"), 0);
        assert!(changes.iter().any(|change| matches!(
            change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == family_id.key()
        )));
        assert!(topology
            .nodes
            .contains_key(&GraphEntityId::Component(component(732))));
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn plugin_family_inventory_traverses_all_versions_without_aliasing_component_ids() {
    let first_version = plugin("postgres", "1.2.0");
    let second_version = plugin("postgres", "1.3.0");
    let unrelated = plugin("postgres-extra", "1.2.0");
    let left = ComponentBatch::builder()
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003ff")
                .expect("fixture UUID"),
            Some(first_version.clone()),
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000400")
                .expect("fixture UUID"),
            Some(second_version.clone()),
        ))
        .build()
        .expect("left graph");
    let right = ComponentBatch::builder()
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e6")
                .expect("fixture UUID"),
            Some(second_version.clone()),
        ))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000369")
                .expect("fixture UUID"),
            Some(unrelated),
        ))
        .build()
        .expect("right graph");
    let instance = drasi_lib::DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_components(left.auto_start(false))
        .with_components(right.auto_start(false))
        .build()
        .await
        .expect("instance");
    let inventory = instance
        .inspect_computation_inventory()
        .await
        .expect("inventory");
    let control = instance.computation_control().unwrap();
    let root = ComputationScope::root(control.desired_snapshot().id.clone());
    let right_component = root.entity(GraphEntityId::Component(component(742)));
    assert_eq!(
        inventory.plugin_family_dependents("postgres"),
        BTreeSet::from([
            root.entity(GraphEntityId::Component(component(1023))),
            root.entity(GraphEntityId::Component(component(1024))),
            right_component.clone(),
        ])
    );
    assert_eq!(inventory.plugin_dependents(&first_version).len(), 1);
    assert_eq!(inventory.plugin_dependents(&second_version).len(), 2);
    assert_eq!(
        inventory.plugin_family_dependents("postgres-extra").len(),
        1
    );
    assert!(inventory.plugin_family_dependents("unknown").is_empty());
    assert_eq!(
        inspected_family(&inventory.scopes[&root].topology, "postgres").version_count(),
        2
    );
    assert_eq!(
        inventory
            .scopes
            .values()
            .filter(|scope| scope.owner.is_none())
            .count(),
        1
    );
    let family_id = GraphEntityId::PluginFamily(Arc::from("postgres"));
    assert_eq!(inventory.dependents(&root.entity(family_id)).count(), 2);
    let mut desired = instance.desired_configuration().unwrap();
    desired.desired.topology.components.retain(|node| {
        node.descriptor.id() != &component(1023) && node.descriptor.id() != &component(1024)
    });
    instance
        .apply_desired_state(
            desired.revision,
            "remove-left-plugin-version",
            desired.desired,
        )
        .await
        .unwrap();
    let status = instance.reconcile_desired_state().await.unwrap();
    assert!(status.applied && status.error.is_none(), "{status:?}");
    let current = instance
        .inspect_computation_inventory()
        .await
        .expect("updated inventory");
    assert_eq!(
        current.plugin_family_dependents("postgres"),
        BTreeSet::from([right_component])
    );
    assert!(current.plugin_dependents(&first_version).is_empty());
    assert_eq!(
        inventory.plugin_family_dependents("postgres").len(),
        3,
        "previous view is immutable"
    );
    instance.shutdown().await.expect("shutdown");
}

#[test]
fn pipe_observations_preserve_generations_and_original_failure_references() {
    let graph = pipeline().build().expect("graph");
    let mut inspection = graph.inspector().snapshot().as_ref().clone();
    let definition = edge(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
    );
    let cause = Arc::new(GraphError::Topology {
        reason: "local failure only".into(),
    });
    for (index, binding) in [
        BindingState::Declared,
        BindingState::Binding,
        BindingState::Bound,
        BindingState::Failed,
        BindingState::Draining,
    ]
    .into_iter()
    .enumerate()
    {
        let observed = Arc::make_mut(&mut inspection.observed)
            .relationships
            .get_mut(&definition)
            .expect("observation");
        observed.binding = binding;
        observed.availability = DataAvailability::Unavailable;
        observed.generation = index as u64 + 10;
        observed.failure = Some(ComponentFailure {
            phase: FailurePhase::Binding,
            disposition: FailureDisposition::Retryable,
            cause: cause.clone(),
            timestamp: Utc::now(),
        });
        let topology = inspection.topology();
        let pipe = inspected_pipe(&topology, &definition);
        assert_eq!(pipe.binding(), binding);
        assert_eq!(pipe.availability(), DataAvailability::Unavailable);
        assert_eq!(pipe.generation(), Some(index as u64 + 10));
        assert!(Arc::ptr_eq(&pipe.failure().expect("failure").cause, &cause));
        assert_eq!(kind_count(&topology, GraphEntityKind::Pipe), 1);
    }
    Arc::make_mut(&mut inspection.observed)
        .relationships
        .clear();
    let topology = inspection.topology();
    let pipe = inspected_pipe(&topology, &definition);
    assert_eq!(pipe.binding(), BindingState::Declared);
    assert_eq!(pipe.availability(), DataAvailability::Unknown);
    assert_eq!(pipe.generation(), None);
    assert!(pipe.failure().is_none());
}

#[tokio::test]
async fn pipe_nodes_survive_unbinding_and_keep_missing_endpoint_declarations() {
    let mut graph = pipeline().build().expect("graph");
    let inspector = graph.inspector();
    let definition = edge(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
    );
    let pipe_id = GraphEntityId::Pipe(definition.clone());
    assert_eq!(
        inspected_pipe(&inspector.topology(), &definition).binding(),
        BindingState::Declared
    );
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deploy");
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(label_count(&elements, "ComputationPipe"), 1);
        assert_eq!(label_count(&elements, "ComputationResource"), 1);
        assert_eq!(label_count(&elements, "FLOWS_TO"), 1);
        for (label, port_name) in [("INPUT_TO_PIPE", "out"), ("OUTPUT_FROM_PIPE", "in")] {
            let links: Vec<_> = elements
                .values()
                .filter(|element| has_label(element, label))
                .collect();
            assert_eq!(links.len(), 1);
            assert_eq!(
                links[0].get_property("port"),
                &ElementValue::String(Arc::from(port_name))
            );
        }
        let flow = elements
            .values()
            .find(|element| has_label(element, "FLOWS_TO"))
            .expect("legacy flow");
        assert_eq!(
            flow.get_property("pipeId"),
            &ElementValue::String(Arc::from(pipe_id.key()))
        );
        assert_eq!(
            flow.get_property("representation"),
            &ElementValue::String(Arc::from("pipeSummary"))
        );
        assert!(!format!("{elements:?}").contains("private-store-binding-marker"));
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::Unbind {
                    edge: definition.clone(),
                    policy: RemovalPolicy::Orphan,
                }],
            )
            .await
            .expect("preview unbind");
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("unbind")
            .ensure_committed()
            .expect("desired topology committed");
        let topology = inspector.topology();
        let pipe = inspected_pipe(&topology, &definition);
        assert!(pipe.declared_only);
        assert_eq!(pipe.binding(), BindingState::Declared);
        assert!(pipe.capabilities.is_none());
        assert_eq!(
            pipe.resources,
            BTreeMap::from([(resource("store"), ResourceRole::StateStore)])
        );
        assert_eq!(topology.dependencies(&pipe_id).count(), 2);
        assert!(topology.dependencies(&pipe_id).any(|link| {
            link.kind == GraphEntityLinkKind::DependsOnData
                && link.to == GraphEntityId::Component(component(77))
        }));
        let changes = drain(&mut source).await;
        assert!(changes.iter().any(|change| matches!(
            change, SourceChange::Update { element, .. } if element.get_reference().element_id.as_ref() == pipe_id.key()
        )));
        assert!(!changes.iter().any(|change| matches!(
            change, SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == pipe_id.key()
        )));
        apply(&mut elements, &changes);
        assert_eq!(label_count(&elements, "ComputationPipe"), 1);
        assert_eq!(label_count(&elements, "FLOWS_TO"), 0);
        assert_eq!(label_count(&elements, "ComputationRelationship"), 1);

        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![component(77)]),
                    policy: RemovalPolicy::Orphan,
                }],
            )
            .await
            .expect("preview producer removal");
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("remove producer")
            .ensure_committed()
            .expect("desired topology committed");
        let topology = inspector.topology();
        assert!(topology.nodes.contains_key(&pipe_id));
        let producer = GraphEntityId::Component(component(77));
        assert!(!topology.nodes.contains_key(&producer));
        assert!(topology.links.contains(&GraphEntityLink {
            from: producer,
            to: pipe_id.clone(),
            kind: GraphEntityLinkKind::PipeInput { port: port("out") },
        }));
        apply(&mut elements, &drain(&mut source).await);
        assert_eq!(label_count(&elements, "ComputationPipe"), 1);
        assert_eq!(label_count(&elements, "INPUT_TO_PIPE"), 0);
        assert_eq!(label_count(&elements, "OUTPUT_FROM_PIPE"), 1);
        assert_eq!(
            elements[&pipe_id.key()].get_property("fromComponent"),
            &ElementValue::String(Arc::from(component(77).as_str()))
        );
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("dispose");
}

struct SecretResolver;

#[async_trait]
impl ConfigurationResolver for SecretResolver {
    fn validate_reference(&self, key: &str) -> anyhow::Result<()> {
        anyhow::ensure!(key == "secret-reference-marker", "unexpected key");
        Ok(())
    }

    async fn resolve(&self, key: &str) -> anyhow::Result<serde_json::Value> {
        self.validate_reference(key)?;
        Ok(serde_json::json!("resolved-secret-marker"))
    }
}

#[tokio::test]
async fn graph_as_data_exposes_provenance_but_not_configuration_secrets_or_failures() {
    let mut spec = specification(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000359")
            .expect("fixture UUID"),
        Some(plugin("declared-plugin", "5")),
    );
    spec.configuration.insert(
        Arc::from("configuration-field-marker"),
        ConfigurationValue::Literal(serde_json::json!("configuration-value-marker")),
    );
    spec.configuration.insert(
        Arc::from("secret-field-marker"),
        ConfigurationValue::Reference {
            resource: resource("secrets"),
            key: Arc::from("secret-reference-marker"),
            secret: true,
        },
    );
    let mut graph = ComputationGraph::builder("redacted")
        .fixture_component(spec.clone(), factory(&spec, true))
        .fixture_service(service(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002dc")
                .expect("fixture UUID"),
            None,
        ))
        .declare_resource(ResourceSpecification {
            id: resource("secrets"),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("resource-binding-marker"),
        })
        .expect("declare resolver")
        .provide_resource(
            resource("secrets"),
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(SecretResolver))),
            ),
        )
        .expect("provide resolver")
        .build()
        .expect("graph");
    let inspector = graph.inspector();
    let run = graph.run().expect("run");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deploy");
        let topology = inspector.topology();
        let component_id = GraphEntityId::Component(component(857));
        let GraphEntity::Component(failed_component) = &topology.nodes[&component_id] else {
            panic!("component");
        };
        assert!(failed_component
            .observed
            .as_ref()
            .expect("observation")
            .failure
            .as_ref()
            .expect("failure")
            .cause
            .to_string()
            .contains("private-failure-marker"));
        assert!(topology.links.contains(&GraphEntityLink {
            from: component_id.clone(),
            to: GraphEntityId::Resource(resource("secrets")),
            kind: GraphEntityLinkKind::UsesResource,
        }));
        let mut source =
            ComputationTopologySource::new(component(733), stream("topology"), inspector.clone());
        source.start().await.expect("start topology source");
        let mut elements = BTreeMap::new();
        apply(&mut elements, &drain(&mut source).await);
        for marker in [
            "configuration-field-marker",
            "configuration-value-marker",
            "secret-field-marker",
            "secret-reference-marker",
            "resolved-secret-marker",
            "private-failure-marker",
            "resource-binding-marker",
        ] {
            assert!(!format!("{elements:?}").contains(marker), "leaked {marker}");
        }
        let node = &elements[&component_id.key()];
        assert_eq!(
            node.get_property("construction"),
            &ElementValue::String(Arc::from("Factory"))
        );
        assert_eq!(
            node.get_property("implementationVersion"),
            &ElementValue::String(Arc::from("7"))
        );
        assert_eq!(
            node.get_property("configurationVersion"),
            &ElementValue::Integer(3)
        );
        assert_eq!(
            node.get_property("pluginVersion"),
            &ElementValue::String(Arc::from("5"))
        );
        assert_eq!(
            node.get_property("failurePhase"),
            &ElementValue::String(Arc::from("Creation"))
        );
        assert_eq!(label_count(&elements, "ComputationPlugin"), 1);
        source.stop().await.expect("stop topology source");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}
