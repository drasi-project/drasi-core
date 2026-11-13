// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "support/mod.rs"]
mod fixture_support;

use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementReference, ElementValue, SourceChange};
use drasi_lib::computation::v1::*;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;

const A: &str = "00000000-0000-4000-8000-000000000001";
const B: &str = "00000000-0000-4000-8000-000000000002";
const C: &str = "00000000-0000-4000-8000-000000000003";
const D: &str = "00000000-0000-4000-8000-000000000004";
const E: &str = "00000000-0000-4000-8000-000000000005";
const OBSERVER: &str = "00000000-0000-4000-8000-000000000006";

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component identity fixture")
}

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).expect("component identity fixture")
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("component identity fixture")
}

fn stream() -> StreamId {
    StreamId::try_new("orders-stream").expect("component identity fixture")
}

struct Fixture {
    descriptor: ComponentDescriptor,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    fail: bool,
    input: Option<mpsc::Receiver<OutputEnvelope>>,
    output: Option<mpsc::UnboundedSender<(u64, u64)>>,
    processed: u64,
}

impl Fixture {
    fn new(uuid: &str, kind: ComponentSemanticKind, direction: Option<PortDirection>) -> Self {
        let ports = direction
            .map(|direction| {
                vec![PortDescriptor::new(
                    port(if direction == PortDirection::Input {
                        "in"
                    } else {
                        "out"
                    }),
                    direction,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )]
            })
            .unwrap_or_default();
        Self {
            descriptor: ComponentDescriptor::try_new(id(uuid), ports)
                .expect("component identity fixture")
                .with_semantic_kind(kind),
            starts: Arc::default(),
            stops: Arc::default(),
            fail: false,
            input: None,
            output: None,
            processed: 0,
        }
    }
}

#[async_trait]
impl ComputationComponent for Fixture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"secret": "must-not-appear-in-conflicts"}))
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail, "private-failure-details");
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Fixture {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[async_trait]
impl EnvelopeSource for Fixture {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self
            .input
            .as_mut()
            .expect("component identity fixture")
            .recv()
            .await)
    }
}

#[async_trait]
impl EnvelopeSink for Fixture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.processed += 1;
        self.output
            .as_ref()
            .expect("component identity fixture")
            .send((input.envelope.system().sequence(), self.processed))?;
        Ok(())
    }
}

fn service(uuid: &str, label: &str, kind: ComponentSemanticKind) -> ComponentAddition {
    ComponentAddition::new(
        name(label),
        ConstructedComponent::service(Box::new(Fixture::new(uuid, kind, None))),
    )
    .auto_start(false)
}

fn rejected(error: &GraphError) -> &GraphError {
    match error {
        GraphError::AdditionRejected { cause, .. } => cause,
        other => other,
    }
}

fn specification(uuid: &str) -> ComponentSpecification {
    ComponentSpecification {
        descriptor: Fixture::new(uuid, ComponentSemanticKind::Service, None).descriptor,
        role: ComponentRole::Service,
        completion: None,
        implementation: ImplementationIdentity::try_new("identity/service", "7")
            .expect("component identity fixture"),
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::new(),
    }
}

struct Factory(FactoryDescriptor);

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let mut fixture = Fixture::new(A, ComponentSemanticKind::Service, None);
        fixture.descriptor = context.specification.descriptor.clone();
        Ok(ConstructedComponent::service(Box::new(fixture)))
    }
}

