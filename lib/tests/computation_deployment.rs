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

#![cfg(feature = "computation")]

#[allow(dead_code)]
mod computation_support;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;

struct SourceFactory {
    descriptor: FactoryDescriptor,
    creates: Arc<AtomicUsize>,
    fail: bool,
}

fn implementation() -> ImplementationIdentity {
    ImplementationIdentity::try_new("test/source", "1").expect("implementation")
}

fn specification(id: &str) -> ComponentSpecification {
    ComponentSpecification {
        descriptor: descriptor(id, &[], &["out"]),
        role: ComponentRole::Source,
        completion: None,
        implementation: implementation(),
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::new(),
    }
}

fn factory(fail: bool, creates: Arc<AtomicUsize>) -> Arc<SourceFactory> {
    Arc::new(SourceFactory {
        descriptor: FactoryDescriptor {
            implementation: implementation(),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        },
        creates,
        fail,
    })
}

#[async_trait]
impl ComponentFactory for SourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "invalid external component definition"
            )));
        }
        if self
            .descriptor
            .configuration
            .fields
            .contains_key("credential")
        {
            assert_eq!(
                context.configuration().get("credential"),
                Some(&serde_json::json!("resolved-private-value"))
            );
        }
        Ok(ConstructedComponent::source(Box::new(FiniteSource::new(
            context.component_id.as_str(),
            vec![output(root(context.component_id.as_str(), 1, &[3]))],
        ))))
    }
}

fn graph_builder(
    spec: ComponentSpecification,
    factory: Arc<dyn ComponentFactory>,
) -> ComputationGraphBuilder {
    let id = spec.descriptor.id().as_str().to_owned();
    ComputationGraph::builder("declarative")
        .component(spec, factory)
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            Received::default(),
        )))
        .bind_stream(endpoint(&id, "out"), stream(&id))
        .connect(
            edge(&id, "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
}

#[test]
fn entire_definition_preflight_happens_before_any_factory_call() {
    for fault in [
        "version",
        "role",
        "schema",
        "unknown-field",
        "factory",
        "plugin",
        "missing-resource",
        "secret-literal",
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut factory = factory(false, calls.clone());
        let mut spec = specification("source");
        match fault {
            "version" => spec.configuration_version = 2,
            "role" => spec.role = ComponentRole::Sink,
            "schema" => {
                spec.descriptor = ComponentDescriptor::try_new(
                    component("source"),
                    vec![PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        named_schema("other"),
                        PipeRequirements::default(),
                    )],
                )
                .expect("descriptor")
            }
            "unknown-field" => {
                spec.configuration.insert(
                    Arc::from("unknown"),
                    ConfigurationValue::Literal(serde_json::json!(1)),
                );
            }
            "factory" => {
                spec.implementation =
                    ImplementationIdentity::try_new("missing", "1").expect("identity")
            }
            "plugin" => {
                spec.implementation.plugin = Some(PluginIdentity {
                    id: Arc::from("plugin"),
                    version: Arc::from("2"),
                })
            }
            "missing-resource" => {
                Arc::get_mut(&mut factory)
                    .expect("unique factory")
                    .descriptor
                    .dependencies
                    .insert(
                        Arc::from("state"),
                        ResourceRequirement::exactly_one::<usize>(ResourceRole::StateStore),
                    );
                spec.dependencies.insert(
                    Arc::from("state"),
                    vec![ResourceId::try_new("missing").expect("resource id")],
                );
            }
            "secret-literal" => {
                Arc::get_mut(&mut factory)
                    .expect("unique factory")
                    .descriptor
                    .configuration
                    .fields
                    .insert(
                        Arc::from("credential"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: true,
                        },
                    );
                spec.configuration.insert(
                    Arc::from("credential"),
                    ConfigurationValue::Literal(serde_json::json!("must-not-be-in-desired")),
                );
            }
            _ => unreachable!(),
        }
        assert!(graph_builder(spec, factory).build().is_err(), "{fault}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{fault}");
    }
}

