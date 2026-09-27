// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{computation::v1::*, management::*, DrasiLib};
use drasi_state_store_redb::RedbConfigurationStore;

struct Journals {
    indexes: Arc<dyn ComputationIndexProvider>,
    latest: Mutex<Option<Weak<QosChannel>>>,
    fail: AtomicBool,
    crash: std::sync::atomic::AtomicUsize,
}

impl Journals {
    fn channel(&self) -> Result<Arc<QosChannel>> {
        self.latest
            .lock()
            .expect("journal observations")
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("managed journal is unavailable"))
    }
}

#[async_trait]
impl ManagementResourceResolver for Journals {
    async fn resolve(
        &self,
        _: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        if self.fail.swap(false, Ordering::AcqRel) {
            anyhow::bail!("injected replacement construction failure");
        }
        if self.crash.load(Ordering::Acquire) == 1 {
            std::process::exit(81);
        }
        let definition: QosChannelDefinition = serde_json::from_value(configuration.clone())?;
        let indexes = self
            .indexes
            .create_indexes(graph, specification.id.as_str())
            .await?;
        let channel = QosChannel::persistent(
            definition,
            indexes,
            FactoryRegistry::standard()
                .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
            specification.id.as_str(),
        )
        .await?;
        if self.crash.load(Ordering::Acquire) == 2 {
            std::process::exit(82);
        }
        *self.latest.lock().expect("journal observations") = Some(Arc::downgrade(&channel));
        Ok(channel.resource())
    }
}

fn definition(capacity: usize) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new("producer/out").expect("stream"),
        capacity: NonZeroUsize::new(capacity).expect("capacity"),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
    }
}

fn desired(capacity: usize) -> Result<DesiredInstance> {
    let mut topology = ComputationGraph::empty("managed")?
        .snapshot()
        .select(GraphSelection::All)?;
    let id = ResourceId::try_new("journal")?;
    topology.resources.push(ResourceSpecification {
        id: id.clone(),
        role: ResourceRole::StateStore,
        ownership: ResourceOwnership::Graph,
        binding: "journal".into(),
    });
    topology
        .resource_configurations
        .insert(id, serde_json::to_value(definition(capacity))?);
    Ok(DesiredInstance {
        version: 1,
        graphs: vec![DesiredGraph {
            auto_start: false,
            topology,
        }],
    })
}

fn event(sequence: u64) -> Result<ChangeEnvelope> {
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("producer", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
        },
        StreamId::try_new("producer/out")?,
        sequence,
        None,
    )?)
}

async fn open(directory: &std::path::Path) -> Result<(DrasiLib, Arc<Journals>)> {
    let resolver = Arc::new(Journals {
        indexes: LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            directory.join("data"),
            false,
            false,
        ))),
        latest: Mutex::new(None),
        fail: AtomicBool::new(false),
        crash: std::sync::atomic::AtomicUsize::new(0),
    });
    let core = DrasiLib::builder()
        .with_id("resource-handover")
        .with_configuration_store(Arc::new(RedbConfigurationStore::new(
            directory.join("config.redb"),
            [19; 32],
        )?))
        .with_management_resources(resolver.clone())
        .build()
        .await?;
    Ok((core, resolver))
}

