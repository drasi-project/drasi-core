// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

#[allow(dead_code)]
mod computation_support;
#[path = "computation_support/pipe_profiles.rs"]
mod profiles;

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use profiles::{timed_event, Profile};

#[derive(Default)]
struct Evidence {
    created: Mutex<Vec<ComponentRole>>,
    started: Mutex<Vec<ComponentId>>,
    stopped: Mutex<Vec<ComponentId>>,
    dropped: Mutex<Vec<ComponentId>>,
    received: Received,
}

struct Stage {
    descriptor: ComponentDescriptor,
    evidence: Arc<Evidence>,
    sequence: u64,
    running: bool,
}

fn stage_descriptor(role: ComponentRole) -> ComponentDescriptor {
    match role {
        ComponentRole::Source => descriptor("source", &[], &["out"]),
        ComponentRole::Transformer => descriptor("transform", &["in"], &["out"]),
        ComponentRole::Sink => descriptor("sink", &["in"], &[]),
        ComponentRole::Query | ComponentRole::Service => unreachable!("synthetic pipeline roles"),
    }
}

impl Stage {
    fn new(role: ComponentRole, evidence: Arc<Evidence>) -> Self {
        Self {
            descriptor: stage_descriptor(role),
            evidence,
            sequence: 0,
            running: false,
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.evidence
            .dropped
            .lock()
            .expect("drop evidence")
            .push(self.descriptor.id().clone());
    }
}

#[async_trait]
impl ComputationComponent for Stage {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        assert!(!self.running, "double start");
        self.running = true;
        self.evidence
            .started
            .lock()
            .expect("start evidence")
            .push(self.descriptor.id().clone());
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        assert!(self.running, "stop without start");
        self.running = false;
        self.evidence
            .stopped
            .lock()
            .expect("stop evidence")
            .push(self.descriptor.id().clone());
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Stage {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        assert!(self.running);
        self.sequence += 1;
        Ok((self.sequence <= 4).then(|| output(timed_event(self.sequence))))
    }
}

#[async_trait]
impl Transformer for Stage {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        assert!(self.running);
        assert_eq!(input.port, port("in"));
        let mut outputs = Vec::new();
        // The independent expected sequence below checks zero/one/many, not just count.
        for offset in 0..input.envelope.system().sequence() % 3 {
            self.sequence += 1;
            let envelope = input.envelope.derive(
                emission_id(&stream("transform"), self.sequence)?,
                changes(
                    "transform",
                    self.sequence,
                    &[values(&input.envelope)[0] * 10 + offset as u16],
                ),
                SystemMetadata::new(stream("transform"), self.sequence)
                    .with_timestamp(input.envelope.system().timestamp().expect("event time")),
            );
            outputs.push(output(envelope));
        }
        Ok(outputs)
    }
}

#[async_trait]
impl EnvelopeSink for Stage {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        assert!(self.running);
        assert_eq!(input.port, port("in"));
        self.evidence
            .received
            .lock()
            .expect("received evidence")
            .push(input);
        Ok(())
    }
}

struct Factory {
    descriptor: FactoryDescriptor,
    evidence: Arc<Evidence>,
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.descriptor == stage_descriptor(self.descriptor.role),
            "unexpected interface"
        );
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let role = self.descriptor.role;
        assert_eq!(&context.component_id, stage_descriptor(role).id());
        self.evidence
            .created
            .lock()
            .expect("creation evidence")
            .push(role);
        let stage = Box::new(Stage::new(role, self.evidence.clone()));
        Ok(match role {
            ComponentRole::Source => ConstructedComponent::source(stage),
            ComponentRole::Transformer => ConstructedComponent::transformer(stage),
            ComponentRole::Sink => ConstructedComponent::sink(stage),
            ComponentRole::Query | ComponentRole::Service => {
                unreachable!("synthetic pipeline roles")
            }
        })
    }
}

