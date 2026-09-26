// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib};
use drasi_state_store_redb::RedbConfigurationStore;
use serde_json::{json, Value};

struct Service {
    descriptor: ComponentDescriptor,
    config: Value,
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(self.config.clone())
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> Result<()> {
        std::future::pending().await
    }
}

struct Factory {
    descriptor: FactoryDescriptor,
    created: Arc<AtomicUsize>,
}

impl Factory {
    fn new(created: Arc<AtomicUsize>) -> Self {
        Self {
            created,
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/managed-service", "1")
                    .unwrap(),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::new(),
                    allow_additional: true,
                },
                dependencies: BTreeMap::new(),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        if std::env::var_os("DRASI_TEST_CRASH_DURING_CREATE").is_some() {
            std::process::exit(72);
        }
        if context.configuration().get("fail") == Some(&json!(true)) {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "deliberate constructor failure"
            )));
        }
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
            config: serde_json::to_value(context.configuration()).unwrap(),
        })))
    }
}

fn registry(created: Arc<AtomicUsize>) -> FactoryRegistry {
    let mut factories = FactoryRegistry::standard();
    factories.register(Arc::new(Factory::new(created))).unwrap();
    factories
}

fn desired(values: &[(&str, bool)]) -> DesiredInstance {
    let mut definition = ComputationGraph::empty("application")
        .unwrap()
        .snapshot()
        .select(GraphSelection::All)
        .unwrap();
    definition.components = values
        .iter()
        .map(|(id, fail)| {
            let descriptor =
                ComponentDescriptor::try_new(ComponentId::try_new(*id).unwrap(), Vec::new())
                    .unwrap();
            DesiredComponent {
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
                    implementation: ImplementationIdentity::try_new("test/managed-service", "1")
                        .unwrap(),
                    configuration_version: 1,
                    configuration: [(Arc::from("fail"), ConfigurationValue::Literal(json!(fail)))]
                        .into(),
                    dependencies: BTreeMap::new(),
                }),
            }
        })
        .collect();
    definition.into()
}

async fn open(
    store: Arc<RedbConfigurationStore>,
    name: &str,
    count: Arc<AtomicUsize>,
) -> Result<DrasiLib> {
    Ok(DrasiLib::builder()
        .with_id(name)
        .with_component_factories(registry(count))
        .with_configuration_store(store)
        .build()
        .await?)
}