#[tokio::test]
async fn valid_specs_are_constructed_and_bound_before_activation() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut graph = graph_builder(specification("source"), factory(false, calls.clone()))
        .build()
        .expect("valid graph");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        graph.observed().components[&component("source")].realization,
        RealizationState::Pending
    );
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployed = control.deployment_report().await.expect("deployment");
        assert_eq!(deployed.summary, OperationSummary::Completed);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            deployed.components[&component("source")],
            CreationOutcome::Created
        ));
        assert_eq!(
            control.observed().components[&component("source")].lifecycle,
            ComponentLifecycle::Stopped
        );
        assert_eq!(
            control.observed().relationships[&edge("source", "sink")].binding,
            BindingState::Bound
        );
        let startup = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .expect("start");
        assert_eq!(startup.summary, OperationSummary::Completed);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn failed_creation_retains_desired_spec_and_independent_instances() {
    let bad_calls = Arc::new(AtomicUsize::new(0));
    let good_calls = Arc::new(AtomicUsize::new(0));
    let received = Received::default();
    let mut graph = graph_builder(specification("bad"), factory(true, bad_calls.clone()))
        .component(specification("good"), factory(false, good_calls.clone()))
        .sink(Box::new(CollectSink::new(
            "good-sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("good", "out"), stream("good"))
        .connect(
            edge("good", "good-sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid declarations");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployed = control
            .deployment_report()
            .await
            .expect("deployment results");
        let CreationOutcome::CreationFailed(failure) = &deployed.components[&component("bad")]
        else {
            panic!("explicit failure");
        };
        assert_eq!(failure.disposition, FailureDisposition::Terminal);
        assert!(matches!(
            deployed.components[&component("sink")],
            CreationOutcome::Blocked { .. }
        ));
        assert!(matches!(
            deployed.components[&component("good")],
            CreationOutcome::Created
        ));
        assert_eq!(control.desired_snapshot().specifications.len(), 2);
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .expect("partial startup");
        assert!(matches!(
            started.components[&component("bad")],
            StartOutcome::NotCreated
        ));
        assert!(matches!(
            started.components[&component("good")],
            StartOutcome::Started
        ));
        assert_eq!(bad_calls.load(Ordering::SeqCst), 1);
        assert_eq!(good_calls.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(graph.snapshot().specifications.len(), 2);
}

struct Resolver;

#[async_trait]
impl ConfigurationResolver for Resolver {
    fn validate_reference(&self, key: &str) -> anyhow::Result<()> {
        if key == "credential" {
            Ok(())
        } else {
            anyhow::bail!("unknown secret reference")
        }
    }

    async fn resolve(&self, _: &str) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!("resolved-private-value"))
    }
}

#[tokio::test]
async fn secret_resolution_is_runtime_only_and_missing_resource_stays_visible() {
    for supplied in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut factory = factory(false, calls.clone());
        Arc::get_mut(&mut factory)
            .expect("unique factory")
            .descriptor
            .configuration
            .fields
            .insert(
                Arc::from("credential"),
                ConfigurationField {
                    value_type: ConfigurationType::String,
                    required: true,
                    secret: true,
                },
            );
        let id = ResourceId::try_new("secrets").expect("resource");
        let mut spec = specification("source");
        spec.configuration.insert(
            Arc::from("credential"),
            ConfigurationValue::Reference {
                resource: id.clone(),
                key: Arc::from("credential"),
                secret: true,
            },
        );
        let mut builder = graph_builder(spec, factory)
            .declare_resource(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::SecretStore,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from("external-secret-store"),
            })
            .expect("declare resource");
        if supplied {
            builder = builder
                .provide_resource(
                    id.clone(),
                    ResourceHandle::new(
                        ResourceRole::SecretStore,
                        Arc::new(ConfigurationResolverResource(Arc::new(Resolver))),
                    ),
                )
                .expect("supply resource");
        }
        let mut graph = builder.build().expect("preflight");
        let run = graph.run().expect("controller");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let deployment = control.deployment_report().await.expect("deployment");
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(supplied));
            if supplied {
                assert!(matches!(
                    deployment.components[&component("source")],
                    CreationOutcome::Created
                ));
                assert_eq!(deployment.resources[&id], ResourceRealization::Created);
            } else {
                assert!(matches!(
                    deployment.components[&component("source")],
                    CreationOutcome::Blocked { .. }
                ));
                assert_eq!(deployment.resources[&id], ResourceRealization::Pending);
            }
            assert!(!format!("{:?}", control.desired_snapshot()).contains("resolved-private-value"));
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }
}

struct CleanupCounter(Arc<AtomicUsize>);

