// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib};
use tokio::sync::Notify;

fn id(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource ID")
}

#[derive(Default)]
struct State {
    calls: Mutex<Vec<String>>,
    latest: Mutex<BTreeMap<ResourceId, Weak<Owner>>>,
    fail_create: AtomicBool,
    fail_cleanup: AtomicBool,
    gate_create: AtomicBool,
    gate_cleanup: AtomicBool,
    entered: Notify,
    release: Notify,
    created: AtomicUsize,
    dropped: AtomicUsize,
}

impl State {
    fn record(&self, operation: &str, resource: &ResourceId) {
        self.calls
            .lock()
            .expect("calls")
            .push(format!("{operation}:{resource}"));
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn owner(&self, name: &str) -> Arc<Owner> {
        self.latest.lock().expect("owners")[&id(name)]
            .upgrade()
            .expect("live owner")
    }
}

struct Owner {
    id: ResourceId,
    state: Arc<State>,
    dependencies: BTreeMap<ResourceId, Arc<Owner>>,
    closed: AtomicBool,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ResourceCleanup for Owner {
    async fn shutdown(&self) -> Result<()> {
        anyhow::ensure!(!self.closed.load(Ordering::Acquire), "duplicate cleanup");
        anyhow::ensure!(
            self.dependencies
                .values()
                .all(|owner| !owner.closed.load(Ordering::Acquire)),
            "dependency was closed before its user"
        );
        self.state.record("close", &self.id);
        if self.id == id("a-leaf") {
            if self.state.gate_cleanup.load(Ordering::Acquire) {
                self.state.entered.notify_one();
                self.state.release.notified().await;
            }
            if self.state.fail_cleanup.load(Ordering::Acquire) {
                self.state.entered.notify_one();
                anyhow::bail!("injected leaf cleanup failure");
            }
        }
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

struct Resolver(Arc<State>);

#[async_trait]
impl ManagementResourceResolver for Resolver {
    async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        self.resolve_with_dependencies(
            instance,
            graph,
            specification,
            configuration,
            &BTreeMap::new(),
        )
        .await
    }

    async fn resolve_with_dependencies(
        &self,
        _: &str,
        _: &str,
        specification: &ResourceSpecification,
        _: &serde_json::Value,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.0.record("open", &specification.id);
        if specification.id == id("m-middle") {
            if self.0.gate_create.load(Ordering::Acquire) {
                self.0.entered.notify_one();
                self.0.release.notified().await;
            }
            anyhow::ensure!(
                !self.0.fail_create.load(Ordering::Acquire),
                "injected creation failure"
            );
        }
        let dependencies: BTreeMap<_, _> = dependencies
            .iter()
            .map(|(id, handle)| Ok((id.clone(), handle.get::<Owner>()?)))
            .collect::<Result<_>>()?;
        for (id, owner) in &dependencies {
            anyhow::ensure!(
                Arc::ptr_eq(owner, &self.0.owner(id.as_str())),
                "not the actual dependency"
            );
            anyhow::ensure!(
                !owner.closed.load(Ordering::Acquire),
                "dependency is closed"
            );
        }
        let owner = Arc::new(Owner {
            id: specification.id.clone(),
            state: self.0.clone(),
            dependencies,
            closed: AtomicBool::new(false),
        });
        self.0.created.fetch_add(1, Ordering::SeqCst);
        self.0
            .latest
            .lock()
            .expect("owners")
            .insert(specification.id.clone(), Arc::downgrade(&owner));
        Ok(ResourceHandle::new(specification.role, owner.clone()).with_cleanup(owner))
    }
}

fn definition(revision: u32) -> DesiredInstance {
    let mut desired = DesiredInstance::default();
    for name in ["a-leaf", "m-middle", "z-root"] {
        desired.topology.resources.push(ResourceSpecification {
            id: id(name),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: name.into(),
        });
        desired.topology.resource_configurations.insert(
            id(name),
            serde_json::json!({"revision": if name == "z-root" { revision } else { 0 }}),
        );
    }
    desired.topology.resource_dependencies = BTreeMap::from([
        (
            id("a-leaf"),
            BTreeMap::from([(id("m-middle"), ResourceRole::StateStore)]),
        ),
        (
            id("m-middle"),
            BTreeMap::from([(id("z-root"), ResourceRole::StateStore)]),
        ),
    ]);
    desired
}

async fn open(state: &Arc<State>) -> Result<DrasiLib> {
    Ok(DrasiLib::builder()
        .with_id("resource-dependencies")
        .with_management_resources(Arc::new(Resolver(state.clone())))
        .build()
        .await?)
}

#[tokio::test]
async fn actual_dependencies_order_construction_replacement_and_shutdown() -> Result<()> {
    let state = Arc::new(State::default());
    let core = open(&state).await?;
    core.apply_desired_state(0, "initial", definition(0))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(
        state.calls(),
        ["open:z-root", "open:m-middle", "open:a-leaf"]
    );
    let old = state.owner("z-root");
    assert_eq!(state.owner("a-leaf").dependencies.len(), 1);
    let snapshot = core.snapshot_computation_configuration().await?;
    let topology = &snapshot
        .native_components
        .expect("native resources")
        .topology;
    assert_eq!(
        topology.resource_dependencies,
        definition(0).topology.resource_dependencies
    );
    assert_eq!(
        DesiredTopology::from_json(&topology.to_json()?)?.resource_dependencies,
        topology.resource_dependencies
    );

    core.apply_desired_state(1, "replace-root", definition(1))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert!(old.closed.load(Ordering::Acquire));
    assert!(!Arc::ptr_eq(&old, &state.owner("z-root")));
    core.shutdown().await?;
    assert_eq!(
        state.calls(),
        [
            "open:z-root",
            "open:m-middle",
            "open:a-leaf",
            "close:a-leaf",
            "close:m-middle",
            "close:z-root",
            "open:z-root",
            "open:m-middle",
            "open:a-leaf",
            "close:a-leaf",
            "close:m-middle",
            "close:z-root",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn failed_cleanup_retains_all_dependencies_until_retry() -> Result<()> {
    let state = Arc::new(State::default());
    let core = open(&state).await?;
    core.apply_desired_state(0, "initial", definition(0))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    let root = state.owner("z-root");
    state.fail_cleanup.store(true, Ordering::Release);
    core.apply_desired_state(1, "replace", definition(1))
        .await?;
    let status = core.management_status().await?;
    assert!(!status.converged());
    assert!(!root.closed.load(Ordering::Acquire));
    assert!(!state
        .calls()
        .iter()
        .any(|call| call == "close:m-middle" || call == "close:z-root"));
    assert_eq!(
        state
            .calls()
            .iter()
            .filter(|call| call.starts_with("open:"))
            .count(),
        3
    );
    state.fail_cleanup.store(false, Ordering::Release);
    assert!(core.reconcile_desired_state().await?.converged());
    assert!(root.closed.load(Ordering::Acquire));
    core.shutdown().await?;
    Ok(())
}

async fn sustained_dependency_churn() -> Result<()> {
    const CYCLES: u32 = 128;
    let state = Arc::new(State::default());
    let core = open(&state).await?;
    let mut revision = 0;
    for cycle in 0..CYCLES {
        state.calls.lock().expect("calls").clear();
        core.apply_desired_state(revision, &format!("add-{cycle}"), definition(cycle))
            .await?;
        revision += 1;
        assert!(core.reconcile_desired_state().await?.converged());
        let old: Vec<_> = ["a-leaf", "m-middle", "z-root"]
            .into_iter()
            .map(|name| Arc::downgrade(&state.owner(name)))
            .collect();
        assert_eq!(
            state.created.load(Ordering::SeqCst) - state.dropped.load(Ordering::SeqCst),
            3
        );

        state.fail_cleanup.store(true, Ordering::Release);
        core.apply_desired_state(
            revision,
            &format!("replace-{cycle}"),
            definition(cycle + CYCLES),
        )
        .await?;
        revision += 1;
        state.entered.notified().await;
        assert!(!core.management_status().await?.converged());
        assert!(old.iter().all(|owner| {
            owner
                .upgrade()
                .is_some_and(|owner| !owner.closed.load(Ordering::Acquire))
        }));
        assert_eq!(
            state.created.load(Ordering::SeqCst) - state.dropped.load(Ordering::SeqCst),
            3,
            "failed cleanup must not construct a replacement generation"
        );
        state.fail_cleanup.store(false, Ordering::Release);
        assert!(core.reconcile_desired_state().await?.converged());
        assert!(old.iter().all(|owner| owner.upgrade().is_none()));

        core.apply_desired_state(
            revision,
            &format!("remove-{cycle}"),
            DesiredInstance::default(),
        )
        .await?;
        revision += 1;
        assert!(core.reconcile_desired_state().await?.converged());
        assert!(state
            .latest
            .lock()
            .expect("owners")
            .values()
            .all(|owner| owner.upgrade().is_none()));
        assert_eq!(
            state.created.load(Ordering::SeqCst),
            (cycle as usize + 1) * 6
        );
        assert_eq!(
            state.created.load(Ordering::SeqCst),
            state.dropped.load(Ordering::SeqCst),
            "resource owners accumulated after cycle {cycle}"
        );
        assert_eq!(
            state.calls(),
            [
                "open:z-root",
                "open:m-middle",
                "open:a-leaf",
                "close:a-leaf",
                "close:a-leaf",
                "close:m-middle",
                "close:z-root",
                "open:z-root",
                "open:m-middle",
                "open:a-leaf",
                "close:a-leaf",
                "close:m-middle",
                "close:z-root",
            ]
        );
    }
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sustained_dependency_churn_current_thread() -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        sustained_dependency_churn(),
    )
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sustained_dependency_churn_multi_thread() -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        sustained_dependency_churn(),
    )
    .await?
}

#[tokio::test]
async fn failed_dependency_blocks_only_its_descendants_and_retry_reuses_parent() -> Result<()> {
    let state = Arc::new(State::default());
    state.fail_create.store(true, Ordering::Release);
    let core = open(&state).await?;
    let mut desired = definition(0);
    desired.topology.resources.push(ResourceSpecification {
        id: id("zz-unrelated"),
        role: ResourceRole::StateStore,
        ownership: ResourceOwnership::Graph,
        binding: "unrelated".into(),
    });
    desired
        .topology
        .resource_configurations
        .insert(id("zz-unrelated"), serde_json::json!({}));
    core.apply_desired_state(0, "initial", desired).await?;
    let status = core.management_status().await?;
    assert!(!status.converged());
    assert!(status.resource_errors.contains_key(&id("m-middle")));
    assert!(status.resource_errors.contains_key(&id("a-leaf")));
    assert_eq!(
        state.calls(),
        ["open:z-root", "open:m-middle", "open:zz-unrelated"]
    );
    let root = state.owner("z-root");
    state.fail_create.store(false, Ordering::Release);
    assert!(core.reconcile_desired_state().await?.converged());
    assert!(Arc::ptr_eq(&root, &state.owner("z-root")));
    assert_eq!(
        state.calls(),
        [
            "open:z-root",
            "open:m-middle",
            "open:zz-unrelated",
            "open:m-middle",
            "open:a-leaf"
        ]
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dependency_construction_timeout_keeps_parent_and_does_not_construct_descendants(
) -> Result<()> {
    let state = Arc::new(State::default());
    state.gate_create.store(true, Ordering::Release);
    let core = open(&state).await?;
    core.apply_desired_state(0, "initial", definition(0))
        .await?;
    state.entered.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    let status = core.management_status().await?;
    assert!(!status.converged());
    assert_eq!(state.calls(), ["open:z-root", "open:m-middle"]);
    assert!(!state.owner("z-root").closed.load(Ordering::Acquire));
    state.gate_create.store(false, Ordering::Release);
    assert!(core.reconcile_desired_state().await?.converged());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn cancelled_dispose_retains_dependencies_and_retry_finishes_in_reverse_order() -> Result<()>
{
    let state = Arc::new(State::default());
    let resolver = Resolver(state.clone());
    let desired = definition(0).topology;
    let mut bindings = TopologyBindings::default();
    for resource in desired.resource_construction_order()? {
        let dependencies = desired
            .resource_dependencies
            .get(&resource)
            .into_iter()
            .flat_map(|required| required.keys())
            .map(|id| (id.clone(), bindings.resources[id].clone()))
            .collect();
        let spec = desired
            .resources
            .iter()
            .find(|spec| spec.id == resource)
            .expect("specification");
        let handle = resolver
            .resolve_with_dependencies(
                "instance",
                &desired.graph_id,
                spec,
                &serde_json::json!({}),
                &dependencies,
            )
            .await?;
        bindings.resources.insert(resource, handle);
    }
    let mut graph = desired.build(bindings)?;
    state.gate_cleanup.store(true, Ordering::Release);
    tokio::select! {
        result = graph.dispose() => panic!("cleanup unexpectedly finished: {result:?}"),
        _ = state.entered.notified() => {}
    }
    assert!(!state.owner("z-root").closed.load(Ordering::Acquire));
    assert!(!state.owner("m-middle").closed.load(Ordering::Acquire));
    state.gate_cleanup.store(false, Ordering::Release);
    graph.dispose().await?;
    assert_eq!(
        state.calls(),
        [
            "open:z-root",
            "open:m-middle",
            "open:a-leaf",
            "close:a-leaf",
            "close:a-leaf",
            "close:m-middle",
            "close:z-root"
        ]
    );
    graph.dispose().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_dependency_definitions_are_rejected_before_acceptance_or_effects() -> Result<()> {
    let state = Arc::new(State::default());
    let core = open(&state).await?;
    let mut cases = Vec::new();
    let mut cycle = definition(0);
    cycle.topology.resource_dependencies.insert(
        id("z-root"),
        BTreeMap::from([(id("a-leaf"), ResourceRole::StateStore)]),
    );
    cases.push(cycle);
    let mut missing = definition(0);
    missing
        .topology
        .resources
        .retain(|resource| resource.id != id("z-root"));
    missing
        .topology
        .resource_configurations
        .remove(&id("z-root"));
    cases.push(missing);
    let mut wrong_role = definition(0);
    wrong_role
        .topology
        .resource_dependencies
        .get_mut(&id("a-leaf"))
        .expect("dependencies")
        .insert(id("m-middle"), ResourceRole::IndexBackend);
    cases.push(wrong_role);
    let mut borrowed = definition(0);
    borrowed.topology.resources[0].ownership = ResourceOwnership::Borrowed;
    cases.push(borrowed);
    for (number, desired) in cases.into_iter().enumerate() {
        let request = format!("invalid-{number}");
        assert!(core
            .apply_desired_state(0, &request, desired)
            .await
            .is_err());
        assert!(core.configuration_receipt(request).await?.is_none());
    }
    assert!(state.calls().is_empty());
    assert_eq!(core.desired_configuration()?.revision, 0);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resource_removal_requires_dependents_to_be_removed_explicitly_or_cascaded() -> Result<()> {
    let state = Arc::new(State::default());
    let core = open(&state).await?;
    core.apply_desired_state(0, "initial", definition(0))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    let control = core.computation_control()?;
    let revision = control.desired_snapshot().revision;
    assert!(control
        .preview(
            revision,
            vec![DesiredMutation::RemoveResource {
                resource: id("z-root"),
                policy: RemovalPolicy::Reject
            }]
        )
        .await
        .is_err());
    let preview = control
        .preview(
            revision,
            vec![DesiredMutation::RemoveResource {
                resource: id("z-root"),
                policy: RemovalPolicy::Cascade,
            }],
        )
        .await?;
    assert!(preview
        .desired()
        .resources
        .iter()
        .all(|resource| !["a-leaf", "m-middle", "z-root"].contains(&resource.id.as_str())));
    assert!(preview.desired().resource_dependencies.is_empty());
    core.apply_desired_state(1, "remove-tree", DesiredInstance::default())
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(
        &state.calls()[3..],
        ["close:a-leaf", "close:m-middle", "close:z-root"]
    );
    core.shutdown().await?;
    Ok(())
}

struct UserFactory(FactoryDescriptor);
#[async_trait]
impl ComponentFactory for UserFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        _: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Err(ComponentCreationError::terminal(anyhow::anyhow!(
            "snapshot-only fixture"
        )))
    }
}

#[test]
fn selected_snapshots_and_inspection_preserve_transitive_resource_dependencies() -> Result<()> {
    let mut desired = definition(0).topology;
    let implementation = ImplementationIdentity::try_new("test/resource-user", "1")?;
    let factory = Arc::new(UserFactory(FactoryDescriptor {
        implementation: implementation.clone(),
        role: ComponentRole::Service,
        configuration_version: 1,
        configuration: ConfigurationSchema::default(),
        dependencies: BTreeMap::from([(
            Arc::from("resource"),
            ResourceRequirement::exactly_one::<Owner>(ResourceRole::StateStore),
        )]),
    }));
    let component = ComponentId::try_new("user")?;
    let descriptor = ComponentDescriptor::try_new(component.clone(), vec![])?;
    desired.components.push(DesiredComponent {
        descriptor: descriptor.clone(),
        role: ComponentRole::Service,
        completion: None,
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::default(),
        construction: ComponentConstruction::Factory(ComponentSpecification {
            descriptor,
            role: ComponentRole::Service,
            completion: None,
            implementation,
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::from([(Arc::from("resource"), vec![id("a-leaf")])]),
        }),
    });
    let mut bindings = TopologyBindings::default();
    bindings.factories.register(factory)?;
    let graph = desired.build(bindings)?;
    let selected = graph
        .snapshot()
        .select(GraphSelection::Exact(vec![component]))?;
    assert_eq!(selected.resources.len(), 3);
    assert_eq!(
        selected.resource_dependencies,
        desired.resource_dependencies
    );
    selected.validate_structure()?;
    let topology = graph.inspector().topology();
    assert!(topology.links.contains(&GraphEntityLink {
        from: GraphEntityId::Resource(id("a-leaf")),
        to: GraphEntityId::Resource(id("m-middle")),
        kind: GraphEntityLinkKind::UsesResource,
    }));
    assert!(topology.links.contains(&GraphEntityLink {
        from: GraphEntityId::Resource(id("m-middle")),
        to: GraphEntityId::Resource(id("z-root")),
        kind: GraphEntityLinkKind::UsesResource,
    }));
    Ok(())
}

struct IndependentResolver(Resolver);
#[async_trait]
impl ManagementResourceResolver for IndependentResolver {
    async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        self.0
            .resolve(instance, graph, specification, configuration)
            .await
    }
}

#[tokio::test]
async fn existing_resolvers_cannot_silently_ignore_declared_dependencies() -> Result<()> {
    let state = Arc::new(State::default());
    let core = DrasiLib::builder()
        .with_management_resources(Arc::new(IndependentResolver(Resolver(state.clone()))))
        .build()
        .await?;
    core.apply_desired_state(0, "initial", definition(0))
        .await?;
    let status = core.reconcile_desired_state().await?;
    assert!(!status.converged());
    assert_eq!(state.calls(), ["open:z-root"]);
    assert!(status.resource_errors.contains_key(&id("m-middle")));
    assert!(status.resource_errors.contains_key(&id("a-leaf")));
    core.shutdown().await?;
    assert_eq!(state.calls(), ["open:z-root", "close:z-root"]);
    Ok(())
}
