// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]

use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use drasi_lib::context::{ComponentResource, ComponentResourceObserver};

fn component(name: &str) -> ComponentId {
    ComponentId::try_new(name).unwrap()
}

fn resource(name: &str) -> ResourceId {
    ResourceId::try_new(name).unwrap()
}

fn descriptor(name: &str) -> ComponentDescriptor {
    ComponentDescriptor::try_new(component(name), Vec::new()).unwrap()
}

struct Idle(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Idle {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Idle {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

fn graph(names: &[&str]) -> ComputationGraph {
    let mut builder = ComputationGraph::builder("resource-observation");
    for name in names {
        builder = builder.service(Box::new(Idle(descriptor(name))));
    }
    builder.build().unwrap()
}

fn specification(
    name: &str,
    role: ResourceRole,
    ownership: ResourceOwnership,
) -> ResourceSpecification {
    ResourceSpecification {
        id: resource(name),
        role,
        ownership,
        binding: Arc::from(name),
    }
}

fn binding(name: &str, handle: ResourceHandle) -> (ResourceSpecification, ResourceHandle) {
    (
        specification(name, handle.role(), ResourceOwnership::Borrowed),
        handle,
    )
}

struct NativeBinding(Arc<usize>);

fn native(provider: &Arc<usize>) -> ResourceHandle {
    ResourceHandle::new(
        ResourceRole::StateStore,
        Arc::new(NativeBinding(provider.clone())),
    )
    .with_shared_identity(provider.clone())
}

struct CountCleanup(Arc<AtomicUsize>);

#[async_trait]
impl ResourceCleanup for CountCleanup {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn published(control: &GraphControl, revision: u64) -> Arc<GraphSnapshot> {
    let mut changes = control.subscribe_observed();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            changes.borrow_and_update();
            let desired = control.desired_snapshot();
            if desired.revision.0 >= revision {
                return desired;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("resource publication deadline")
}

fn report(
    control: &GraphControl,
    name: &str,
    resources: Vec<(ResourceSpecification, ResourceHandle)>,
) {
    let handle = control.component_handle(&component(name)).unwrap();
    control
        .observe_resources(handle.id(), handle.generation(), resources)
        .unwrap();
}

async fn remove(control: &GraphControl, name: &str) {
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![component(name)]),
                policy: RemovalPolicy::Reject,
            }],
        )
        .await
        .unwrap();
    assert!(
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap()
            .committed
    );
}

#[tokio::test]
async fn shared_native_reports_coalesce_before_publication_and_replace_complete_snapshots() {
    let mut graph = graph(&["first", "second"]);
    let provider = Arc::new(7usize);
    let cleanup = Arc::new(AtomicUsize::new(0));
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision.0;
        for name in ["first", "second"] {
            report(
                &control,
                name,
                vec![binding(
                    "store",
                    native(&provider).with_cleanup(Arc::new(CountCleanup(cleanup.clone()))),
                )],
            );
        }
        assert_eq!(control.desired_snapshot().revision.0, revision);
        let desired = published(&control, revision + 2).await;
        assert_eq!(desired.resources.len(), 1);
        assert_eq!(
            desired.component_resources[&component("first")],
            desired.component_resources[&component("second")]
        );
        let original = desired.resources.keys().next().unwrap().clone();
        let generation = control.observed().resources[&original].generation;
        assert_eq!(
            desired.resources[&original].ownership,
            ResourceOwnership::Borrowed
        );

        let replacement = Arc::new(7usize);
        report(
            &control,
            "first",
            vec![binding("store", native(&replacement))],
        );
        let desired = published(&control, desired.revision.0 + 1).await;
        assert_eq!(desired.resources.len(), 2);
        assert_ne!(
            desired.component_resources[&component("first")],
            desired.component_resources[&component("second")]
        );
        assert_eq!(
            control.observed().resources[&original].generation,
            generation
        );

        report(&control, "first", Vec::new());
        let desired = published(&control, desired.revision.0 + 1).await;
        assert!(desired.component_resources[&component("first")].is_empty());
        assert_eq!(desired.resources.len(), 1);
        assert!(desired.resources.contains_key(&original));
        remove(&control, "first").await;
        assert_eq!(control.desired_snapshot().resources.len(), 1);
        remove(&control, "second").await;
        assert!(control.desired_snapshot().resources.is_empty());
        assert!(control.observed().resources.is_empty());
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert_eq!(cleanup.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shared_identity_requires_the_same_role_and_concrete_binding_interface() {
    struct OtherBinding;

    let provider = Arc::new(7usize);
    let mut graph = graph(&["store", "identity", "other-interface"]);
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision.0;
        report(
            &control,
            "store",
            vec![binding("provider", native(&provider))],
        );
        report(
            &control,
            "identity",
            vec![binding(
                "provider",
                ResourceHandle::new(
                    ResourceRole::Identity,
                    Arc::new(NativeBinding(provider.clone())),
                )
                .with_shared_identity(provider.clone()),
            )],
        );
        report(
            &control,
            "other-interface",
            vec![binding(
                "provider",
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(OtherBinding))
                    .with_shared_identity(provider),
            )],
        );
        let desired = published(&control, revision + 3).await;
        assert_eq!(desired.resources.len(), 3);
        assert_eq!(
            desired
                .component_resources
                .values()
                .flat_map(|resources| resources.iter().cloned())
                .collect::<BTreeSet<_>>()
                .len(),
            3
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn untagged_handles_use_the_actual_binding_arc_not_wrapper_contents() {
    let provider = Arc::new(7usize);
    let shared = Arc::new(NativeBinding(provider.clone()));
    let mut graph = graph(&["first", "second", "separate-binding"]);
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision.0;
        for name in ["first", "second"] {
            report(
                &control,
                name,
                vec![binding(
                    "store",
                    ResourceHandle::new(ResourceRole::StateStore, shared.clone()),
                )],
            );
        }
        report(
            &control,
            "separate-binding",
            vec![binding(
                "store",
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(NativeBinding(provider))),
            )],
        );
        let desired = published(&control, revision + 3).await;
        assert_eq!(desired.resources.len(), 2);
        assert_eq!(
            desired.component_resources[&component("first")],
            desired.component_resources[&component("second")]
        );
        assert_ne!(
            desired.component_resources[&component("first")],
            desired.component_resources[&component("separate-binding")]
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

struct BindingProbe<T> {
    descriptor: FactoryDescriptor,
    expected: Arc<T>,
}

#[async_trait]
impl<T: Any + Send + Sync> ComponentFactory for BindingProbe<T> {
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
        let bindings = context
            .resources::<T>("provider")
            .map_err(ComponentCreationError::terminal)?;
        if bindings.len() != 1 || !Arc::ptr_eq(&bindings[0], &self.expected) {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "observation replaced the actual registered binding"
            )));
        }
        Ok(ConstructedComponent::service(Box::new(Idle(
            context.specification.descriptor.clone(),
        ))))
    }
}

async fn probe_binding<T: Any + Send + Sync>(
    control: &GraphControl,
    id: ResourceId,
    role: ResourceRole,
    expected: Arc<T>,
) {
    let implementation = ImplementationIdentity::try_new("fixture/binding-probe", "1").unwrap();
    let factory = Arc::new(BindingProbe {
        descriptor: FactoryDescriptor {
            implementation: implementation.clone(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::from([(
                Arc::from("provider"),
                ResourceRequirement::exactly_one::<T>(role),
            )]),
        },
        expected,
    });
    let probe = control
        .add_component(ComponentAddition::from_specification(
            ComponentSpecification {
                descriptor: descriptor("probe"),
                role: ComponentRole::Service,
                completion: None,
                implementation,
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: BTreeMap::from([(Arc::from("provider"), vec![id])]),
            },
            factory,
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), probe.wait_started())
        .await
        .unwrap()
        .unwrap();
    remove(control, "probe").await;
}

#[tokio::test]
async fn observing_an_owned_instance_preserves_its_binding_and_cleanup_responsibility() {
    let provider = Arc::new(7usize);
    let original_binding = Arc::new(NativeBinding(provider.clone()));
    assert!(Arc::ptr_eq(&original_binding.0, &provider));
    let cleanup = Arc::new(AtomicUsize::new(0));
    let id = resource("attached/owned-store");
    let mut graph = ComputationGraph::builder("owned-observation")
        .service(Box::new(Idle(descriptor("component"))))
        .declare_resource(specification(
            id.as_str(),
            ResourceRole::StateStore,
            ResourceOwnership::Graph,
        ))
        .unwrap()
        .provide_resource(
            id.clone(),
            ResourceHandle::new(ResourceRole::StateStore, original_binding.clone())
                .with_shared_identity(provider.clone())
                .with_cleanup(Arc::new(CountCleanup(cleanup.clone()))),
        )
        .unwrap()
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision.0;
        report(
            &control,
            "component",
            vec![binding("store", native(&provider))],
        );
        let desired = published(&control, revision + 1).await;
        assert_eq!(desired.resources.len(), 1);
        assert_eq!(desired.resources[&id].ownership, ResourceOwnership::Graph);
        assert_eq!(
            desired.component_resources[&component("component")],
            BTreeSet::from([id.clone()])
        );
        probe_binding(
            &control,
            id.clone(),
            ResourceRole::StateStore,
            original_binding,
        )
        .await;
        let revision = control.desired_snapshot().revision.0;
        report(&control, "component", Vec::new());
        let desired = published(&control, revision + 1).await;
        assert!(desired.component_resources[&component("component")].is_empty());
        assert_eq!(desired.resources[&id].ownership, ResourceOwnership::Graph);
        assert_eq!(cleanup.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
    graph.dispose().await.unwrap();
    assert_eq!(cleanup.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_rebinding_still_requires_a_new_binding_not_a_new_shared_provider() {
    let provider = Arc::new(7usize);
    let old_cleanup = Arc::new(AtomicUsize::new(0));
    let new_cleanup = Arc::new(AtomicUsize::new(0));
    let original = native(&provider).with_cleanup(Arc::new(CountCleanup(old_cleanup.clone())));
    let id = resource("owned-store");
    let mut graph = ComputationGraph::builder("explicit-rebinding")
        .service(Box::new(Idle(descriptor("component"))))
        .declare_resource(specification(
            id.as_str(),
            ResourceRole::StateStore,
            ResourceOwnership::Graph,
        ))
        .unwrap()
        .provide_resource(id.clone(), original.clone())
        .unwrap()
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RebindResource(id.clone())],
            )
            .await
            .unwrap();
        let mut bindings = TopologyBindings::default();
        bindings.resources.insert(id.clone(), original);
        assert!(matches!(
            control.reconcile(preview.clone(), bindings).await,
            Err(GraphError::Topology { .. })
        ));
        assert_eq!(old_cleanup.load(Ordering::SeqCst), 0);
        let mut bindings = TopologyBindings::default();
        bindings.resources.insert(
            id.clone(),
            native(&provider).with_cleanup(Arc::new(CountCleanup(new_cleanup.clone()))),
        );
        assert!(
            control
                .reconcile(preview, bindings)
                .await
                .unwrap()
                .committed
        );
        assert_eq!(old_cleanup.load(Ordering::SeqCst), 1);
        assert_eq!(new_cleanup.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert_eq!(old_cleanup.load(Ordering::SeqCst), 1);
    assert_eq!(new_cleanup.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_reports_reject_ownership_transfer_roles_duplicates_and_obsolete_generations() {
    let provider = Arc::new(7usize);
    let cleanup = Arc::new(AtomicUsize::new(0));
    let mut graph = graph(&["component"]);
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let handle = control.component_handle(&component("component")).unwrap();
        let report =
            |bindings| control.observe_resources(handle.id(), handle.generation(), bindings);
        let revision = control.desired_snapshot().revision.0;
        let owned = specification("store", ResourceRole::StateStore, ResourceOwnership::Graph);
        assert!(matches!(
            report(vec![(
                owned,
                native(&provider).with_cleanup(Arc::new(CountCleanup(cleanup.clone()))),
            )]),
            Err(GraphError::Topology { reason }) if reason.contains("cleanup ownership")
        ));
        let wrong_role =
            specification("store", ResourceRole::Identity, ResourceOwnership::Borrowed);
        assert!(matches!(
            report(vec![(wrong_role, native(&provider))]),
            Err(GraphError::Topology { .. })
        ));
        assert!(matches!(
            report(vec![
                binding("duplicate", native(&provider)),
                binding("duplicate", native(&provider)),
            ]),
            Err(GraphError::Topology { .. })
        ));
        for (id, generation) in [
            (
                handle.id().clone(),
                ComponentGeneration(handle.generation().0 + 1),
            ),
            (component("absent"), handle.generation()),
        ] {
            assert!(matches!(
                control.observe_resources(&id, generation, Vec::new()),
                Err(GraphError::StaleGeneration)
            ));
        }
        assert_eq!(control.desired_snapshot().revision.0, revision);
        assert!(control.desired_snapshot().resources.is_empty());
        report(vec![binding("store", native(&provider))]).unwrap();
        assert_eq!(published(&control, revision + 1).await.resources.len(), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert_eq!(cleanup.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn queued_reports_cannot_restore_retired_links_or_provenance_on_a_reused_component_id() {
    let provider = Arc::new(7usize);
    let mut graph = graph(&["component"]);
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let old = control.component_handle(&component("component")).unwrap();
        let revision = control.desired_snapshot().revision.0;
        report(
            &control,
            "component",
            vec![binding("store", native(&provider))],
        );
        published(&control, revision + 1).await;
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![component("component")]),
                    policy: RemovalPolicy::Reject,
                }],
            )
            .await
            .unwrap();
        let mut removal = Box::pin(control.reconcile(preview, TopologyBindings::default()));
        assert!(futures::poll!(removal.as_mut()).is_pending());
        control
            .observe_resources(
                old.id(),
                old.generation(),
                vec![binding("late", native(&provider))],
            )
            .unwrap();
        control
            .observe_plugin(old.id(), old.generation(), plugin("late-plugin", "1"))
            .unwrap();
        assert!(removal.await.unwrap().committed);
        let replacement = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::service(Box::new(Idle(descriptor(
                    "component",
                )))))
                .auto_start(false),
            )
            .await
            .unwrap();
        assert_ne!(old.generation(), replacement.generation());
        assert!(control.desired_snapshot().resources.is_empty());
        assert!(control.desired_snapshot().component_plugins.is_empty());
        assert!(matches!(
            control.observe_resources(old.id(), old.generation(), Vec::new()),
            Err(GraphError::StaleGeneration)
        ));
        assert!(matches!(
            control.observe_plugin(old.id(), old.generation(), plugin("late-plugin", "1")),
            Err(GraphError::StaleGeneration)
        ));
        let revision = control.desired_snapshot().revision.0;
        report(
            &control,
            "component",
            vec![binding("store", native(&provider))],
        );
        assert_eq!(published(&control, revision + 1).await.resources.len(), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

fn plugin(id: &str, version: &str) -> PluginIdentity {
    PluginIdentity {
        id: Arc::from(id),
        version: Arc::from(version),
    }
}

#[tokio::test]
async fn plugin_provenance_is_validated_before_queuing_and_again_at_publication() {
    let declared = plugin("declared-plugin", "1");
    let mut graph = ComputationGraph::builder("plugin-observation")
        .service(Box::new(Idle(
            descriptor("declared")
                .with_plugin_identity(declared.clone())
                .unwrap(),
        )))
        .service(Box::new(Idle(descriptor("opaque"))))
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let known = control.component_handle(&component("declared")).unwrap();
        assert!(matches!(
            control.observe_plugin(
                known.id(),
                known.generation(),
                plugin("declared-plugin", "2")
            ),
            Err(GraphError::Validation { .. })
        ));
        for invalid in [plugin("", "1"), plugin("valid", "bad version")] {
            assert!(matches!(
                control.observe_plugin(known.id(), known.generation(), invalid),
                Err(GraphError::Contract(_))
            ));
        }
        control
            .observe_plugin(known.id(), known.generation(), declared.clone())
            .unwrap();
        let opaque = control.component_handle(&component("opaque")).unwrap();
        let first = plugin("reported-plugin", "1");
        control
            .observe_plugin(opaque.id(), opaque.generation(), first.clone())
            .unwrap();
        control
            .observe_plugin(
                opaque.id(),
                opaque.generation(),
                plugin("reported-plugin", "2"),
            )
            .unwrap();
        let mut changes = control.subscribe_observed();
        tokio::time::timeout(
            Duration::from_secs(2),
            changes.wait_for(|state| state.components[opaque.id()].failure.is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            control.observed().components[opaque.id()]
                .failure
                .as_ref()
                .unwrap()
                .phase,
            FailurePhase::Validation
        );
        let desired = control.desired_snapshot();
        assert_eq!(desired.component_plugins[known.id()], declared);
        assert_eq!(desired.component_plugins[opaque.id()], first);
        assert!(desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == opaque.id())
            .unwrap()
            .descriptor
            .plugin_identity()
            .is_none());
        assert!(matches!(
            control.observe_plugin(
                opaque.id(),
                opaque.generation(),
                plugin("reported-plugin", "2")
            ),
            Err(GraphError::Validation { .. })
        ));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn observation_admission_is_nonblocking_and_reports_full_or_closed_queues() {
    let mut graph = graph(&["component"]);
    let run = graph.run().unwrap();
    let control = run.control();
    let handle = control.component_handle(&component("component")).unwrap();
    let mut accepted = 0;
    loop {
        match control.observe_resources(handle.id(), handle.generation(), Vec::new()) {
            Ok(()) => accepted += 1,
            Err(GraphError::ObservationQueueFull) => break,
            Err(error) => panic!("unexpected queue error: {error}"),
        }
        assert!(accepted < 1024, "observation queue must remain bounded");
    }
    assert!(accepted > 0);
    assert!(matches!(
        control.observe_plugin(handle.id(), handle.generation(), plugin("plugin", "1")),
        Err(GraphError::ObservationQueueFull)
    ));
    drop(run);
    assert!(matches!(
        control.observe_resources(handle.id(), handle.generation(), Vec::new()),
        Err(GraphError::ControllerClosed)
    ));
    assert!(matches!(
        control.observe_plugin(handle.id(), handle.generation(), plugin("plugin", "1")),
        Err(GraphError::ControllerClosed)
    ));
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn weak_graph_publications_do_not_retain_shared_identity_owners() {
    let owner = Arc::new(7usize);
    let weak = Arc::downgrade(&owner);
    let mut graph = ComputationGraph::builder("identity-lifetime")
        .service(Box::new(Idle(descriptor("component"))))
        .declare_resource(specification(
            "provider",
            ResourceRole::StateStore,
            ResourceOwnership::Borrowed,
        ))
        .unwrap()
        .provide_resource(
            resource("provider"),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(0usize))
                .with_shared_identity(owner),
        )
        .unwrap()
        .build()
        .unwrap();
    let inspector = graph.inspector();
    let history = inspector.snapshot();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        assert!(weak.upgrade().is_some());
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert!(
        weak.upgrade().is_some(),
        "the borrowed binding still exists"
    );
    drop(graph);
    assert!(weak.upgrade().is_none());
    assert!(control
        .desired_snapshot()
        .resources
        .contains_key(&resource("provider")));
    assert!(history
        .desired
        .resources
        .contains_key(&resource("provider")));
    assert!(inspector
        .snapshot()
        .desired
        .resources
        .contains_key(&resource("provider")));
}

#[tokio::test]
async fn compatibility_observers_reuse_untagged_legacy_bindings_without_weakening_binding_identity()
{
    let provider: Arc<dyn drasi_lib::identity::IdentityProvider> = Arc::new(
        drasi_lib::identity::PasswordIdentityProvider::new("user", "private"),
    );
    let original_binding = Arc::new(LegacyIdentityResource(provider.clone()));
    let mut graph = ComputationGraph::builder("legacy-normalization")
        .service(Box::new(Idle(descriptor("first"))))
        .service(Box::new(Idle(descriptor("second"))))
        .declare_resource(specification(
            "declared-identity",
            ResourceRole::Identity,
            ResourceOwnership::Borrowed,
        ))
        .unwrap()
        .provide_resource(
            resource("declared-identity"),
            ResourceHandle::new(ResourceRole::Identity, original_binding.clone()),
        )
        .unwrap()
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision.0;
        for name in ["first", "second"] {
            let handle = control.component_handle(&component(name)).unwrap();
            GraphResourceObserver::new(control.clone(), handle.id().clone(), handle.generation())
                .observe(vec![ComponentResource::Identity(provider.clone())])
                .await
                .unwrap();
        }
        let desired = published(&control, revision + 2).await;
        assert_eq!(desired.resources.len(), 1);
        for name in ["first", "second"] {
            assert_eq!(
                desired.component_resources[&component(name)],
                BTreeSet::from([resource("declared-identity")])
            );
        }
        probe_binding(
            &control,
            resource("declared-identity"),
            ResourceRole::Identity,
            original_binding,
        )
        .await;
        let revision = control.desired_snapshot().revision.0;
        for name in ["first", "second"] {
            let handle = control.component_handle(&component(name)).unwrap();
            GraphResourceObserver::new(control.clone(), handle.id().clone(), handle.generation())
                .observe(Vec::new())
                .await
                .unwrap();
        }
        let desired = published(&control, revision + 2).await;
        assert_eq!(desired.resources.len(), 1);
        assert!(desired.component_resources.values().all(BTreeSet::is_empty));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
}

#[test]
fn standard_registry_keeps_every_native_and_legacy_factory_registered() {
    let mut registry = FactoryRegistry::standard();
    let factories: Vec<Arc<dyn ComponentFactory>> = vec![
        Arc::new(ContinuousQueryFactory::default()),
        Arc::new(LegacySourceFactory::default()),
        Arc::new(SourcePluginAdapterFactory::default()),
        Arc::new(LegacyReactionFactory::default()),
        Arc::new(ReactionPluginAdapterFactory::default()),
        Arc::new(QueryReplayFactory::default()),
        Arc::new(QueryResultsOutletFactory::default()),
        Arc::new(WalReplaySourceFactory::default()),
        Arc::new(ComputationTopologyFactory::default()),
    ];
    for factory in factories {
        assert!(matches!(
            registry.register(factory),
            Err(GraphError::Topology { reason }) if reason == "duplicate component implementation"
        ));
    }
}