#[tokio::test]
async fn persistence_is_optional_and_object_injection_remains_lightweight() -> Result<()> {
    let core = DrasiLib::builder().build().await?;
    assert!(core.desired_configuration().is_err());
    let descriptor = ComponentDescriptor::try_new(ComponentId::try_new("injected")?, vec![])?;
    core.add_computation_component(ComponentAddition::new(ConstructedComponent::service(
        Box::new(Service {
            descriptor,
            config: json!({}),
        }),
    )))
    .await?
    .wait_created()
    .await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn desired_diff_retains_healthy_instances_and_replaces_changed_or_removes_absent(
) -> Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let core = DrasiLib::builder()
        .with_component_factories(registry(count.clone()))
        .build()
        .await?;
    let first = core
        .apply_desired_state(0, "first", desired(&[("a", false), ("b", false)]))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    let handle = core.get_computation_graph("application").await?;
    let before = handle.observed();
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let repeated = core
        .apply_desired_state(0, "first", desired(&[("a", false), ("b", false)]))
        .await?;
    assert_eq!(first, repeated);
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(count.load(Ordering::SeqCst), 2);
    let second = core
        .apply_desired_state(
            first.revision,
            "second",
            desired(&[("a", false), ("c", false)]),
        )
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert_eq!(
        handle.observed().components[&ComponentId::try_new("a")?].generation,
        before.components[&ComponentId::try_new("a")?].generation
    );
    assert!(!handle
        .observed()
        .components
        .contains_key(&ComponentId::try_new("b")?));
    assert!(core
        .apply_desired_state(0, "stale", DesiredInstance::default())
        .await
        .is_err());
    assert!(core
        .apply_desired_state(second.revision, "first", DesiredInstance::default())
        .await
        .is_err());
    assert!(
        handle
            .control()
            .set_lifecycle_policy(
                handle.desired().revision,
                ComponentId::try_new("a")?,
                LifecyclePolicy { auto_start: false }
            )
            .await
            .is_err(),
        "raw management bypass rejected"
    );
    core.start().await?;
    core.stop().await?;
    core.apply_desired_state(second.revision, "remove-all", DesiredInstance::default())
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert!(core.get_computation_graph("application").await.is_err());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn redb_recovers_every_declaration_and_isolates_instances_snapshots_and_receipts(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("definitions.redb");
    let count = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(RedbConfigurationStore::new(&path, [42; 32])?);
    let a = open(store.clone(), "a", count.clone()).await?;
    let b = open(store.clone(), "b", count.clone()).await?;
    assert!(open(store.clone(), "a", count.clone()).await.is_err());
    a.apply_desired_state(0, "request", desired(&[("good", false), ("bad", true)]))
        .await?;
    let report = a.reconcile_desired_state().await?;
    assert!(!report.converged());
    let observed = a.get_computation_graph("application").await?.observed();
    assert_eq!(
        observed.components[&ComponentId::try_new("bad")?].realization,
        RealizationState::CreationFailed
    );
    assert!(observed.components[&ComponentId::try_new("bad")?]
        .failure
        .is_some());
    assert_eq!(
        a.desired_configuration()?.desired.graphs[0]
            .topology
            .components
            .len(),
        2
    );
    assert_eq!(
        a.snapshot_desired_configuration("before").await?.revision,
        1
    );
    b.apply_desired_state(0, "request", desired(&[("only-b", false)]))
        .await?;
    b.reconcile_desired_state().await?;
    assert_eq!(
        b.desired_configuration()?.desired.graphs[0]
            .topology
            .components
            .len(),
        1
    );
    a.shutdown().await?;
    let restored = open(store.clone(), "a", count.clone()).await?;
    assert_eq!(restored.desired_configuration()?.revision, 1);
    assert_eq!(
        restored
            .configuration_receipt("request")
            .await?
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        restored
            .get_computation_graph("application")
            .await?
            .observed()
            .components
            .len(),
        2
    );
    restored
        .apply_desired_state(1, "fix", desired(&[("good", false), ("bad", false)]))
        .await?;
    assert!(restored.reconcile_desired_state().await?.converged());
    restored
        .restore_configuration_snapshot("before", 2, "restore")
        .await?;
    assert!(!restored.reconcile_desired_state().await?.converged());
    assert_eq!(restored.desired_configuration()?.revision, 3);
    restored.shutdown().await?;
    b.shutdown().await?;
    drop((a, b, restored, store));
    let reopened = Arc::new(RedbConfigurationStore::new(&path, [42; 32])?);
    let a = open(reopened, "a", count).await?;
    assert_eq!(a.desired_configuration()?.revision, 3);
    a.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unavailable_factory_is_visible_and_can_be_registered_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("config.redb"),
        [7; 32],
    )?);
    let core = DrasiLib::builder()
        .with_configuration_store(store.clone())
        .build()
        .await?;
    core.apply_desired_state(0, "add", desired(&[("missing", false)]))
        .await?;
    assert!(!core.reconcile_desired_state().await?.converged());
    assert!(core
        .get_computation_graph("application")
        .await?
        .observed()
        .components
        .contains_key(&ComponentId::try_new("missing")?));
    let count = Arc::new(AtomicUsize::new(0));
    core.register_component_factory(Arc::new(Factory::new(count.clone())))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(core
        .add_query(
            drasi_lib::Query::cypher("opaque-query")
                .query("RETURN 1")
                .build()
        )
        .await
        .is_err());
    assert!(core
        .add_computation_graph(
            ComputationGraph::empty("untracked")?,
            ComputationOptions::default()
        )
        .await
        .is_err());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stored_definitions_and_snapshots_are_encrypted_and_wrong_keys_fail() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("encrypted.redb");
    let store = RedbConfigurationStore::new(&path, [9; 32])?;
    let session = store.open("instance").await?;
    let mut definition = desired(&[("credential-component", false)]);
    let ComponentConstruction::Factory(spec) =
        &mut definition.graphs[0].topology.components[0].construction
    else {
        unreachable!()
    };
    let secret = "recognizable-password-that-must-not-occur-on-disk";
    spec.configuration.insert(
        Arc::from("credential"),
        ConfigurationValue::Literal(json!(secret)),
    );
    session.commit(0, "request", &definition).await?;
    session.snapshot("saved").await?;
    session.close().await?;
    drop((session, store));
    let bytes = std::fs::read(&path)?;
    assert!(!bytes
        .windows(secret.len())
        .any(|window| window == secret.as_bytes()));
    assert!(RedbConfigurationStore::new(&path, [8; 32]).is_err());
    Ok(())
}

