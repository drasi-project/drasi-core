// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bootstrap providers declare their pairing protocol, element namespace,
//! element identity and completion; sources declare their coordination offer
//! and identities, and consumers what initialization they accept. The graph
//! compares them when a bootstrap edge binds: a mismatch leaves both nodes in
//! the graph, fails and unbinds the edge, and records the terminal binding
//! failure on the consumer, before any session starts.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use drasi_lib::computation::v1::*;

fn id(value: u128) -> ComponentId {
    fixture_support::component_id(value)
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn endpoint(node: u128, name: &str) -> Endpoint {
    Endpoint::new(id(node), port(name))
}
fn pipe() -> Box<dyn PipeProvider> {
    Box::new(BoundedPipeConfig { capacity: 4 })
}
fn graph_port(name: &str, direction: PortDirection, bootstrap: bool) -> PortDescriptor {
    let schema = GraphChangeCodec::schema().descriptor().clone();
    if bootstrap {
        PortDescriptor::bootstrap(port(name), direction, schema, PipeRequirements::default())
    } else {
        PortDescriptor::new(port(name), direction, schema, PipeRequirements::default())
    }
}

const PROVIDER: u128 = 101;
const SOURCE: u128 = 102;
const CONSUMER: u128 = 103;

fn protocol(name: &str) -> BootstrapProtocol {
    BootstrapProtocol::new(name, 1)
}

fn identity(binding: &str) -> ElementIdentity {
    ElementIdentity {
        scheme: "drasi.test/elements".into(),
        binding: binding.into(),
    }
}

/// A provider declaring `output` on its bootstrap output. It never serves a
/// session in these tests.
fn provider(output: BootstrapOutputDescription) -> BootstrapProviderComponent {
    let factory: BootstrapProviderFactory =
        Arc::new(|_| anyhow::bail!("binding tests serve no session"));
    let component = BootstrapProviderComponent::new(id(PROVIDER), factory).expect("provider");
    let description =
        BootstrapProviderComponent::declared_description(component.descriptor(), output);
    component.with_description(description)
}

fn output(
    completion: BootstrapCompletionKind,
    pairing: Option<BootstrapProtocol>,
    namespace: ElementNamespace,
    identity: Option<ElementIdentity>,
) -> BootstrapOutputDescription {
    BootstrapOutputDescription {
        completion,
        pairing,
        namespace,
        identity,
    }
}

/// A source relaying its consumers' sessions, with the given offer and
/// identities.
struct Source {
    descriptor: ComponentDescriptor,
    description: ComponentDescription,
}

fn source(
    coordination: Option<BootstrapCoordination>,
    element_identity: Option<ElementIdentity>,
) -> Source {
    let descriptor = ComponentDescriptor::try_new(
        id(SOURCE),
        vec![
            graph_port("out", PortDirection::Output, false),
            graph_port("bootstrap", PortDirection::Input, true),
            graph_port("snapshot", PortDirection::Output, true),
        ],
    )
    .expect("source descriptor");
    let mut description = ComponentDescription::new(ProgressPersistence::Stateless)
        .with_outputs(&descriptor, OutputDescription::UNTRACKED)
        .with_bootstrap_ports(
            &descriptor,
            BootstrapInputDescription::SOURCE_RELAY,
            BootstrapOutputDescription::RELAYED,
        );
    description.coordination = coordination;
    description.element_identity = element_identity;
    Source {
        descriptor,
        description,
    }
}

#[async_trait]
impl ComputationComponent for Source {
    fn description(&self) -> ComponentDescription {
        self.description.clone()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

/// A consumer accepting `input` initialization on its bootstrap input.
struct Consumer {
    descriptor: ComponentDescriptor,
    description: ComponentDescription,
}

fn consumer(input: BootstrapInputDescription) -> Consumer {
    let descriptor = ComponentDescriptor::try_new(
        id(CONSUMER),
        vec![
            graph_port("in", PortDirection::Input, false),
            graph_port("bootstrap", PortDirection::Input, true),
        ],
    )
    .expect("consumer descriptor");
    let description = ComponentDescription::new(ProgressPersistence::Stateless)
        .with_bootstrap_ports(&descriptor, input, BootstrapOutputDescription::UNDECLARED);
    Consumer {
        descriptor,
        description,
    }
}

#[async_trait]
impl ComputationComponent for Consumer {
    fn description(&self) -> ComponentDescription {
        self.description.clone()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Consumer {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn provider_to_source() -> EdgeDefinition {
    EdgeDefinition::new(
        endpoint(PROVIDER, "snapshot"),
        endpoint(SOURCE, "bootstrap"),
    )
}

fn source_to_consumer() -> EdgeDefinition {
    EdgeDefinition::new(endpoint(SOURCE, "out"), endpoint(CONSUMER, "in"))
}

#[tokio::test(flavor = "current_thread")]
async fn shared_cut_refuses_independent_reads_but_not_an_unused_or_live_only_input() {
    let first = source_to_consumer();
    let second = EdgeDefinition::new(endpoint(SOURCE + 10, "out"), endpoint(CONSUMER, "other"));
    for (connect_second, live_only) in [(true, false), (true, true), (false, false)] {
        let desc = ComponentDescriptor::try_new(
            id(CONSUMER),
            vec![
                graph_port("in", PortDirection::Input, false),
                graph_port("bootstrap", PortDirection::Input, true).for_data_port(port("in")),
                graph_port("other", PortDirection::Input, false),
                graph_port("other-state", PortDirection::Input, true).for_data_port(port("other")),
            ],
        )
        .unwrap();
        let mut builder =
            ComputationGraph::builder("shared-cut").fixture_sink(Box::new(Consumer {
                description: ComponentDescription::undeclared()
                    .with_bootstrap_ports(
                        &desc,
                        BootstrapInputDescription::BOUNDED,
                        BootstrapOutputDescription::UNDECLARED,
                    )
                    .with_upstream_initial_state(true)
                    .with_shared_input_cut(),
                descriptor: desc,
            }));
        for (number, name) in [(SOURCE, "a"), (SOURCE + 10, "b")] {
            let desc = ComponentDescriptor::try_new(
                id(number),
                vec![
                    graph_port("out", PortDirection::Output, false),
                    graph_port("snapshot", PortDirection::Output, true).for_data_port(port("out")),
                ],
            )
            .unwrap();
            builder = builder
                .fixture_source(Box::new(Source {
                    description: ComponentDescription::undeclared().with_bootstrap_ports(
                        &desc,
                        BootstrapInputDescription::SOURCE_RELAY,
                        BootstrapOutputDescription::OUTPUT_SNAPSHOT,
                    ),
                    descriptor: desc,
                }))
                .bind_stream(endpoint(number, "out"), StreamId::try_new(name).unwrap());
        }
        builder = builder.connect(first.clone(), pipe());
        if connect_second {
            builder = builder.connect(second.clone(), pipe());
            if live_only {
                builder =
                    builder.relationship_policy(second.clone(), RelationshipPolicy::live_only());
            }
        }
        observe(builder.build().unwrap(), |observed| {
            if connect_second && !live_only {
                for edge in [&first, &second] {
                    assert_refused(observed, edge, |error| {
                        matches!(error, Incompatibility::SharedInputCutUnavailable)
                    });
                }
            } else {
                assert_bound(observed, &first);
                if connect_second {
                    assert_bound(observed, &second);
                }
            }
        })
        .await;
    }
}

fn provider_to_consumer() -> EdgeDefinition {
    EdgeDefinition::new(
        endpoint(PROVIDER, "snapshot"),
        endpoint(CONSUMER, "bootstrap"),
    )
}

/// Deploy `graph`, then hand its observation to `check`.
async fn observe(mut graph: ComputationGraph, check: impl FnOnce(&ObservedGraph)) {
    let run = graph.run().expect("run");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        tokio::time::timeout(Duration::from_secs(10), async {
            control.deployment_report().await.expect("deployment");
            check(&control.observed());
        })
        .await
        .expect("check deadline");
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    graph.dispose().await.expect("cleanup");
}

fn provider_and_source(
    provider_output: BootstrapOutputDescription,
    source_component: Source,
) -> ComputationGraph {
    ComputationGraph::builder("bootstrap-binding")
        .bootstrap_provider(
            fixture_support::component_name(),
            Box::new(provider(provider_output)),
        )
        .fixture_source(Box::new(source_component))
        .bind_stream(
            endpoint(SOURCE, "out"),
            StreamId::try_new("source/out").expect("stream"),
        )
        .connect(provider_to_source(), pipe())
        .build()
        .expect("a mismatch never rejects the graph")
}

fn assert_bound(observed: &ObservedGraph, edge: &EdgeDefinition) {
    let relationship = &observed.relationships[edge];
    assert_eq!(
        relationship.binding,
        BindingState::Bound,
        "{relationship:?}"
    );
    assert!(observed.components[&edge.to.component].failure.is_none());
}

/// The edge is failed and unbound, both nodes stay, and the consumer carries
/// the terminal binding failure `expected` matches.
fn assert_refused(
    observed: &ObservedGraph,
    edge: &EdgeDefinition,
    expected: impl Fn(&Incompatibility) -> bool,
) {
    let relationship = &observed.relationships[edge];
    assert_eq!(
        relationship.binding,
        BindingState::Failed,
        "{relationship:?}"
    );
    let consumer = &observed.components[&edge.to.component];
    let failure = consumer.failure.as_ref().expect("consumer binding failure");
    assert_eq!(failure.phase, FailurePhase::Binding);
    assert_eq!(failure.disposition, FailureDisposition::Terminal);
    match failure.cause.underlying() {
        GraphError::Incompatible { source, .. } => assert!(expected(source), "{source:?}"),
        other => panic!("expected an incompatible edge, got {other:?}"),
    }
    assert_eq!(consumer.realization, RealizationState::Blocked);
    assert!(observed.components[&edge.from.component].failure.is_none());
}

fn paired(name: &str) -> BootstrapOutputDescription {
    output(
        BootstrapCompletionKind::Durable,
        Some(protocol(name)),
        ElementNamespace::ServedSource,
        None,
    )
}

fn offer(name: &str, required: bool) -> Option<BootstrapCoordination> {
    Some(BootstrapCoordination {
        protocol: protocol(name),
        required,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_paired_provider_binds_to_the_source_offering_its_protocol() {
    observe(
        provider_and_source(
            paired("drasi.test/coordinated"),
            source(offer("drasi.test/coordinated", true), None),
        ),
        |observed| assert_bound(observed, &provider_to_source()),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_paired_provider_on_the_wrong_source_is_refused_when_the_edge_binds() {
    observe(
        provider_and_source(
            paired("drasi.test/coordinated"),
            source(offer("drasi.test/other", false), None),
        ),
        |observed| {
            assert_refused(observed, &provider_to_source(), |error| {
                matches!(
                    error,
                    Incompatibility::BootstrapPairing { offered: Some(offered), .. }
                        if **offered == protocol("drasi.test/other")
                )
            })
        },
    )
    .await;
    observe(
        provider_and_source(paired("drasi.test/coordinated"), source(None, None)),
        |observed| {
            assert_refused(observed, &provider_to_source(), |error| {
                matches!(
                    error,
                    Incompatibility::BootstrapPairing { offered: None, .. }
                )
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_source_requiring_coordination_refuses_an_unpaired_provider() {
    let unpaired = output(
        BootstrapCompletionKind::Quiescent,
        None,
        ElementNamespace::ServedSource,
        None,
    );
    observe(
        provider_and_source(
            unpaired.clone(),
            source(offer("drasi.test/coordinated", true), None),
        ),
        |observed| {
            assert_refused(observed, &provider_to_source(), |error| {
                matches!(error, Incompatibility::BootstrapCoordinationRequired { .. })
            })
        },
    )
    .await;
    observe(
        provider_and_source(
            unpaired,
            source(offer("drasi.test/coordinated", false), None),
        ),
        |observed| assert_bound(observed, &provider_to_source()),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_snapshot_with_other_element_identities_than_its_source_is_refused() {
    let file = |binding: &str| {
        output(
            BootstrapCompletionKind::Quiescent,
            None,
            ElementNamespace::ServedSource,
            Some(identity(binding)),
        )
    };
    observe(
        provider_and_source(
            file("sensors:serial"),
            source(None, Some(identity("sensors:serial"))),
        ),
        |observed| assert_bound(observed, &provider_to_source()),
    )
    .await;
    observe(
        provider_and_source(
            file("sensors:serial"),
            source(None, Some(identity("sensors:rowid"))),
        ),
        |observed| {
            assert_refused(observed, &provider_to_source(), |error| {
                matches!(error, Incompatibility::BootstrapIdentity { .. })
            })
        },
    )
    .await;
}

fn provider_and_consumer(
    provider_output: BootstrapOutputDescription,
    input: BootstrapInputDescription,
) -> ComputationGraph {
    ComputationGraph::builder("bootstrap-binding")
        .bootstrap_provider(
            fixture_support::component_name(),
            Box::new(provider(provider_output)),
        )
        .fixture_sink(Box::new(consumer(input)))
        .connect(provider_to_consumer(), pipe())
        .build()
        .expect("a mismatch never rejects the graph")
}

#[tokio::test(flavor = "current_thread")]
async fn a_provider_serving_a_sources_namespace_needs_a_source_relation() {
    let served = output(
        BootstrapCompletionKind::Quiescent,
        None,
        ElementNamespace::ServedSource,
        None,
    );
    observe(
        provider_and_consumer(served, BootstrapInputDescription::DURABLE),
        |observed| {
            assert_refused(observed, &provider_to_consumer(), |error| {
                matches!(error, Incompatibility::BootstrapNamespace)
            })
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_query_local_provider_serves_the_single_source_its_consumer_reads() {
    let file = |binding: &str| {
        output(
            BootstrapCompletionKind::Quiescent,
            None,
            ElementNamespace::ServedSource,
            Some(identity(binding)),
        )
    };
    let graph = |binding: &str| {
        ComputationGraph::builder("bootstrap-binding")
            .bootstrap_provider(
                fixture_support::component_name(),
                Box::new(provider(file(binding))),
            )
            .fixture_source(Box::new(source(None, Some(identity("items:code")))))
            .fixture_sink(Box::new(consumer(BootstrapInputDescription::DURABLE)))
            .bind_stream(
                endpoint(SOURCE, "out"),
                StreamId::try_new("source/out").expect("stream"),
            )
            .connect(
                EdgeDefinition::new(endpoint(SOURCE, "out"), endpoint(CONSUMER, "in")),
                pipe(),
            )
            .connect(provider_to_consumer(), pipe())
            .build()
            .expect("a mismatch never rejects the graph")
    };
    // The consumer's single data input names the source whose namespace and
    // identities the local provider must match.
    observe(graph("items:code"), |observed| {
        assert_bound(observed, &provider_to_consumer())
    })
    .await;
    observe(graph("items:rowid"), |observed| {
        assert_refused(observed, &provider_to_consumer(), |error| {
            matches!(error, Incompatibility::BootstrapIdentity { .. })
        })
    })
    .await;
}

#[test]
fn the_served_source_is_the_relay_or_the_single_data_producer() {
    let node = |component: ComponentId, ports: Vec<PortDescriptor>, streams: &[(&str, &str)]| {
        NodeSnapshot {
            name: fixture_support::component_name(),
            descriptor: ComponentDescriptor::try_new(component, ports).expect("descriptor"),
            role: ComponentRole::Source,
            completion: None,
            output_streams: streams
                .iter()
                .map(|(port_name, stream)| {
                    (port(port_name), StreamId::try_new(*stream).expect("stream"))
                })
                .collect(),
            input_merge: InputMergePolicy::default(),
        }
    };
    let relay = node(
        id(SOURCE),
        vec![
            graph_port("out", PortDirection::Output, false),
            graph_port("bootstrap", PortDirection::Input, true),
        ],
        &[("out", "source/out")],
    );
    let reader = node(
        id(CONSUMER),
        vec![
            graph_port("in", PortDirection::Input, false),
            graph_port("bootstrap", PortDirection::Input, true),
        ],
        &[],
    );
    // The reader's own scheduled-notification helper has no namespace to
    // serve.
    let helper_id =
        ComponentId::derive_owned(&id(CONSUMER), QUERY_SCHEDULED_HELPER).expect("helper identity");
    let helper = node(
        helper_id.clone(),
        vec![graph_port("out", PortDirection::Output, false)],
        &[("out", "helper/out")],
    );
    let nodes = [relay, reader, helper];
    let lookup =
        |component: &ComponentId| nodes.iter().find(|node| node.descriptor.id() == component);
    let data = EdgeDefinition::new(endpoint(SOURCE, "out"), endpoint(CONSUMER, "in"));
    let stream = StreamId::try_new("source/out").expect("stream");
    assert_eq!(
        served_source(&id(SOURCE), lookup, []),
        Some((id(SOURCE), stream.clone())),
        "a source serves its own namespace"
    );
    let scheduled = EdgeDefinition::new(
        Endpoint::new(helper_id.clone(), port("out")),
        endpoint(CONSUMER, "in"),
    );
    assert_eq!(
        served_source(&id(CONSUMER), lookup, [&data, &scheduled]),
        Some((id(SOURCE), stream)),
        "a reader is served the source it reads"
    );
    assert_eq!(
        served_source(&id(CONSUMER), lookup, []),
        None,
        "a reader of no source has no served namespace"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_quiescent_input_refuses_a_durable_provider_before_any_snapshot() {
    let own = |completion| output(completion, None, ElementNamespace::Own, None);
    observe(
        provider_and_consumer(
            own(BootstrapCompletionKind::Durable),
            BootstrapInputDescription::QUIESCENT,
        ),
        |observed| {
            assert_refused(observed, &provider_to_consumer(), |error| {
                matches!(
                    error,
                    Incompatibility::BootstrapCompletion {
                        completion: BootstrapCompletionKind::Durable,
                        ..
                    }
                )
            })
        },
    )
    .await;
    observe(
        provider_and_consumer(
            own(BootstrapCompletionKind::Quiescent),
            BootstrapInputDescription::QUIESCENT,
        ),
        |observed| assert_bound(observed, &provider_to_consumer()),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_source_relaying_a_durable_provider_to_a_quiescent_input_is_refused() {
    let durable = output(
        BootstrapCompletionKind::Durable,
        None,
        ElementNamespace::ServedSource,
        None,
    );
    let graph = ComputationGraph::builder("bootstrap-binding")
        .bootstrap_provider(
            fixture_support::component_name(),
            Box::new(provider(durable)),
        )
        .fixture_source(Box::new(source(None, None)))
        .fixture_sink(Box::new(consumer(BootstrapInputDescription::QUIESCENT)))
        .bind_stream(
            endpoint(SOURCE, "out"),
            StreamId::try_new("source/out").expect("stream"),
        )
        .connect(provider_to_source(), pipe())
        .connect(source_to_consumer(), pipe())
        .build()
        .expect("a mismatch never rejects the graph");
    observe(graph, |observed| {
        // The relay's completion is its provider's, so the mismatch is found
        // through the relay from either edge: the quiescent consumer refuses
        // the source's output, and the source refuses a provider whose
        // completion one of its consumers cannot accept.
        let failed: Vec<_> = [provider_to_source(), source_to_consumer()]
            .into_iter()
            .filter(|edge| observed.relationships[edge].binding == BindingState::Failed)
            .collect();
        assert!(!failed.is_empty(), "{:#?}", observed.relationships);
        for edge in &failed {
            let consumer = &observed.components[&edge.to.component];
            let failure = consumer.failure.as_ref().expect("consumer binding failure");
            assert_eq!(failure.phase, FailurePhase::Binding);
            assert!(
                matches!(
                    failure.cause.underlying(),
                    GraphError::Incompatible {
                        source: Incompatibility::BootstrapCompletion {
                            accepts: BootstrapAcceptance::Quiescent,
                            completion: BootstrapCompletionKind::Durable,
                        },
                        ..
                    }
                ),
                "{failure:?}"
            );
        }
        assert!(
            observed.components[&id(PROVIDER)].failure.is_none(),
            "the provider stays in the graph without a failure"
        );
    })
    .await;
}