#[test]
fn named_topology_and_configuration_roundtrip_without_changing_execution_descriptors() {
    let spec = specification(A);
    let factory: Arc<dyn ComponentFactory> = Arc::new(Factory(FactoryDescriptor {
        implementation: spec.implementation.clone(),
        role: spec.role,
        configuration_version: spec.configuration_version,
        configuration: ConfigurationSchema {
            fields: BTreeMap::new(),
            allow_additional: false,
        },
        dependencies: BTreeMap::new(),
    }));
    let graph = ComputationGraph::builder("names")
        .component(name("orders"), spec.clone(), factory.clone())
        .build()
        .expect("component identity fixture");
    let snapshot = graph
        .configuration_snapshot()
        .expect("component identity fixture");
    let encoded = serde_json::to_string(&snapshot).expect("component identity fixture");
    let decoded: GraphConfigurationSnapshot =
        serde_json::from_str(&encoded).expect("component identity fixture");
    let desired = decoded
        .reconstructible_topology()
        .expect("component identity fixture");
    assert_eq!(desired.version, DESIRED_TOPOLOGY_VERSION);
    assert_eq!(desired.components[0].name, name("orders"));
    assert_eq!(desired.components[0].descriptor.id(), &id(A));
    assert_eq!(
        desired,
        DesiredTopology::from_json(&desired.to_json().expect("component identity fixture"))
            .expect("component identity fixture")
    );

    let mut bindings = TopologyBindings::default();
    bindings
        .factories
        .register(factory)
        .expect("component identity fixture");
    let restored = desired.build(bindings).expect("component identity fixture");
    assert_eq!(
        restored
            .snapshot()
            .component_by_id(&id(A))
            .expect("component identity fixture")
            .name,
        name("orders")
    );
    assert_eq!(restored.snapshot().specifications[&id(A)], spec);
    assert_eq!(
        restored
            .snapshot()
            .select(GraphSelection::Exact(vec![id(A)]))
            .expect("component identity fixture"),
        desired
    );

    let mut invalid = desired.clone();
    invalid.version = 1;
    assert!(invalid.validate_structure().is_err());
    assert!(
        DesiredTopology::from_json(&invalid.to_json().expect("component identity fixture"))
            .is_err()
    );
    let mut missing_name = serde_json::to_value(&desired).expect("component identity fixture");
    missing_name["components"][0]
        .as_object_mut()
        .expect("component identity fixture")
        .remove("name");
    assert!(serde_json::from_value::<DesiredTopology>(missing_name).is_err());
    let mut duplicate_name = desired.clone();
    let mut duplicate = desired.components[0].clone();
    duplicate.descriptor = specification(B).descriptor;
    duplicate.construction = ComponentConstruction::Factory(specification(B));
    duplicate_name.components.push(duplicate);
    assert!(matches!(
        duplicate_name.validate_structure(),
        Err(GraphError::DuplicateComponentName { existing }) if existing.id == id(A)
    ));

    let duplicate_id = ComputationGraph::builder("duplicates")
        .service(
            name("source"),
            Box::new(Fixture::new(A, ComponentSemanticKind::Source, None)),
        )
        .service(
            name("reaction"),
            Box::new(Fixture::new(A, ComponentSemanticKind::Reaction, None)),
        )
        .build();
    assert!(matches!(
        duplicate_id,
        Err(GraphError::DuplicateComponentId { existing })
            if existing.id == id(A) && existing.name == name("source")
    ));
}