#[tokio::test]
async fn concurrent_revision_writers_cannot_overwrite_each_other() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = RedbConfigurationStore::new(directory.path().join("config.redb"), [5; 32])?;
    let session = store.open("instance").await?;
    let one = desired(&[("one", false)]);
    let two = desired(&[("two", false)]);
    let (a, b) = tokio::join!(session.commit(0, "a", &one), session.commit(0, "b", &two));
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(session.load().await?.revision, 1);
    session.close().await?;
    assert!(session.commit(1, "closed", &one).await.is_err());
    let again = store.open("instance").await?;
    assert_eq!(again.load().await?.revision, 1);
    again.close().await?;
    Ok(())
}

#[tokio::test]
async fn factory_objects_in_persistent_builder_are_explicitly_rejected() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("config.redb"),
        [1; 32],
    )?);
    let result = DrasiLib::builder()
        .with_configuration_store(store)
        .with_query(
            drasi_lib::Query::cypher("not-a-recipe")
                .query("RETURN 1")
                .build(),
        )
        .build()
        .await;
    assert!(result.is_err());
    Ok(())
}

struct FaultStore {
    inner: Arc<RedbConfigurationStore>,
    fault: Arc<AtomicUsize>,
}
struct FaultSession {
    inner: Arc<dyn ConfigurationSession>,
    fault: Arc<AtomicUsize>,
}
#[async_trait]
impl ConfigurationStore for FaultStore {
    async fn open(&self, id: &str) -> Result<Arc<dyn ConfigurationSession>> {
        Ok(Arc::new(FaultSession {
            inner: self.inner.open(id).await?,
            fault: self.fault.clone(),
        }))
    }
}
#[async_trait]
impl ConfigurationSession for FaultSession {
    async fn load(&self) -> Result<CommittedConfiguration> {
        self.inner.load().await
    }
    async fn commit(
        &self,
        expected: u64,
        id: &str,
        definition: &DesiredInstance,
    ) -> Result<AcceptanceReceipt> {
        let fault = self.fault.swap(0, Ordering::SeqCst);
        if fault == 1 {
            anyhow::bail!("injected failure before commit");
        }
        if fault == 4 {
            std::process::exit(70);
        }
        let receipt = self.inner.commit(expected, id, definition).await?;
        if fault == 2 {
            anyhow::bail!("injected lost response after commit");
        }
        if fault == 3 {
            std::process::exit(71);
        }
        Ok(receipt)
    }
    async fn receipt(&self, id: &str) -> Result<Option<AcceptanceReceipt>> {
        self.inner.receipt(id).await
    }
    async fn snapshot(&self, id: &str) -> Result<CommittedConfiguration> {
        self.inner.snapshot(id).await
    }
    async fn load_snapshot(&self, id: &str) -> Result<Option<CommittedConfiguration>> {
        self.inner.load_snapshot(id).await
    }
    async fn close(&self) -> Result<()> {
        self.inner.close().await
    }
}

