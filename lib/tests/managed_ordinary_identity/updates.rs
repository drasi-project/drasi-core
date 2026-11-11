// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{load, name, FileStore, OwnedFile, Provider};
use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

struct DataFile {
    path: PathBuf,
    writes: Mutex<()>,
}

impl DataFile {
    fn new(path: PathBuf) -> anyhow::Result<Arc<Self>> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::fs::write(&path, "0")?;
        Ok(Arc::new(Self {
            path,
            writes: Mutex::new(()),
        }))
    }

    fn value(&self) -> anyhow::Result<i64> {
        Ok(serde_json::from_slice(&std::fs::read(&self.path)?)?)
    }

    fn add(&self, value: i64) -> anyhow::Result<i64> {
        use std::io::Write;
        let _lock = self.writes.lock().unwrap();
        let next = self.value()? + value;
        let staging = self.path.with_extension("processing-next");
        let mut file = std::fs::File::create(&staging)?;
        write!(file, "{next}")?;
        file.sync_all()?;
        std::fs::rename(staging, &self.path)?;
        Ok(next)
    }
}

impl Drop for DataFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("processing-next"));
    }
}

#[derive(Debug, Clone)]
struct Event {
    hook: &'static str,
    generation: ComponentGeneration,
    factor: i64,
    operation: Option<String>,
}

struct Factory {
    descriptor: FactoryDescriptor,
    plan: ComponentUpdatePlan,
    supported: bool,
    events: Mutex<Vec<Event>>,
    input: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<i64>>>,
    output: mpsc::UnboundedSender<i64>,
    data: Arc<DataFile>,
    core: Mutex<Weak<DrasiLib>>,
    gate: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Factory {
    fn new(
        plan: ComponentUpdatePlan,
        supported: bool,
        data: Arc<DataFile>,
    ) -> (
        Arc<Self>,
        mpsc::UnboundedSender<i64>,
        mpsc::UnboundedReceiver<i64>,
    ) {
        let (input, receiver) = mpsc::unbounded_channel();
        let (output, results) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                descriptor: FactoryDescriptor {
                    implementation: ImplementationIdentity::try_new("fixture/owned-updates", "1")
                        .unwrap(),
                    role: ComponentRole::Service,
                    configuration_version: 1,
                    configuration: ConfigurationSchema {
                        fields: BTreeMap::new(),
                        allow_additional: true,
                    },
                    dependencies: BTreeMap::new(),
                },
                plan,
                supported,
                events: Mutex::new(Vec::new()),
                input: Arc::new(tokio::sync::Mutex::new(receiver)),
                output,
                data,
                core: Mutex::new(Weak::new()),
                gate: AtomicBool::new(false),
                entered: Notify::new(),
                release: Notify::new(),
            }),
            input,
            results,
        )
    }

    fn record(
        &self,
        hook: &'static str,
        generation: ComponentGeneration,
        factor: i64,
        operation: Option<String>,
    ) {
        self.events.lock().unwrap().push(Event {
            hook,
            generation,
            factor,
            operation,
        });
    }

    fn hooks(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.hook)
            .collect()
    }

    fn assert_accepted(&self, id: &ComponentId, operation: &str) {
        let core = self.core.lock().unwrap().upgrade();
        let record = match core {
            Some(core) => core.desired_configuration().unwrap(),
            None => {
                load(&self.data.path.with_extension("yaml"))
                    .unwrap()
                    .current
            }
        };
        assert!(record
            .pending_updates
            .iter()
            .any(|update| &update.component_id == id && update.operation_id == operation));
    }

    async fn pause_update(&self) {
        if self.gate.load(Ordering::Acquire) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

struct RegisteredFactory(Arc<Factory>);

#[async_trait]
impl ComponentFactory for RegisteredFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0.descriptor
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.0.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("factor"),
                ConfigurationValue::Literal(configuration["factor"].clone()),
            )]),
            dependencies: BTreeMap::new(),
        })
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(specification.configuration.get("factor"),
            Some(ConfigurationValue::Literal(value)) if value.as_i64().is_some_and(|value| value > 0)),
            "factor must be a positive integer"
        );
        Ok(())
    }

    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(self.0.supported, "fixture author does not support updates");
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.implementation == proposed.implementation,
            "immutable interface changed"
        );
        self.validate(proposed)?;
        Ok(self.0.plan)
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let factor = context.configuration()["factor"].as_i64().unwrap();
        let operation = context
            .update_directive()
            .map(|update| update.operation_id.to_string());
        if let Some(operation) = &operation {
            self.0.assert_accepted(&context.component_id, operation);
            assert_eq!(
                context.update_directive().unwrap().plan.mode,
                ComponentUpdateMode::Recreate
            );
        }
        self.0
            .record("create", context.generation, factor, operation);
        if context.update_directive().is_some() {
            self.0.pause_update().await;
        }
        Ok(ConstructedComponent::service(Box::new(Counter {
            descriptor: context.specification.descriptor.clone(),
            generation: context.generation,
            factor,
            factory: self.0.clone(),
        })))
    }
}