#[async_trait]
impl ResourceCleanup for CleanupCounter {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn explicit_disposal_cleans_owned_resources_but_never_borrowed_resources() {
    let owned = Arc::new(AtomicUsize::new(0));
    let borrowed = Arc::new(AtomicUsize::new(0));
    let mut builder = graph_builder(
        specification("source"),
        factory(false, Arc::new(AtomicUsize::new(0))),
    );
    for (id, ownership, counter) in [
        ("owned", ResourceOwnership::Graph, owned.clone()),
        ("borrowed", ResourceOwnership::Borrowed, borrowed.clone()),
    ] {
        let id = ResourceId::try_new(id).expect("id");
        builder = builder
            .declare_resource(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::StateStore,
                ownership,
                binding: Arc::from(id.as_str()),
            })
            .expect("specification")
            .provide_resource(
                id,
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(7usize))
                    .with_cleanup(Arc::new(CleanupCounter(counter))),
            )
            .expect("resource");
    }
    let mut graph = builder.build().expect("graph");
    graph.start().expect("scope").await.expect("finite drain");
    graph.dispose().await.expect("explicit resource disposal");
    graph.dispose().await.expect("idempotent disposal");
    assert_eq!(owned.load(Ordering::SeqCst), 1);
    assert_eq!(borrowed.load(Ordering::SeqCst), 0);
    assert!(matches!(
        graph.start(),
        Err(GraphError::InvalidState { .. })
    ));
}

struct PendingFactory {
    descriptor: FactoryDescriptor,
    entered: Arc<tokio::sync::Notify>,
    active: Arc<AtomicUsize>,
}

struct ActiveConstruction(Arc<AtomicUsize>);
impl Drop for ActiveConstruction {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ComponentFactory for PendingFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        _: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let _active = ActiveConstruction(self.active.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_a_pending_construction_retains_desired_state_and_cancels_scoped_work() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let active = Arc::new(AtomicUsize::new(0));
    let descriptor = factory(false, Arc::new(AtomicUsize::new(0)))
        .descriptor
        .clone();
    let mut graph = graph_builder(
        specification("source"),
        Arc::new(PendingFactory {
            descriptor,
            entered: entered.clone(),
            active: active.clone(),
        }),
    )
    .build()
    .expect("graph");
    let mut run = Box::pin(graph.run().expect("scope"));
    let control = run.control();
    tokio::select! {
        result = &mut run => panic!("construction must stay pending: {result:?}"),
        _ = entered.notified() => {}
    }
    assert_eq!(
        control.observed().components[&component("source")].realization,
        RealizationState::Creating
    );
    assert_eq!(active.load(Ordering::SeqCst), 1);
    drop(run);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert!(matches!(
        control.deployment_report().await,
        Err(GraphError::ControllerClosed)
    ));
    assert!(matches!(
        control.startup_report().await,
        Err(GraphError::ControllerClosed)
    ));
    graph
        .shutdown()
        .await
        .expect("cleanup cancelled construction");
    assert_eq!(graph.snapshot().specifications.len(), 1);
}

struct FailingCleanup(Arc<AtomicUsize>);
#[async_trait]
impl ResourceCleanup for FailingCleanup {
    async fn shutdown(&self) -> anyhow::Result<()> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            anyhow::bail!("retryable cleanup failure");
        }
        Ok(())
    }
}

#[tokio::test]
async fn failed_resource_cleanup_is_visible_and_explicitly_retryable() {
    let counter = Arc::new(AtomicUsize::new(0));
    let resource = ResourceId::try_new("state").expect("id");
    let mut graph = graph_builder(
        specification("source"),
        factory(false, Arc::new(AtomicUsize::new(0))),
    )
    .declare_resource(ResourceSpecification {
        id: resource.clone(),
        role: ResourceRole::StateStore,
        ownership: ResourceOwnership::Graph,
        binding: Arc::from("state"),
    })
    .expect("declaration")
    .provide_resource(
        resource.clone(),
        ResourceHandle::new(ResourceRole::StateStore, Arc::new(1usize))
            .with_cleanup(Arc::new(FailingCleanup(counter.clone()))),
    )
    .expect("resource")
    .build()
    .expect("graph");
    graph.start().expect("scope").await.expect("drain");
    assert!(matches!(
        graph.dispose().await,
        Err(GraphError::Cleanup { .. })
    ));
    assert_eq!(
        graph.observed().resources[&resource].realization,
        ResourceRealization::CleanupRequired
    );
    assert!(graph.observed().resources[&resource].failure.is_some());
    graph.dispose().await.expect("explicit retry");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    assert_eq!(
        graph.observed().resources[&resource].realization,
        ResourceRealization::Released
    );
}