#[tokio::test]
async fn commit_failure_prevents_construction_and_lost_response_is_resolved_by_request_id(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("fault.redb"),
        [4; 32],
    )?);
    let count = Arc::new(AtomicUsize::new(0));
    let fault = Arc::new(AtomicUsize::new(1));
    let core = DrasiLib::builder()
        .with_id("fault")
        .with_component_factories(registry(count.clone()))
        .with_configuration_store(Arc::new(FaultStore {
            inner: store.clone(),
            fault: fault.clone(),
        }))
        .build()
        .await?;
    assert!(core
        .apply_desired_state(0, "before", desired(&[("saved", false)]))
        .await
        .is_err());
    assert_eq!(core.desired_configuration()?.revision, 0);
    assert!(core.configuration_receipt("before").await?.is_none());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    fault.store(2, Ordering::SeqCst);
    assert!(core
        .apply_desired_state(0, "after", desired(&[("saved", false)]))
        .await
        .is_err());
    assert_eq!(
        core.configuration_receipt("after").await?.unwrap().revision,
        1
    );
    core.shutdown().await?;
    let restored = open(store, "fault", count.clone()).await?;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let receipt = restored
        .apply_desired_state(0, "after", desired(&[("saved", false)]))
        .await?;
    assert_eq!(receipt.revision, 1);
    assert!(receipt.durable);
    assert!(restored.reconcile_desired_state().await?.converged());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    restored.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "subprocess crash worker invoked by the crash recovery test"]
async fn management_crash_worker() -> Result<()> {
    let path = std::env::var_os("DRASI_MANAGEMENT_CRASH_PATH").expect("worker path");
    let phase: usize = std::env::var("DRASI_MANAGEMENT_CRASH_PHASE")?.parse()?;
    let store = Arc::new(RedbConfigurationStore::new(path, [31; 32])?);
    let core = DrasiLib::builder()
        .with_id("crash")
        .with_component_factories(registry(Arc::new(AtomicUsize::new(0))))
        .with_configuration_store(Arc::new(FaultStore {
            inner: store,
            fault: Arc::new(AtomicUsize::new(phase)),
        }))
        .build()
        .await?;
    core.apply_desired_state(
        0,
        "crash-request",
        desired(&[("retained", false), ("failed", true)]),
    )
    .await?;
    core.reconcile_desired_state().await?;
    anyhow::bail!("crash worker unexpectedly completed")
}

#[test]
fn process_crashes_before_commit_after_commit_and_during_creation_recover_exact_intent(
) -> Result<()> {
    for (phase, code) in [(4, 70), (3, 71), (0, 72)] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("crash.redb");
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "management_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("DRASI_MANAGEMENT_CRASH_PATH", &path)
            .env("DRASI_MANAGEMENT_CRASH_PHASE", phase.to_string());
        if phase == 0 {
            command.env("DRASI_TEST_CRASH_DURING_CREATE", "1");
        }
        let result = command.output()?;
        assert_eq!(
            result.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let store = Arc::new(RedbConfigurationStore::new(&path, [31; 32])?);
                let core = open(store, "crash", Arc::new(AtomicUsize::new(0))).await?;
                assert_eq!(
                    core.desired_configuration()?.revision,
                    if phase == 4 { 0 } else { 1 }
                );
                if phase != 4 {
                    assert_eq!(
                        core.configuration_receipt("crash-request")
                            .await?
                            .unwrap()
                            .revision,
                        1
                    );
                    let graph = core.get_computation_graph("application").await?;
                    assert_eq!(graph.observed().components.len(), 2);
                    assert_eq!(
                        graph.observed().components[&ComponentId::try_new("failed")?].realization,
                        RealizationState::CreationFailed
                    );
                }
                core.shutdown().await?;
                anyhow::Ok(())
            })?;
    }
    Ok(())
}