struct Counter {
    descriptor: ComponentDescriptor,
    generation: ComponentGeneration,
    factor: i64,
    factory: Arc<Factory>,
}

impl Counter {
    fn record(&self, hook: &'static str) {
        self.factory
            .record(hook, self.generation, self.factor, None);
    }
}

#[async_trait]
impl ComputationComponent for Counter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"factor": self.factor}))
    }
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.record("initialize");
        Ok(())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.record("start");
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.record("stop");
        Ok(())
    }
    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.record("dispose");
        Ok(())
    }
    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.record("pause");
        Ok(())
    }
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.record("resume");
        Ok(())
    }
    async fn apply_update(&mut self, context: ComponentUpdateContext) -> anyhow::Result<()> {
        self.factory
            .assert_accepted(self.descriptor.id(), &context.operation_id);
        self.factory.record(
            "apply",
            self.generation,
            self.factor,
            Some(context.operation_id.to_string()),
        );
        self.factory.pause_update().await;
        self.factor = context.proposed.configuration()["factor"].as_i64().unwrap();
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Counter {
    async fn run(&mut self) -> anyhow::Result<()> {
        let mut input = self.factory.input.lock().await;
        while let Some(value) = input.recv().await {
            let total = self.factory.data.add(value * self.factor)?;
            self.factory.output.send(total)?;
        }
        std::future::pending().await
    }
}

async fn instance(
    factory: Arc<Factory>,
    store: Option<Arc<FileStore>>,
) -> anyhow::Result<Arc<DrasiLib>> {
    let mut factories = FactoryRegistry::default();
    factories.register(Arc::new(RegisteredFactory(factory.clone())))?;
    let mut builder = DrasiLib::builder()
        .with_id("managed-update-fixture")
        .with_startup_timeout_secs(5)
        .with_shutdown_timeout_secs(5)
        .with_construction_timeout_secs(2)
        .with_activation_timeout_secs(2)
        .with_cleanup_timeout_secs(2)
        .with_component_factories(factories);
    if let Some(store) = store {
        builder = builder.with_configuration_store(Arc::new(Provider(store)));
    }
    let core = Arc::new(builder.build().await?);
    *factory.core.lock().unwrap() = Arc::downgrade(&core);
    Ok(core)
}

async fn add_counter(core: &DrasiLib, factory: &Factory) -> anyhow::Result<ComponentHandle> {
    let receipt = core
        .create_managed_component(
            0,
            "counter",
            ComponentDefinition {
                name: name("counter"),
                id: None,
                implementation: factory.descriptor.implementation.clone(),
                configuration: serde_json::json!({"factor": 1}),
                dependencies: BTreeMap::new(),
                streams: BTreeMap::new(),
                lifecycle: LifecyclePolicy { auto_start: true },
            },
        )
        .await?;
    let id = &receipt.creation.unwrap().components[0].id;
    core.reconcile_desired_state().await?;
    let handle = core.computation_component(id.as_str())?;
    handle.wait_created().await?;
    core.start().await?;
    handle.wait_started().await?;
    Ok(handle)
}

async fn change(core: &DrasiLib, request: &str, factor: i64) -> anyhow::Result<AcceptanceReceipt> {
    let saved = core.desired_configuration()?;
    let mut desired = saved.desired;
    let ComponentConstruction::Factory(specification) =
        &mut desired.topology.components[0].construction
    else {
        unreachable!()
    };
    specification.configuration.insert(
        Arc::from("factor"),
        ConfigurationValue::Literal(factor.into()),
    );
    Ok(core
        .apply_desired_state(saved.revision, request, desired)
        .await?)
}

async fn received(output: &mut mpsc::UnboundedReceiver<i64>) -> anyhow::Result<i64> {
    Ok(tokio::time::timeout(Duration::from_secs(5), output.recv())
        .await?
        .unwrap())
}

async fn completed(core: &DrasiLib) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            core.reconcile_desired_state().await?;
            if core.desired_configuration()?.pending_updates.is_empty() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

#[test]
fn factory_plans_preserve_state_and_queue_ownership_on_both_runtimes() -> anyhow::Result<()> {
    for multithreaded in [false, true] {
        let runtime = if multithreaded {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
        };
        for mode in [
            ComponentUpdateMode::InPlace,
            ComponentUpdateMode::Restart,
            ComponentUpdateMode::Recreate,
        ] {
            runtime.block_on(async {
                let file = OwnedFile::new()?;
                let data = DataFile::new(file.0.with_extension("counter"))?;
                let (factory, input, mut output) = Factory::new(
                    ComponentUpdatePlan {
                        mode,
                        data: ComponentUpdateData::Preserve,
                    },
                    true,
                    data.clone(),
                );
                let core = instance(factory.clone(), None).await?;
                assert!(!core.allow_external_component_ids());
                let original = add_counter(&core, &factory).await?;
                input.send(1)?;
                assert_eq!(received(&mut output).await?, 1);
                factory.gate.store(true, Ordering::Release);
                let receipt = change(&core, "update", 2).await?;
                assert_eq!(receipt.updates.len(), 1);
                assert_eq!(receipt.updates[0].component_id, *original.id());
                tokio::time::timeout(Duration::from_secs(5), factory.entered.notified()).await?;
                let waiter = {
                    let core = core.clone();
                    tokio::spawn(async move { core.reconcile_desired_state().await })
                };
                tokio::task::yield_now().await;
                waiter.abort();
                let _ = waiter.await;
                input.send(3)?;
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), output.recv())
                        .await
                        .is_err()
                );
                factory.gate.store(false, Ordering::Release);
                factory.release.notify_one();
                completed(&core).await?;
                let current = core.computation_component(original.id().as_str())?;
                current.wait_started().await?;
                assert_eq!(received(&mut output).await?, 7);
                assert_eq!(data.value()?, 7);
                assert_eq!(
                    core.desired_configuration()?.revision,
                    receipt.revision,
                    "execution acknowledgement must not change the desired revision"
                );
                let hooks = factory.hooks();
                match mode {
                    ComponentUpdateMode::InPlace => {
                        assert_eq!(original.generation(), current.generation());
                        assert_eq!(
                            hooks,
                            ["create", "initialize", "start", "pause", "apply", "resume"]
                        );
                    }
                    ComponentUpdateMode::Restart => {
                        assert_eq!(original.generation(), current.generation());
                        assert_eq!(
                            hooks,
                            ["create", "initialize", "start", "stop", "apply", "start"]
                        );
                    }
                    ComponentUpdateMode::Recreate => {
                        assert_ne!(original.generation(), current.generation());
                        assert!(original.start().await.is_err());
                        assert_eq!(
                            hooks,
                            [
                                "create",
                                "initialize",
                                "start",
                                "stop",
                                "dispose",
                                "create",
                                "initialize",
                                "start"
                            ]
                        );
                    }
                }
                core.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            })?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_and_rebootstrap_plans_leave_configuration_and_processing_untouched(
) -> anyhow::Result<()> {
    for (supported, data_policy) in
        [(false, ComponentUpdateData::Preserve), (true, ComponentUpdateData::Rebootstrap)]
    {
        let file = OwnedFile::new()?;
        let data = DataFile::new(file.0.with_extension("counter"))?;
        let (factory, input, mut output) = Factory::new(
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::Recreate,
                data: data_policy,
            },
            supported,
            data.clone(),
        );
        let core = instance(factory.clone(), None).await?;
        add_counter(&core, &factory).await?;
        input.send(4)?;
        assert_eq!(received(&mut output).await?, 4);
        let before = core.desired_configuration()?;
        let hooks = factory.hooks();
        let error = change(&core, "unsupported", 2).await.unwrap_err();
        if supported {
            assert!(format!("{error:#}").contains("Rebootstrap"));
        }
        assert_eq!(core.desired_configuration()?, before);
        assert_eq!(factory.hooks(), hooks);
        input.send(3)?;
        assert_eq!(received(&mut output).await?, 7);
        assert_eq!(data.value()?, 7);
        core.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn uncertain_update_completion_is_confirmed_before_processing_resumes() -> anyhow::Result<()>
{
    let file = OwnedFile::new()?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    let data = DataFile::new(file.0.with_extension("counter"))?;
    let (factory, input, mut output) = Factory::new(
        ComponentUpdatePlan {
            mode: ComponentUpdateMode::InPlace,
            data: ComponentUpdateData::Preserve,
        },
        true,
        data,
    );
    let core = instance(factory.clone(), Some(store.clone())).await?;
    add_counter(&core, &factory).await?;
    store.lose_complete_reply.store(true, Ordering::Release);
    let receipt = change(&core, "uncertain", 2).await?;
    completed(&core).await?;
    input.send(5)?;
    assert_eq!(received(&mut output).await?, 10);
    assert!(load(&file.0)?.current.pending_updates.is_empty());
    assert_eq!(load(&file.0)?.current.revision, receipt.revision);
    assert_eq!(
        factory
            .hooks()
            .iter()
            .filter(|hook| **hook == "apply")
            .count(),
        1
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn restart_recovery_retains_original_update_identity_and_compatible_state(
) -> anyhow::Result<()> {
    for mode in [
        ComponentUpdateMode::Restart,
        ComponentUpdateMode::Recreate,
        ComponentUpdateMode::InPlace,
    ] {
        let file = OwnedFile::new()?;
        let store = Arc::new(FileStore::new(file.0.clone()));
        let data = DataFile::new(file.0.with_extension("counter"))?;
        let plan = ComponentUpdatePlan {
            mode,
            data: ComponentUpdateData::Preserve,
        };
        let (factory, input, mut output) = Factory::new(plan, true, data.clone());
        let core = instance(factory.clone(), Some(store.clone())).await?;
        let original = add_counter(&core, &factory).await?;
        input.send(4)?;
        assert_eq!(received(&mut output).await?, 4);
        store.reject_complete.store(true, Ordering::Release);
        let receipt = change(&core, "recover", 2).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let events = factory.events.lock().unwrap().clone();
                if events.iter().any(|event| {
                    event.operation.as_deref() == Some(receipt.updates[0].operation_id.as_str())
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(load(&file.0)?.current.pending_updates, receipt.updates);
        core.shutdown().await?;
        drop(core);
        assert_eq!(
            load(&file.0)?.current.pending_updates,
            receipt.updates,
            "{mode:?}: shutdown must not forget an unconfirmed accepted operation"
        );
        assert_eq!(
            data.value()?,
            4,
            "disposal must not delete compatible processing state"
        );

        store.reject_complete.store(false, Ordering::Release);
        let (restored_factory, restored_input, mut restored_output) =
            Factory::new(plan, true, data.clone());
        let restored = instance(restored_factory.clone(), Some(store)).await?;
        restored.start().await?;
        completed(&restored).await?;
        assert_eq!(restored.component_by_name("counter")?.id(), original.id());
        assert!(original.start().await.is_err());
        restored_input.send(3)?;
        assert_eq!(received(&mut restored_output).await?, 10);
        let events = restored_factory.events.lock().unwrap().clone();
        let update = events
            .iter()
            .find(|event| event.operation.is_some())
            .unwrap_or_else(|| panic!("{mode:?}: restored operation was skipped: {events:?}"));
        assert_eq!(
            update.operation.as_deref(),
            Some(receipt.updates[0].operation_id.as_str())
        );
        let created = events.iter().find(|event| event.hook == "create").unwrap();
        assert_eq!(
            created.factor,
            if mode == ComponentUpdateMode::Recreate {
                2
            } else {
                1
            }
        );
        assert_eq!(created.generation, update.generation);
        assert!(load(&file.0)?.current.pending_updates.is_empty());
        restored.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn pending_reconstruction_binds_names_once_without_replaying_completed_construction(
) -> anyhow::Result<()> {
    let file = OwnedFile::new()?;
    let store = Arc::new(FileStore::new(file.0.clone()));
    let resolver = Arc::new(super::Resolver::new(file.0.clone()));
    let core = super::core(store.clone(), resolver.clone()).await?;
    let receipt = core
        .create_managed_ordinary_component(
            0,
            "reaction",
            OrdinaryComponentDefinition {
                id: None,
                name: name("consumer"),
                auto_start: false,
                component: OrdinaryComponent::Reaction {
                    kind: "application".into(),
                    configuration: serde_json::json!({}),
                    inputs: vec![],
                },
            },
        )
        .await?;
    let id = receipt.creation.unwrap().components[0].id.clone();
    super::settle(&core).await?;
    core.computation_component(id.as_str())?
        .wait_created()
        .await?;
    assert_eq!(resolver.reactions.load(Ordering::Acquire), 1);
    store.reject_complete.store(true, Ordering::Release);
    let saved = core.desired_configuration()?;
    let mut desired = saved.desired;
    let OrdinaryComponent::Reaction { inputs, .. } = &mut desired.ordinary[0].component else {
        unreachable!()
    };
    *inputs = vec![drasi_lib::config::ComponentReference::by_name("later")?];
    let update = core
        .apply_desired_state(saved.revision, "bind-later", desired)
        .await?;
    core.reconcile_desired_state().await?;
    assert_eq!(resolver.reactions.load(Ordering::Acquire), 1);
    assert!(!core
        .computation_component(id.as_str())?
        .observed()?
        .waiting_for
        .is_empty());

    let query = || {
        OrdinaryComponentDefinition::query(
            drasi_lib::Query::cypher("later")
                .query("MATCH (n) RETURN n")
                .enable_bootstrap(false)
                .auto_start(false)
                .build()?,
        )
    };
    let added = core
        .create_managed_ordinary_component(
            core.desired_configuration()?.revision,
            "dependency",
            query()?,
        )
        .await?;
    let query_id = added.creation.unwrap().components[0].id.clone();
    core.reconcile_desired_state().await?;
    tokio::time::timeout(
        Duration::from_secs(5),
        core.computation_component(id.as_str())?.wait_created(),
    )
    .await??;
    assert_eq!(resolver.reactions.load(Ordering::Acquire), 2);

    let saved = core.desired_configuration()?;
    let mut desired = saved.desired;
    desired
        .ordinary
        .iter_mut()
        .find(|record| record.id == query_id)
        .unwrap()
        .name = name("renamed");
    core.apply_desired_state(saved.revision, "rename-dependency", desired)
        .await?;
    core.create_managed_ordinary_component(
        core.desired_configuration()?.revision,
        "reused-name",
        query()?,
    )
    .await?;
    store.reject_complete.store(false, Ordering::Release);
    super::settle(&core).await?;
    let stored = core.desired_configuration()?;
    assert!(stored.pending_updates.is_empty());
    let OrdinaryComponent::Reaction { inputs, .. } = &stored
        .desired
        .ordinary
        .iter()
        .find(|record| record.id == id)
        .unwrap()
        .component
    else {
        unreachable!()
    };
    assert_eq!(inputs[0].as_id(), Some(&query_id));
    assert_ne!(core.component_by_name("later")?.id(), &query_id);
    assert_eq!(resolver.reactions.load(Ordering::Acquire), 2);
    assert_eq!(
        load(&file.0)?.receipts["bind-later"].1.updates,
        update.updates
    );
    core.shutdown().await?;
    Ok(())
}