async fn matrix() {
    let mut combinations = 0;
    for incoming in Profile::ALL {
        for outgoing in Profile::ALL {
            for factory_mask in 0..8 {
                let evidence = Arc::new(Evidence::default());
                let mut builder = ComputationGraph::builder("foundation-matrix");
                for (bit, role) in
                    [ComponentRole::Source, ComponentRole::Transformer, ComponentRole::Sink]
                        .into_iter()
                        .enumerate()
                {
                    if factory_mask & (1 << bit) == 0 {
                        let stage = Box::new(Stage::new(role, evidence.clone()));
                        builder = match role {
                            ComponentRole::Source => builder.source(stage),
                            ComponentRole::Transformer => builder.transformer(stage),
                            ComponentRole::Sink => builder.sink(stage),
                            ComponentRole::Query | ComponentRole::Service => unreachable!(),
                        };
                    } else {
                        let implementation =
                            ImplementationIdentity::try_new(format!("test/{role:?}"), "1")
                                .expect("implementation");
                        builder = builder.component(
                            ComponentSpecification {
                                descriptor: stage_descriptor(role),
                                role,
                                completion: (role == ComponentRole::Sink)
                                    .then_some(SinkCompletion::Handled),
                                implementation: implementation.clone(),
                                configuration_version: 1,
                                configuration: BTreeMap::new(),
                                dependencies: BTreeMap::new(),
                            },
                            Arc::new(Factory {
                                descriptor: FactoryDescriptor {
                                    implementation,
                                    role,
                                    configuration_version: 1,
                                    configuration: ConfigurationSchema::default(),
                                    dependencies: BTreeMap::new(),
                                },
                                evidence: evidence.clone(),
                            }),
                        );
                    }
                }
                for (profile, from, to) in
                    [(incoming, "source", "transform"), (outgoing, "transform", "sink")]
                {
                    let capacity = NonZeroUsize::new(if profile.backpressure() { 1 } else { 8 })
                        .expect("capacity");
                    let (provider, resources) = profile.provider(from, stream(from), capacity);
                    for (id, handle) in resources {
                        let role = provider.resource_dependencies()[&id];
                        builder = builder
                            .declare_resource(ResourceSpecification {
                                id: id.clone(),
                                role,
                                ownership: ResourceOwnership::Graph,
                                binding: Arc::from(id.as_str()),
                            })
                            .expect("resource declaration")
                            .provide_resource(id, handle)
                            .expect("resource binding");
                    }
                    builder = builder
                        .bind_stream(endpoint(from, "out"), stream(from))
                        .connect(edge(from, to), provider);
                }
                let case = format!("{incoming:?} -> {outgoing:?}, factory mask {factory_mask}");
                let mut graph = builder
                    .build()
                    .unwrap_or_else(|error| panic!("{case}: {error}"));
                assert!(evidence.started.lock().expect("start evidence").is_empty());
                assert!(evidence
                    .created
                    .lock()
                    .expect("creation evidence")
                    .is_empty());
                tokio::time::timeout(Duration::from_secs(5), graph.start().expect("start"))
                    .await
                    .unwrap_or_else(|_| panic!("{case}: deadlocked"))
                    .unwrap_or_else(|error| panic!("{case}: {error}"));
                assert_eq!(graph.state(), GraphState::Completed, "{case}");
                {
                    let received = evidence.received.lock().expect("received evidence");
                    assert_eq!(
                        received
                            .iter()
                            .flat_map(|input| values(&input.envelope))
                            .collect::<Vec<_>>(),
                        [10, 20, 21, 40],
                        "{case}"
                    );
                    assert_eq!(
                        received
                            .iter()
                            .map(|input| input.envelope.system().sequence())
                            .collect::<Vec<_>>(),
                        [1, 2, 3, 4],
                        "{case}"
                    );
                    assert_eq!(
                        received
                            .iter()
                            .map(|input| input
                                .envelope
                                .lineage()
                                .expect("lineage")
                                .system()
                                .sequence())
                            .collect::<Vec<_>>(),
                        [1, 2, 2, 4],
                        "{case}"
                    );
                    assert!(received
                        .iter()
                        .all(|input| input.envelope.changes().schema() == &schema_descriptor()));
                }
                assert_eq!(
                    evidence.created.lock().expect("creation evidence").len(),
                    (factory_mask as u32).count_ones() as usize,
                    "{case}"
                );
                for observed in [&evidence.started, &evidence.stopped] {
                    let mut ids = observed.lock().expect("lifecycle evidence").clone();
                    ids.sort();
                    assert_eq!(
                        ids,
                        [component("sink"), component("source"), component("transform")],
                        "{case}"
                    );
                }
                graph.dispose().await.expect("dispose");
                drop(graph);
                let mut dropped = evidence.dropped.lock().expect("drop evidence").clone();
                dropped.sort();
                assert_eq!(
                    dropped,
                    [component("sink"), component("source"), component("transform")],
                    "{case}"
                );
                combinations += 1;
            }
        }
    }
    assert_eq!(combinations, 648);
}

#[tokio::test(flavor = "current_thread")]
async fn all_pipe_pairs_and_construction_modes_on_current_thread() {
    matrix().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_pipe_pairs_and_construction_modes_on_multiple_threads() {
    matrix().await;
}