#[tokio::test]
async fn live_same_path_journal_replacement_preserves_unhandled_data_and_closes_the_old_owner(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, resolver) = open(directory.path()).await?;
    core.apply_desired_state(0, "initial", desired(2)?).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    let old = resolver.channel()?;
    old.publish(&event(1)?).await?;
    old.publish(&event(2)?).await?;
    let receipt = core.apply_desired_state(1, "resize", desired(4)?).await?;
    assert!(receipt.durable);
    let status = core.reconcile_desired_state().await?;
    assert!(status.converged(), "{status:?}");
    assert_eq!(status.revision, 2);
    assert!(
        old.progress().await.is_err(),
        "old observers must not keep the physical store open or usable"
    );
    let new = resolver.channel()?;
    assert!(!Arc::ptr_eq(&old, &new));
    assert_eq!(new.progress().await?.accepted, 2);
    assert_eq!(new.progress().await?.processed["consumer"], 0);
    new.publish(&event(3)?).await?;
    let id = ResourceId::try_new("journal")?;
    let mut pipe = definition(4)
        .pipe(id.clone(), "consumer")
        .create_with_resources(&BTreeMap::from([(id, new.resource())]))?;
    let mut receiver = pipe.pipe.take_receiver()?;
    for sequence in 1..=3 {
        let delivery = receiver.receive().await?.unwrap();
        assert_eq!(delivery.envelope().system().sequence(), sequence);
        delivery
            .into_parts()
            .1
            .unwrap()
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    assert_eq!(new.progress().await?.processed["consumer"], 3);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_resource_replacement_leaves_an_explicit_retryable_target_and_keeps_data(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, resolver) = open(directory.path()).await?;
    core.apply_desired_state(0, "initial", desired(2)?).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    resolver.channel()?.publish(&event(1)?).await?;
    resolver.fail.store(true, Ordering::Release);
    core.apply_desired_state(1, "resize", desired(4)?).await?;
    // A queued receipt request observes the automatically attempted reconciliation.
    assert_eq!(
        core.configuration_receipt("resize")
            .await?
            .unwrap()
            .revision,
        2
    );
    let failed = core.management_status().await?;
    assert!(!failed.converged());
    assert!(failed.graphs[0].error.is_some() || !failed.graphs[0].resource_errors.is_empty());
    let recovered = core.reconcile_desired_state().await?;
    assert!(recovered.converged(), "{recovered:?}");
    assert_eq!(resolver.channel()?.progress().await?.accepted, 1);
    assert_eq!(
        resolver.channel()?.progress().await?.processed["consumer"],
        0
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "resource handover process worker invoked by the parent test"]
async fn managed_resource_crash_worker() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("DRASI_MANAGED_RESOURCE_WORKER_ROOT")?);
    let crash: usize = std::env::var("DRASI_MANAGED_RESOURCE_WORKER_PHASE")?.parse()?;
    let (core, resolver) = open(&root).await?;
    core.apply_desired_state(0, "initial", desired(2)?).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    let channel = resolver.channel()?;
    channel.publish(&event(1)?).await?;
    channel.publish(&event(2)?).await?;
    resolver.crash.store(crash, Ordering::Release);
    core.apply_desired_state(1, "resize", desired(4)?).await?;
    core.configuration_receipt("resize").await?;
    anyhow::bail!("resource handover did not reach the requested crash boundary")
}

#[tokio::test]
async fn process_crashes_during_resource_handover_restore_target_and_pending_data() -> Result<()> {
    for (phase, exit) in [(1, 81), (2, 82)] {
        let directory = tempfile::tempdir()?;
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "managed_resource_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DRASI_MANAGED_RESOURCE_WORKER_ROOT", directory.path())
                .env("DRASI_MANAGED_RESOURCE_WORKER_PHASE", phase.to_string())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (core, resolver) = open(directory.path()).await?;
        assert_eq!(core.desired_configuration()?.revision, 2);
        assert_eq!(
            core.configuration_receipt("resize")
                .await?
                .unwrap()
                .revision,
            2
        );
        assert!(core.reconcile_desired_state().await?.converged());
        let channel = resolver.channel()?;
        assert_eq!(channel.progress().await?.accepted, 2);
        assert_eq!(channel.progress().await?.processed["consumer"], 0);
        let resource = ResourceId::try_new("journal")?;
        let mut connection = definition(4)
            .pipe(resource.clone(), "consumer")
            .create_with_resources(&BTreeMap::from([(resource, channel.resource())]))?;
        let mut receiver = connection.pipe.take_receiver()?;
        for sequence in 1..=2 {
            let (envelope, acknowledgement) = receiver.receive().await?.unwrap().into_parts();
            assert_eq!(envelope.id(), event(sequence)?.id());
            assert_eq!(envelope.system().sequence(), sequence);
            acknowledgement
                .unwrap()
                .complete(HandlingOutcome::Handled)
                .await?;
        }
        assert_eq!(channel.progress().await?.processed["consumer"], 2);
        core.shutdown().await?;
    }
    Ok(())
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct Listener {
    address: std::net::SocketAddr,
    socket: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
    fail_cleanup: AtomicBool,
    cleanup_attempts: std::sync::atomic::AtomicUsize,
    starts: std::sync::atomic::AtomicUsize,
    stops: std::sync::atomic::AtomicUsize,
}

impl Listener {
    async fn connect(&self) -> Result<()> {
        let socket = self.socket.lock().await;
        let socket = socket.as_ref().context("listener is closed")?;
        let client = tokio::net::TcpStream::connect(self.address).await?;
        let (server, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.accept()).await??;
        assert_eq!(client.peer_addr()?, server.local_addr()?);
        Ok(())
    }
}

#[async_trait]
impl ResourceCleanup for Listener {
    async fn shutdown(&self) -> Result<()> {
        self.cleanup_attempts.fetch_add(1, Ordering::AcqRel);
        if self.fail_cleanup.swap(false, Ordering::AcqRel) {
            anyhow::bail!("injected listener cleanup failure");
        }
        self.socket.lock().await.take();
        Ok(())
    }
}