#[tokio::test]
async fn conflicts_reserve_stopped_waiting_and_failed_names_and_fence_old_handles() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut graph = ComputationGraph::empty(ComponentBatch::GRAPH_ID).expect("component identity fixture");
        let run = graph.run().expect("component identity fixture");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.expect("component identity fixture");
            let original = control.add_component(service(A, "orders", ComponentSemanticKind::Source)).await.expect("component identity fixture");
            original.wait_created().await.expect("component identity fixture");
            for (uuid, label, same_id) in [(A, "other", true), (B, "orders", false)] {
                let error = control
                    .add_component(service(uuid, label, ComponentSemanticKind::Reaction))
                    .await
                    .expect_err("component identity fixture");
                let existing = match (same_id, rejected(&error)) {
                    (true, GraphError::DuplicateComponentId { existing })
                    | (false, GraphError::DuplicateComponentName { existing }) => existing,
                    _ => panic!("unexpected conflict: {error:?}"),
                };
                assert_eq!(existing.id, id(A));
                assert_eq!(existing.name, name("orders"));
                assert_eq!(existing.kind, ComponentSemanticKind::Source);
                assert_eq!(existing.lifecycle, Some(ComponentLifecycle::Stopped));
                assert!(!format!("{existing:?}").contains("must-not-appear"));
                let serialized = serde_json::to_value(existing).expect("component identity fixture");
                assert_eq!(serialized["id"], A);
                assert_eq!(serialized["name"], "orders");
                assert!(serialized.get("kind").is_some());
                assert!(serialized.get("configuration").is_none());
                assert!(!serialized.to_string().contains("must-not-appear"));
            }
            let batch = ComponentBatch::builder()
                .service(name("orders"), Box::new(Fixture::new(B, ComponentSemanticKind::Reaction, None)))
                .build()
                .expect("component identity fixture");
            let error = control.preview(
                control.desired_snapshot().revision,
                batch.definition.components.into_iter().map(DesiredMutation::PutComponent).collect(),
            ).await.expect_err("component identity fixture");
            assert!(matches!(error, GraphError::DuplicateComponentName { existing } if existing.id == id(A)));

            control.add_component(ComponentAddition::declaration(name("waiting"), specification(C))).await.expect("component identity fixture");
            let error = control.add_component(service(D, "waiting", ComponentSemanticKind::Reaction)).await.expect_err("component identity fixture");
            assert!(matches!(
                rejected(&error),
                GraphError::DuplicateComponentName { existing }
                    if existing.id == id(C) && existing.implementation.as_ref().expect("component identity fixture").version.as_ref() == "7"
            ));

            let mut failed = Fixture::new(D, ComponentSemanticKind::Service, None);
            failed.fail = true;
            let failed = control.add_component(ComponentAddition::new(name("failed"), ConstructedComponent::service(Box::new(failed)))).await.expect("component identity fixture");
            assert!(failed.wait_started().await.is_err());
            let error = control.add_component(service(E, "failed", ComponentSemanticKind::Reaction)).await.expect_err("component identity fixture");
            assert!(matches!(rejected(&error), GraphError::DuplicateComponentName { existing } if existing.id == id(D)));
            assert!(!format!("{:?}", rejected(&error)).contains("private-failure-details"));

            let revision = control.desired_snapshot().revision;
            assert!(matches!(
                original.rename(name("waiting")).await,
                Err(GraphError::DuplicateComponentName { .. })
            ));
            assert_eq!(control.desired_snapshot().revision, revision);
            let preview = control.preview(revision, vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![id(A)]),
                policy: RemovalPolicy::Reject,
            }]).await.expect("component identity fixture");
            assert!(control.reconcile(preview, TopologyBindings::default()).await.expect("component identity fixture").committed);
            let replacement = control.add_component(service(A, "orders", ComponentSemanticKind::Reaction)).await.expect("component identity fixture");
            assert_ne!(original.generation(), replacement.generation());
            assert!(matches!(original.rename(name("stale")).await, Err(GraphError::StaleGeneration)));
            assert_eq!(control.component_handle_by_name(&name("orders")).expect("component identity fixture").generation(), replacement.generation());
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }).await.expect("component identity fixture");
}

#[tokio::test]
async fn concurrent_add_and_rename_have_one_owner_per_name() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut graph =
            ComputationGraph::empty("concurrent-names").expect("component identity fixture");
        let run = graph.run().expect("component identity fixture");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control
                .deployment_report()
                .await
                .expect("component identity fixture");
            let (left, right) = tokio::join!(
                control.add_component(service(A, "unique", ComponentSemanticKind::Source)),
                control.add_component(service(B, "unique", ComponentSemanticKind::Reaction)),
            );
            assert_ne!(left.is_ok(), right.is_ok());
            let winner = left.or(right).expect("component identity fixture");
            assert_eq!(
                control
                    .component_handle_by_name(&name("unique"))
                    .expect("component identity fixture")
                    .id(),
                winner.id()
            );
            let other = control
                .add_component(service(C, "other", ComponentSemanticKind::Service))
                .await
                .expect("component identity fixture");
            let (left, right) = tokio::join!(
                winner.rename(name("claimed")),
                other.rename(name("claimed"))
            );
            assert_ne!(left.is_ok(), right.is_ok());
            let expected = if left.is_ok() {
                winner.id()
            } else {
                other.id()
            };
            assert_eq!(
                control
                    .component_handle_by_name(&name("claimed"))
                    .expect("component identity fixture")
                    .id(),
                expected
            );
            let desired = control
                .configuration_snapshot()
                .expect("component identity fixture")
                .topology;
            desired
                .validate_structure()
                .expect("component identity fixture");
            assert_eq!(desired.components.len(), 2);
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    })
    .await
    .expect("component identity fixture");
}