#[tokio::test]
async fn blocked_creation_does_not_delay_durable_acceptance_and_cancelled_waits_do_not_lose_work(
) -> Result<()> {
    struct BlockingFactory {
        inner: Factory,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ComponentFactory for BlockingFactory {
        fn descriptor(&self) -> &FactoryDescriptor {
            self.inner.descriptor()
        }
        fn validate(&self, spec: &ComponentSpecification) -> Result<()> {
            self.inner.validate(spec)
        }
        async fn create(
            &self,
            context: ConstructionContext,
        ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.create(context).await
        }
    }
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("pending.redb"),
        [12; 32],
    )?);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut factories = FactoryRegistry::standard();
    factories.register(Arc::new(BlockingFactory {
        inner: Factory::new(Arc::new(AtomicUsize::new(0))),
        entered: entered.clone(),
        release: release.clone(),
    }))?;
    let core = DrasiLib::builder()
        .with_component_factories(factories)
        .with_configuration_store(store)
        .build()
        .await?;
    let receipt = tokio::time::timeout(
        Duration::from_secs(2),
        core.apply_desired_state(0, "pending", desired(&[("pending", false)])),
    )
    .await??;
    assert!(receipt.durable);
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    assert!(!core.management_status().await?.converged());
    assert_eq!(
        core.get_computation_graph("application")
            .await?
            .observed()
            .components[&ComponentId::try_new("pending")?]
            .realization,
        RealizationState::Creating
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), core.reconcile_desired_state())
            .await
            .is_err()
    );
    release.notify_one();
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(core.desired_configuration()?.revision, 1);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resource_recipe_changes_preserve_cleanup_ownership_and_retry_the_committed_target(
) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    struct Cleanup {
        fail: Arc<AtomicBool>,
        cleaned: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ResourceCleanup for Cleanup {
        async fn shutdown(&self) -> Result<()> {
            if self.fail.swap(false, Ordering::SeqCst) {
                anyhow::bail!("injected provider cleanup failure");
            }
            self.cleaned.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    struct Resources {
        fail: Arc<AtomicBool>,
        created: Arc<AtomicUsize>,
        cleaned: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ManagementResourceResolver for Resources {
        async fn resolve(
            &self,
            _: &str,
            _: &str,
            spec: &ResourceSpecification,
            value: &Value,
        ) -> Result<ResourceHandle> {
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(ResourceHandle::new(
                spec.role,
                Arc::new(value["value"].as_u64().unwrap() as usize),
            )
            .with_cleanup(Arc::new(Cleanup {
                fail: self.fail.clone(),
                cleaned: self.cleaned.clone(),
            })))
        }
    }
    let fail = Arc::new(AtomicBool::new(false));
    let created = Arc::new(AtomicUsize::new(0));
    let cleaned = Arc::new(AtomicUsize::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let mut factory = Factory::new(count.clone());
    factory.descriptor.dependencies.insert(
        Arc::from("provider"),
        ResourceRequirement::exactly_one::<usize>(ResourceRole::StateStore),
    );
    let mut factories = FactoryRegistry::standard();
    factories.register(Arc::new(factory))?;
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("resources.redb"),
        [11; 32],
    )?);
    let core = DrasiLib::builder()
        .with_component_factories(factories)
        .with_configuration_store(store)
        .with_management_resources(Arc::new(Resources {
            fail: fail.clone(),
            created: created.clone(),
            cleaned: cleaned.clone(),
        }))
        .build()
        .await?;
    let mut first = desired(&[("component", false)]);
    let resource = ResourceId::try_new("state")?;
    first.graphs[0]
        .topology
        .resources
        .push(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from("provider"),
        });
    first.graphs[0]
        .topology
        .resource_configurations
        .insert(resource.clone(), json!({"value":1}));
    let ComponentConstruction::Factory(spec) =
        &mut first.graphs[0].topology.components[0].construction
    else {
        unreachable!()
    };
    spec.dependencies
        .insert(Arc::from("provider"), vec![resource.clone()]);
    core.apply_desired_state(0, "one", first.clone()).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(created.load(Ordering::SeqCst), 1);
    core.apply_desired_state(1, "same", first.clone()).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(created.load(Ordering::SeqCst), 1);
    fail.store(true, Ordering::SeqCst);
    first.graphs[0]
        .topology
        .resource_configurations
        .insert(resource, json!({"value":2}));
    core.apply_desired_state(1, "two", first).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(core.desired_configuration()?.revision, 2);
    assert!(cleaned.load(Ordering::SeqCst) >= 2);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn desired_changes_do_not_implicitly_adopt_programmatically_added_graphs() -> Result<()> {
    let core = DrasiLib::builder()
        .with_component_factories(registry(Arc::new(AtomicUsize::new(0))))
        .with_computation_graph(
            ComputationGraph::empty("application")?,
            ComputationOptions { auto_start: false },
        )
        .build()
        .await?;
    assert!(core
        .apply_desired_state(0, "adopt", desired(&[("new", false)]))
        .await
        .is_err());
    assert_eq!(core.desired_configuration()?.revision, 0);
    core.shutdown().await?;
    Ok(())
}