#[derive(Default)]
struct Listeners {
    latest: Mutex<BTreeMap<ResourceId, Weak<Listener>>>,
    gate: Mutex<Option<Arc<Gate>>>,
    wrong_role: AtomicBool,
    created: std::sync::atomic::AtomicUsize,
    user_creations: std::sync::atomic::AtomicUsize,
}

impl Listeners {
    fn listener(&self, id: &str) -> Result<Arc<Listener>> {
        self.latest
            .lock()
            .expect("listener observations")
            .get(&ResourceId::try_new(id)?)
            .and_then(Weak::upgrade)
            .context("listener is unavailable")
    }
}

#[async_trait]
impl ManagementResourceResolver for Listeners {
    async fn resolve(
        &self,
        _: &str,
        _: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        let gate = self.gate.lock().expect("construction gate").take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let socket = tokio::net::TcpListener::bind(
            configuration["listen"]
                .as_str()
                .context("missing listener address")?,
        )
        .await?;
        let listener = Arc::new(Listener {
            address: socket.local_addr()?,
            socket: tokio::sync::Mutex::new(Some(socket)),
            fail_cleanup: AtomicBool::new(false),
            cleanup_attempts: std::sync::atomic::AtomicUsize::new(0),
            starts: std::sync::atomic::AtomicUsize::new(0),
            stops: std::sync::atomic::AtomicUsize::new(0),
        });
        self.created.fetch_add(1, Ordering::AcqRel);
        self.latest
            .lock()
            .expect("listener observations")
            .insert(specification.id.clone(), Arc::downgrade(&listener));
        let role = if self.wrong_role.swap(false, Ordering::AcqRel) {
            ResourceRole::Pipe
        } else {
            specification.role
        };
        Ok(ResourceHandle::new(role, listener.clone()).with_cleanup(listener))
    }
}

struct ListenerService {
    descriptor: ComponentDescriptor,
    listener: Arc<Listener>,
}

#[async_trait]
impl ComputationComponent for ListenerService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        self.listener.connect().await?;
        self.listener.starts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.listener.stops.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl ComputationService for ListenerService {
    async fn run(&mut self) -> Result<()> {
        std::future::pending().await
    }
}

struct ListenerFactory {
    descriptor: FactoryDescriptor,
    resources: Arc<Listeners>,
}

#[async_trait]
impl ComponentFactory for ListenerFactory {
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
        let listener = context
            .resources::<Listener>("listener")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .expect("required listener");
        self.resources.user_creations.fetch_add(1, Ordering::AcqRel);
        Ok(ConstructedComponent::service(Box::new(ListenerService {
            descriptor: context.specification.descriptor.clone(),
            listener,
        })))
    }
}

async fn listeners() -> Result<(DrasiLib, Arc<Listeners>, DesiredInstance)> {
    let resolver = Arc::new(Listeners::default());
    let implementation = ImplementationIdentity::try_new("test/listener-user", "1")?;
    let mut factories = FactoryRegistry::standard();
    factories.register(Arc::new(ListenerFactory {
        descriptor: FactoryDescriptor {
            implementation: implementation.clone(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::from([(
                Arc::from("listener"),
                ResourceRequirement::exactly_one::<Listener>(ResourceRole::StateStore),
            )]),
        },
        resources: resolver.clone(),
    }))?;
    let core = DrasiLib::builder()
        .with_management_resources(resolver.clone())
        .with_component_factories(factories)
        .build()
        .await?;
    let mut target = ComputationGraph::empty("listeners")?
        .snapshot()
        .select(GraphSelection::All)?;
    for name in ["changed", "unrelated"] {
        let id = ResourceId::try_new(name)?;
        target.resources.push(ResourceSpecification {
            id: id.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: name.into(),
        });
        target
            .resource_configurations
            .insert(id.clone(), serde_json::json!({"listen":"127.0.0.1:0"}));
        let descriptor =
            ComponentDescriptor::try_new(ComponentId::try_new(format!("{name}-user"))?, vec![])?;
        target.components.push(DesiredComponent {
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
                implementation: implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: BTreeMap::from([(Arc::from("listener"), vec![id])]),
            }),
        });
    }
    let desired: DesiredInstance = target.into();
    core.apply_desired_state(0, "initial", desired.clone())
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    Ok((core, resolver, desired))
}