async fn projection(source: &mut ComputationTopologySource) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(20), source.next()).await {
        changes.extend(
            GraphChangeCodec::decode_changes(
                &event
                    .expect("component identity fixture")
                    .expect("component identity fixture")
                    .envelope,
            )
            .expect("component identity fixture"),
        );
    }
    changes
}

#[tokio::test]
async fn running_and_stopped_renames_preserve_object_wiring_progress_and_node_uuid() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (input, receiver) = mpsc::channel(2);
        let (output, mut results) = mpsc::unbounded_channel();
        let mut source = Fixture::new(A, ComponentSemanticKind::Source, Some(PortDirection::Output));
        source.input = Some(receiver);
        let source_starts = source.starts.clone();
        let source_stops = source.stops.clone();
        let mut sink = Fixture::new(B, ComponentSemanticKind::Reaction, Some(PortDirection::Input));
        sink.output = Some(output);
        let sink_starts = sink.starts.clone();
        let sink_stops = sink.stops.clone();
        let edge = EdgeDefinition::new(Endpoint::new(id(A), port("out")), Endpoint::new(id(B), port("in")));
        let mut graph = ComputationGraph::builder("live-rename")
            .source(name("orders"), Box::new(source))
            .sink(name("totals"), Box::new(sink))
            .bind_stream(Endpoint::new(id(A), port("out")), stream())
            .connect(edge.clone(), Box::new(BoundedPipeConfig { capacity: 2 }))
            .build().expect("component identity fixture");
        let mut topology_source = ComputationTopologySource::new(id(OBSERVER), StreamId::try_new("topology").expect("component identity fixture"), graph.inspector());
        let run = graph.start().expect("component identity fixture");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.expect("component identity fixture");
            let source = control.component_handle_by_name(&name("orders")).expect("component identity fixture");
            let sink = control.component_handle_by_id(&id(B)).expect("component identity fixture");
            source.wait_started().await.expect("component identity fixture");
            sink.wait_started().await.expect("component identity fixture");
            topology_source.start().await.expect("component identity fixture");
            let before_projection = projection(&mut topology_source).await;
            let before = control.desired_snapshot();
            let binding = control.observed().relationships[&edge].generation;
            let generation = source.generation();

            for sequence in 1..=2 {
                input.send(OutputEnvelope {
                    port: port("out"),
                    envelope: GraphChangeCodec::encode_change(SourceChange::Delete {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("business-records", "order"),
                            labels: Arc::from([]),
                            effective_from: sequence,
                        },
                    }, stream(), sequence, None).expect("component identity fixture"),
                }).await.expect("component identity fixture");
                assert_eq!(results.recv().await.expect("component identity fixture"), (sequence, sequence));
                if sequence == 1 {
                    control.rename_component(&id(A), name("purchases")).await.expect("component identity fixture");
                    let preview = control.preview(control.desired_snapshot().revision, vec![DesiredMutation::RenameComponent {
                        component: id(B), name: name("purchase-totals"),
                    }]).await.expect("component identity fixture");
                    assert!(preview.created().is_empty() && preview.replaced().is_empty());
                    assert!(preview.updated().is_empty() && preview.restarted().is_empty() && preview.paused().is_empty());
                    assert!(control.reconcile(preview, TopologyBindings::default()).await.expect("component identity fixture").committed);
                }
            }
            assert_eq!(source.generation(), generation);
            assert_eq!(source.observed().expect("component identity fixture").lifecycle, ComponentLifecycle::Running);
            assert_eq!(source_starts.load(Ordering::SeqCst), 1);
            assert_eq!(sink_starts.load(Ordering::SeqCst), 1);
            assert_eq!(source_stops.load(Ordering::SeqCst), 0);
            assert_eq!(sink_stops.load(Ordering::SeqCst), 0);
            assert_eq!(control.observed().relationships[&edge].generation, binding);
            let after = control.desired_snapshot();
            assert_eq!(after.component_by_id(&id(A)).expect("component identity fixture").descriptor, before.component_by_id(&id(A)).expect("component identity fixture").descriptor);
            assert_eq!(after.component_by_id(&id(A)).expect("component identity fixture").output_streams, before.component_by_id(&id(A)).expect("component identity fixture").output_streams);
            assert_eq!(after.edges[0].definition, edge);
            assert!(control.component_handle_by_name(&name("orders")).is_err());
            let keeper = control.add_component(service(C, "orders", ComponentSemanticKind::Service)).await.expect("component identity fixture");
            keeper.wait_created().await.expect("component identity fixture");
            keeper.start().await.expect("component identity fixture");
            assert_eq!(control.component_handle_by_name(&name("orders")).expect("component identity fixture").id(), &id(C));
            assert_eq!(source.id(), &id(A));
            assert_eq!(control.desired_snapshot().edges[0].definition.from.component, id(A));

            for uuid in [A, B] {
                assert_eq!(GraphEntityId::Component(id(uuid)).key(), uuid);
                assert!(before_projection.iter().any(|change| matches!(change,
                    SourceChange::Insert { element: Element::Node { metadata, .. } }
                    if metadata.reference.element_id.as_ref() == uuid)));
            }
            assert!(before_projection.iter().any(|change| matches!(change,
                SourceChange::Insert { element: Element::Relation { metadata, in_node, out_node, .. } }
                if metadata.labels.iter().any(|label| label.as_ref() == "FLOWS_TO")
                    && in_node.element_id.as_ref() == A && out_node.element_id.as_ref() == B)));
            let changes = projection(&mut topology_source).await;
            assert!(changes.iter().any(|change| matches!(change,
                SourceChange::Update { element: Element::Node { metadata, properties }, .. }
                if metadata.reference.element_id.as_ref() == A
                    && properties.get("name") == Some(&ElementValue::String(Arc::from("purchases"))))));
            assert!(!changes.iter().any(|change| matches!(change,
                SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == A)));
            assert!(GraphEntityId::Resource(ResourceId::try_new(A).expect("component identity fixture")).key().starts_with("resource:"));

            source.stop().await.expect("component identity fixture");
            sink.stop().await.expect("component identity fixture");
            let stopped_starts = source_starts.load(Ordering::SeqCst);
            let stopped_stops = source_stops.load(Ordering::SeqCst);
            source.rename(name("archived-orders")).await.expect("component identity fixture");
            assert_eq!(source.observed().expect("component identity fixture").lifecycle, ComponentLifecycle::Stopped);
            assert_eq!(source_starts.load(Ordering::SeqCst), stopped_starts);
            assert_eq!(source_stops.load(Ordering::SeqCst), stopped_stops);
            let mut desired = control.configuration_snapshot().expect("component identity fixture").topology;
            desired.components.iter_mut().find(|node| node.descriptor.id() == &id(A)).expect("component identity fixture").name = name("saved-orders");
            let preview = control.preview(control.desired_snapshot().revision, vec![DesiredMutation::SetTopology(desired)]).await.expect("component identity fixture");
            assert!(preview.replaced().is_empty() && preview.restarted().is_empty() && preview.paused().is_empty());
            assert!(control.reconcile(preview, TopologyBindings::default()).await.expect("component identity fixture").committed);
            assert_eq!(source.observed().expect("component identity fixture").lifecycle, ComponentLifecycle::Stopped);
            assert_eq!(source.generation(), generation);
            assert_eq!(source_starts.load(Ordering::SeqCst), stopped_starts);
            let snapshot = control.configuration_snapshot().expect("component identity fixture");
            let decoded: GraphConfigurationSnapshot = serde_json::from_str(&serde_json::to_string(&snapshot).expect("component identity fixture")).expect("component identity fixture");
            assert_eq!(decoded.topology, snapshot.topology);
            assert_eq!(decoded.topology.components.iter().find(|node| node.descriptor.id() == &id(A)).expect("component identity fixture").name, name("saved-orders"));
            topology_source.stop().await.expect("component identity fixture");
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }).await.expect("component identity fixture");
}