#[tokio::test]
async fn listener_handover_retries_cleanup_and_survives_cancelled_wait_without_reopening_unrelated_resources(
) -> Result<()> {
    let (core, resolver, mut desired) = listeners().await?;
    let core = Arc::new(core);
    let old = resolver.listener("changed")?;
    let unrelated = resolver.listener("unrelated")?;
    old.connect().await?;
    unrelated.connect().await?;
    assert_eq!(unrelated.starts.load(Ordering::Acquire), 1);
    old.fail_cleanup.store(true, Ordering::Release);
    desired.graphs[0].topology.resource_configurations.insert(
        ResourceId::try_new("changed")?,
        serde_json::json!({"listen":old.address.to_string()}),
    );
    core.apply_desired_state(1, "replace", desired).await?;
    core.configuration_receipt("replace").await?;
    assert!(!core.management_status().await?.converged());
    assert_eq!(resolver.created.load(Ordering::Acquire), 2);
    assert_eq!(old.cleanup_attempts.load(Ordering::Acquire), 1);
    old.connect().await?;

    let gate = Arc::new(Gate::default());
    *resolver.gate.lock().unwrap() = Some(gate.clone());
    let waiter = tokio::spawn({
        let core = core.clone();
        async move { core.reconcile_desired_state().await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
    assert!(old.socket.lock().await.is_none());
    assert_eq!(old.cleanup_attempts.load(Ordering::Acquire), 2);
    unrelated.connect().await?;
    assert_eq!(unrelated.cleanup_attempts.load(Ordering::Acquire), 0);
    assert_eq!(unrelated.stops.load(Ordering::Acquire), 0);
    assert!(!core.management_status().await?.converged());
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    gate.release.notify_one();
    core.configuration_receipt("replace").await?;

    assert!(core.management_status().await?.converged());
    let replacement = resolver.listener("changed")?;
    assert_eq!(replacement.address, old.address);
    replacement.connect().await?;
    assert!(Arc::ptr_eq(&unrelated, &resolver.listener("unrelated")?));
    assert_eq!(resolver.created.load(Ordering::Acquire), 3);
    assert_eq!(resolver.user_creations.load(Ordering::Acquire), 3);
    assert_eq!(unrelated.starts.load(Ordering::Acquire), 1);
    assert_eq!(replacement.starts.load(Ordering::Acquire), 1);
    core.shutdown().await?;
    assert!(replacement.socket.lock().await.is_none());
    assert!(unrelated.socket.lock().await.is_none());
    assert_eq!(old.cleanup_attempts.load(Ordering::Acquire), 2);
    assert_eq!(replacement.cleanup_attempts.load(Ordering::Acquire), 1);
    assert_eq!(unrelated.cleanup_attempts.load(Ordering::Acquire), 1);
    Ok(())
}

#[tokio::test]
async fn wrong_role_resource_is_not_injected_and_retains_cleanup_ownership_until_retry(
) -> Result<()> {
    let (core, resolver, mut desired) = listeners().await?;
    let old = resolver.listener("changed")?;
    let id = ResourceId::try_new("changed")?;
    desired.graphs[0].topology.resource_configurations.insert(
        id.clone(),
        serde_json::json!({"listen":old.address.to_string()}),
    );
    resolver.wrong_role.store(true, Ordering::Release);
    core.apply_desired_state(1, "replace", desired).await?;
    core.configuration_receipt("replace").await?;
    let status = core.management_status().await?;
    assert!(!status.converged());
    assert!(status.graphs[0].resource_errors[&id].contains("role differs"));
    let graph = core.get_computation_graph("listeners").await?;
    assert_eq!(
        graph.observed().resources[&id].realization,
        ResourceRealization::CleanupRequired
    );
    assert_eq!(
        graph.observed().components[&ComponentId::try_new("changed-user")?].realization,
        RealizationState::Blocked
    );
    assert_eq!(resolver.user_creations.load(Ordering::Acquire), 2);
    let invalid = resolver.listener("changed")?;
    invalid.fail_cleanup.store(true, Ordering::Release);
    assert!(!core.reconcile_desired_state().await?.converged());
    assert_eq!(resolver.created.load(Ordering::Acquire), 3);
    assert!(invalid.socket.lock().await.is_some());
    assert!(core.reconcile_desired_state().await?.converged());
    assert!(invalid.socket.lock().await.is_none());
    assert_eq!(invalid.cleanup_attempts.load(Ordering::Acquire), 2);
    let replacement = resolver.listener("changed")?;
    assert_eq!(replacement.address, old.address);
    replacement.connect().await?;
    assert_eq!(resolver.created.load(Ordering::Acquire), 4);
    core.shutdown().await?;
    assert!(replacement.socket.lock().await.is_none());
    Ok(())
}
