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
    fn new(directory: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            indexes: LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
                directory.join("data"),
                false,
                false,
            ))),
            latest: Mutex::new(None),
            fail: AtomicBool::new(false),
            crash: std::sync::atomic::AtomicUsize::new(0),
        })
    }

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
    Ok(topology.into())
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
    let resolver = Journals::new(directory);
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
    let configuration = core.snapshot_computation_configuration().await?;
    let native = configuration
        .native_components
        .as_ref()
        .context("resource-only configuration")?;
    assert!(native.topology.components.is_empty());
    assert_eq!(native.topology.resources.len(), 1);
    assert!(native
        .topology
        .resource_configurations
        .contains_key(&ResourceId::try_new("journal")?));
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
    assert!(failed.error.is_some() || !failed.resource_errors.is_empty());
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
    hold_cleanup: AtomicBool,
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
        if self.hold_cleanup.load(Ordering::Acquire)
            || self.fail_cleanup.swap(false, Ordering::AcqRel)
        {
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
    after_bind: Mutex<Option<Arc<Gate>>>,
    crash: std::sync::atomic::AtomicUsize,
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
        if self.crash.load(Ordering::Acquire) == 1 {
            std::process::exit(83);
        }
        let socket = tokio::net::TcpListener::bind(
            configuration["listen"]
                .as_str()
                .context("missing listener address")?,
        )
        .await?;
        if self.crash.load(Ordering::Acquire) == 2 {
            std::process::exit(84);
        }
        let gate = self.after_bind.lock().expect("post-bind gate").take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let listener = Arc::new(Listener {
            address: socket.local_addr()?,
            socket: tokio::sync::Mutex::new(Some(socket)),
            fail_cleanup: AtomicBool::new(false),
            hold_cleanup: AtomicBool::new(false),
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

async fn open_listeners(
    store: Option<Arc<RedbConfigurationStore>>,
) -> Result<(DrasiLib, Arc<Listeners>)> {
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
    let mut builder = DrasiLib::builder()
        .with_id("listener-handover")
        .with_management_resources(resolver.clone())
        .with_component_factories(factories);
    if let Some(store) = store {
        builder = builder.with_configuration_store(store);
    }
    Ok((builder.build().await?, resolver))
}

fn listener_desired() -> Result<DesiredInstance> {
    let implementation = ImplementationIdentity::try_new("test/listener-user", "1")?;
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
    Ok(target.into())
}

async fn listeners() -> Result<(DrasiLib, Arc<Listeners>, DesiredInstance)> {
    let (core, resolver) = open_listeners(None).await?;
    let desired = listener_desired()?;
    core.apply_desired_state(0, "initial", desired.clone())
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    Ok((core, resolver, desired))
}

#[tokio::test]
async fn listener_resolution_timeout_releases_partial_acquisition_and_preserves_retry() -> Result<()>
{
    let (core, resolver, mut desired) = listeners().await?;
    let old = resolver.listener("changed")?;
    let unrelated = resolver.listener("unrelated")?;
    let id = ResourceId::try_new("changed")?;
    desired.topology.resource_configurations.insert(
        id.clone(),
        serde_json::json!({"listen":old.address.to_string()}),
    );
    let gate = Arc::new(Gate::default());
    *resolver.after_bind.lock().unwrap() = Some(gate.clone());
    core.apply_desired_state(1, "timeout", desired.clone())
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
    assert!(old.socket.lock().await.is_none());
    assert!(std::net::TcpListener::bind(old.address).is_err());
    unrelated.connect().await?;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::time::resume();
    core.configuration_receipt("timeout").await?;
    let status = core.management_status().await?;
    assert!(!status.converged(), "{status:?}");
    assert!(status.resource_errors.contains_key(&id), "{status:?}");
    assert_eq!(core.desired_configuration()?.desired, desired.normalized()?);
    assert_eq!(resolver.created.load(Ordering::Acquire), 2);
    assert_eq!(resolver.user_creations.load(Ordering::Acquire), 2);
    // The timed-out constructor owned a real bound socket, not just a pending future.
    drop(std::net::TcpListener::bind(old.address)?);
    assert!(core.reconcile_desired_state().await?.converged());
    resolver.listener("changed")?.connect().await?;
    assert!(Arc::ptr_eq(&unrelated, &resolver.listener("unrelated")?));
    assert_eq!(unrelated.stops.load(Ordering::Acquire), 0);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn external_listener_contention_does_not_lose_accepted_target_or_restart_unrelated_users(
) -> Result<()> {
    let (core, resolver, mut desired) = listeners().await?;
    let old = resolver.listener("changed")?;
    let unrelated = resolver.listener("unrelated")?;
    let id = ResourceId::try_new("changed")?;
    desired.topology.resource_configurations.insert(
        id.clone(),
        serde_json::json!({"listen":old.address.to_string()}),
    );
    let gate = Arc::new(Gate::default());
    *resolver.gate.lock().unwrap() = Some(gate.clone());
    core.apply_desired_state(1, "contended", desired).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
    let competitor = std::net::TcpListener::bind(old.address)?;
    gate.release.notify_one();
    core.configuration_receipt("contended").await?;
    let failed = core.management_status().await?;
    assert!(!failed.converged());
    assert!(failed.resource_errors.contains_key(&id), "{failed:?}");
    assert_eq!(core.desired_configuration()?.revision, 2);
    assert_eq!(resolver.created.load(Ordering::Acquire), 2);
    assert_eq!(resolver.user_creations.load(Ordering::Acquire), 2);
    unrelated.connect().await?;
    drop(competitor);
    assert!(core.reconcile_desired_state().await?.converged());
    resolver.listener("changed")?.connect().await?;
    assert!(Arc::ptr_eq(&unrelated, &resolver.listener("unrelated")?));
    assert_eq!(unrelated.stops.load(Ordering::Acquire), 0);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "listener replacement process worker invoked by the parent test"]
async fn listener_replacement_crash_worker() -> Result<()> {
    let path = std::path::PathBuf::from(std::env::var("DRASI_LISTENER_WORKER_PATH")?);
    let phase: usize = std::env::var("DRASI_LISTENER_WORKER_PHASE")?.parse()?;
    let (core, resolver) =
        open_listeners(Some(Arc::new(RedbConfigurationStore::new(path, [29; 32])?))).await?;
    let mut desired = listener_desired()?;
    core.apply_desired_state(0, "initial", desired.clone())
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    desired.topology.resource_configurations.insert(
        ResourceId::try_new("changed")?,
        serde_json::json!({"listen":resolver.listener("changed")?.address.to_string()}),
    );
    resolver.crash.store(phase, Ordering::Release);
    core.apply_desired_state(1, "replace", desired).await?;
    core.configuration_receipt("replace").await?;
    anyhow::bail!("listener replacement did not reach the requested process-exit boundary")
}

#[tokio::test]
async fn process_death_before_and_after_listener_bind_restores_committed_replacement() -> Result<()>
{
    for (phase, exit) in [(1, 83), (2, 84)] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("listeners.redb");
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "listener_replacement_crash_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DRASI_LISTENER_WORKER_PATH", &path)
                .env("DRASI_LISTENER_WORKER_PHASE", phase.to_string())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        assert_eq!(output.status.code(), Some(exit), "{output:?}");
        let (core, resolver) = open_listeners(Some(Arc::new(RedbConfigurationStore::new(
            &path, [29; 32],
        )?)))
        .await?;
        let committed = core.desired_configuration()?;
        assert_eq!(committed.revision, 2);
        assert_eq!(
            core.configuration_receipt("replace")
                .await?
                .unwrap()
                .revision,
            2
        );
        assert!(core.reconcile_desired_state().await?.converged());
        core.start().await?;
        let changed = resolver.listener("changed")?;
        assert_eq!(
            committed.desired.topology.resource_configurations[&ResourceId::try_new("changed")?]
                ["listen"],
            changed.address.to_string()
        );
        changed.connect().await?;
        resolver.listener("unrelated")?.connect().await?;
        core.shutdown().await?;
        drop(std::net::TcpListener::bind(changed.address)?);
    }
    Ok(())
}

#[tokio::test]
async fn failed_shutdown_retains_configuration_lease_and_stale_owner_cannot_close_replacement(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("shutdown.redb"),
        [46; 32],
    )?);
    let (old, resolver) = open_listeners(Some(store.clone())).await?;
    old.apply_desired_state(0, "initial", listener_desired()?)
        .await?;
    assert!(old.reconcile_desired_state().await?.converged());
    old.start().await?;
    let listener = resolver.listener("changed")?;
    listener.hold_cleanup.store(true, Ordering::Release);
    assert!(old.shutdown().await.is_err());
    assert!(matches!(
        store
            .open("listener-handover")
            .await
            .err()
            .unwrap()
            .downcast_ref(),
        Some(ManagementError::AlreadyOwned(_))
    ));
    let unrelated = store.open("unrelated").await?;
    unrelated.close().await?;
    assert!(listener.socket.lock().await.is_some());
    listener.hold_cleanup.store(false, Ordering::Release);
    old.shutdown().await?;
    assert!(listener.socket.lock().await.is_none());
    let (replacement, resources) = open_listeners(Some(store.clone())).await?;
    replacement.start().await?;
    old.shutdown().await?;
    drop((old, resolver));
    assert!(store.open("listener-handover").await.is_err());
    resources.listener("changed")?.connect().await?;
    replacement.shutdown().await?;
    Ok(())
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
    desired.topology.resource_configurations.insert(
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
    desired.topology.resource_configurations.insert(
        id.clone(),
        serde_json::json!({"listen":old.address.to_string()}),
    );
    resolver.wrong_role.store(true, Ordering::Release);
    core.apply_desired_state(1, "replace", desired).await?;
    core.configuration_receipt("replace").await?;
    let status = core.management_status().await?;
    assert!(!status.converged());
    assert!(status.resource_errors[&id].contains("role differs"));
    let graph = core.computation_control()?;
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

struct TrafficProbe {
    send: tokio::sync::mpsc::Sender<ChangeEnvelope>,
    receive: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ChangeEnvelope>>,
    pulled: tokio::sync::watch::Sender<u64>,
    handled: tokio::sync::watch::Sender<Vec<(u64, ChangeEnvelope)>>,
    gate: Mutex<Option<Arc<Gate>>>,
    fail_handling: AtomicBool,
    handling_failures: std::sync::atomic::AtomicUsize,
    created: std::sync::atomic::AtomicUsize,
    starts: std::sync::atomic::AtomicUsize,
    stops: std::sync::atomic::AtomicUsize,
    drops: std::sync::atomic::AtomicUsize,
}

impl TrafficProbe {
    fn new() -> Arc<Self> {
        let (send, receive) = tokio::sync::mpsc::channel(16);
        Arc::new(Self {
            send,
            receive: tokio::sync::Mutex::new(receive),
            pulled: tokio::sync::watch::channel(0).0,
            handled: tokio::sync::watch::channel(Vec::new()).0,
            gate: Mutex::new(None),
            fail_handling: AtomicBool::new(false),
            handling_failures: 0.into(),
            created: 0.into(),
            starts: 0.into(),
            stops: 0.into(),
            drops: 0.into(),
        })
    }

    async fn wait_handled(&self, count: usize) -> Result<()> {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.handled
                .subscribe()
                .wait_for(|events| events.len() >= count),
        )
        .await??;
        Ok(())
    }
}

struct TrafficNode {
    descriptor: ComponentDescriptor,
    version: u64,
    probe: Arc<TrafficProbe>,
}

impl Drop for TrafficNode {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[async_trait]
impl ComputationComponent for TrafficNode {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"version": self.version}))
    }
    async fn start(&mut self) -> Result<()> {
        self.probe.starts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.probe.stops.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for TrafficNode {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let next = self.probe.receive.lock().await.recv().await;
        next.map(|envelope| {
            self.probe.pulled.send_replace(envelope.system().sequence());
            Ok(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope,
            })
        })
        .transpose()
    }
}

#[async_trait]
impl EnvelopeSink for TrafficNode {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let gate = self.probe.gate.lock().expect("handler gate").take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        if self.probe.fail_handling.swap(false, Ordering::AcqRel) {
            self.probe.handling_failures.fetch_add(1, Ordering::AcqRel);
            anyhow::bail!("injected failure before handling the held event");
        }
        self.probe
            .handled
            .send_modify(|seen| seen.push((self.version, input.envelope)));
        Ok(())
    }
}

struct TrafficFactory {
    descriptor: FactoryDescriptor,
    probes: BTreeMap<String, Arc<TrafficProbe>>,
}

#[async_trait]
impl ComponentFactory for TrafficFactory {
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
        let probe = self
            .probes
            .get(context.component_id.as_str())
            .context("undeclared traffic fixture")
            .map_err(ComponentCreationError::terminal)?
            .clone();
        let version = context.configuration()["version"]
            .as_u64()
            .context("invalid traffic version")
            .map_err(ComponentCreationError::terminal)?;
        probe.created.fetch_add(1, Ordering::AcqRel);
        let node = Box::new(TrafficNode {
            descriptor: context.specification.descriptor.clone(),
            version,
            probe,
        });
        Ok(if self.descriptor.role == ComponentRole::Source {
            ConstructedComponent::source(node)
        } else {
            ConstructedComponent::sink(node)
        })
    }
}

fn traffic_factory(
    role: ComponentRole,
    probes: BTreeMap<String, Arc<TrafficProbe>>,
) -> Result<TrafficFactory> {
    Ok(TrafficFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new(
                if role == ComponentRole::Source {
                    "test/traffic-source"
                } else {
                    "test/traffic-sink"
                },
                "1",
            )?,
            role,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::from([(
                    Arc::from("version"),
                    ConfigurationField {
                        value_type: ConfigurationType::Integer,
                        required: true,
                        secret: false,
                    },
                )]),
                allow_additional: false,
            },
            dependencies: BTreeMap::new(),
        },
        probes,
    })
}

fn traffic_desired(capacity: usize, version: u64) -> Result<DesiredInstance> {
    let mut target = desired(capacity)?;
    let graph = &mut target;
    let journal = ResourceId::try_new("journal")?;
    let mut channel = definition(capacity);
    channel
        .subscribers
        .insert("mirror".into(), SubscriptionStart::Earliest);
    graph
        .topology
        .resource_configurations
        .insert(journal.clone(), serde_json::to_value(&channel)?);
    for (id, source) in [
        ("producer", true),
        ("consumer", false),
        ("mirror", false),
        ("heartbeat", true),
        ("observer", false),
    ] {
        let component = ComponentId::try_new(id)?;
        let descriptor = ComponentDescriptor::try_new(
            component.clone(),
            vec![PortDescriptor::new(
                PortId::try_new(if source { "out" } else { "in" })?,
                if source {
                    PortDirection::Output
                } else {
                    PortDirection::Input
                },
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        let role = if source {
            ComponentRole::Source
        } else {
            ComponentRole::Sink
        };
        graph.topology.components.push(DesiredComponent {
            descriptor: descriptor.clone(),
            role,
            completion: (!source).then_some(SinkCompletion::Handled),
            streams: if source {
                BTreeMap::from([(
                    PortId::try_new("out")?,
                    StreamId::try_new(format!("{id}/out"))?,
                )])
            } else {
                BTreeMap::new()
            },
            lifecycle: LifecyclePolicy::default(),
            input_merge: InputMergePolicy::Arrival,
            construction: ComponentConstruction::Factory(ComponentSpecification {
                descriptor,
                role,
                completion: (!source).then_some(SinkCompletion::Handled),
                implementation: ImplementationIdentity::try_new(
                    if source {
                        "test/traffic-source"
                    } else {
                        "test/traffic-sink"
                    },
                    "1",
                )?,
                configuration_version: 1,
                configuration: BTreeMap::from([(
                    Arc::from("version"),
                    ConfigurationValue::Literal(serde_json::json!(if id == "consumer" {
                        version
                    } else {
                        0
                    })),
                )]),
                dependencies: BTreeMap::new(),
            }),
        });
    }
    for (source, sink) in [
        ("producer", "consumer"),
        ("producer", "mirror"),
        ("heartbeat", "observer"),
    ] {
        graph.topology.relationships.push(DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(ComponentId::try_new(source)?, PortId::try_new("out")?),
                Endpoint::new(ComponentId::try_new(sink)?, PortId::try_new("in")?),
            ),
            policy: RelationshipPolicy::default(),
            pipe: if source == "producer" {
                DesiredPipe::Qos(channel.pipe(journal.clone(), sink))
            } else {
                DesiredPipe::Bounded { capacity: 1 }
            },
        });
    }
    Ok(target)
}

async fn wait_journal(channel: &QosChannel, accepted: u64, processed: u64) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let progress = channel.progress().await?;
            if progress.accepted == accepted
                && progress
                    .processed
                    .values()
                    .all(|cursor| *cursor == processed)
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

struct Traffic {
    core: DrasiLib,
    resolver: Arc<Journals>,
    probes: BTreeMap<String, Arc<TrafficProbe>>,
}

async fn open_traffic(directory: &std::path::Path) -> Result<Traffic> {
    let resolver = Journals::new(directory);
    let probes: BTreeMap<_, _> = ["producer", "consumer", "mirror", "heartbeat", "observer"]
        .into_iter()
        .map(|id| (id.to_owned(), TrafficProbe::new()))
        .collect();
    let mut factories = FactoryRegistry::standard();
    factories.register(Arc::new(traffic_factory(
        ComponentRole::Source,
        probes.clone(),
    )?))?;
    factories.register(Arc::new(traffic_factory(
        ComponentRole::Sink,
        probes.clone(),
    )?))?;
    let core = DrasiLib::builder()
        .with_id("active-handover")
        .with_configuration_store(Arc::new(RedbConfigurationStore::new(
            directory.join("config.redb"),
            [31; 32],
        )?))
        .with_management_resources(resolver.clone())
        .with_component_factories(factories)
        .build()
        .await?;
    Ok(Traffic {
        core,
        resolver,
        probes,
    })
}

async fn live_traffic_transition(
    replace_journal: bool,
    fail_construction: bool,
    fail_handling: bool,
) -> Result<()> {
    const CYCLES: u64 = 64;
    let directory = tempfile::tempdir()?;
    let Traffic {
        core,
        resolver,
        probes,
    } = open_traffic(directory.path()).await?;
    core.apply_desired_state(0, "initial", traffic_desired(2, 0)?)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    let graph = core.computation_control()?;
    let before = graph.observed();
    let mut expected_versions = Vec::new();
    let mut capacity = 2;
    for iteration in 0..CYCLES {
        let first = iteration * 4 + 1;
        let old = resolver.channel()?;
        let gate = Arc::new(Gate::default());
        *probes["consumer"].gate.lock().expect("handler gate") = Some(gate.clone());
        probes["consumer"]
            .fail_handling
            .store(fail_handling, Ordering::Release);
        for sequence in first..=first + capacity as u64 {
            probes["producer"].send.send(event(sequence)?).await?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            probes["producer"]
                .pulled
                .subscribe()
                .wait_for(|sequence| *sequence == first + capacity as u64),
        )
        .await??;
        let full = old.progress().await?;
        assert_eq!(full.accepted, first + capacity as u64 - 1);
        assert_eq!(full.processed["consumer"], first - 1);
        let new_capacity = if replace_journal {
            if capacity == 2 {
                3
            } else {
                2
            }
        } else {
            capacity
        };
        resolver.fail.store(fail_construction, Ordering::Release);
        let receipt = core
            .apply_desired_state(
                iteration + 1,
                format!("replace-{iteration}"),
                traffic_desired(new_capacity, iteration + 1)?,
            )
            .await?;
        assert!(receipt.durable);
        let paused = ComponentId::try_new(if replace_journal {
            "producer"
        } else {
            "consumer"
        })?;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            graph.subscribe_observed().wait_for(|state| {
                state.components[&paused].lifecycle == ComponentLifecycle::Quiescing
            }),
        )
        .await??;
        assert!(!core.management_status().await?.converged());
        assert_eq!(
            probes["consumer"].stops.load(Ordering::Acquire),
            iteration as usize
        );
        if !fail_construction {
            let waiting = core.reconcile_desired_state();
            tokio::pin!(waiting);
            assert!(
                futures::poll!(waiting).is_pending(),
                "the handler still owns unfinished input"
            );
        }
        let heartbeat = GraphChangeCodec::encode_change(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("heartbeat", "tick"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: iteration,
                },
            },
            StreamId::try_new("heartbeat/out")?,
            iteration + 1,
            None,
        )?;
        probes["heartbeat"].send.send(heartbeat.clone()).await?;
        probes["observer"]
            .wait_handled(iteration as usize + 1)
            .await?;
        assert_eq!(
            GraphChangeCodec::decode_changes(
                &probes["observer"].handled.borrow()[iteration as usize].1
            )?,
            GraphChangeCodec::decode_changes(&heartbeat)?
        );
        gate.release.notify_one();
        core.configuration_receipt(format!("replace-{iteration}"))
            .await?;
        if fail_construction {
            let failed = core.management_status().await?;
            assert!(!failed.converged(), "failed replacement cannot look ready");
            assert!(
                failed.resource_errors[&ResourceId::try_new("journal")?]
                    .contains("injected replacement construction failure"),
                "{failed:?}"
            );
            assert!(
                !resolver.fail.load(Ordering::Acquire),
                "the intended constructor fault must execute"
            );
            assert!(
                old.progress().await.is_err(),
                "retired storage must not remain usable"
            );
        }
        let status = core.reconcile_desired_state().await?;
        assert!(status.converged(), "{status:?}");
        let current = resolver.channel()?;
        if replace_journal {
            assert!(!Arc::ptr_eq(&old, &current));
            assert!(matches!(
                old.publish(&event(first + 3)?).await,
                Err(PipeError::Closed)
            ));
        } else {
            assert!(Arc::ptr_eq(&old, &current));
        }
        for sequence in first + capacity as u64 + 1..=first + 3 {
            probes["producer"].send.send(event(sequence)?).await?;
        }
        wait_journal(&current, first + 3, first + 3).await?;
        for offset in 0..4 {
            expected_versions.push(
                if !fail_handling && (replace_journal && offset <= capacity || offset == 0) {
                    iteration
                } else {
                    iteration + 1
                },
            );
        }
        capacity = new_capacity;
        let observed = graph.observed();
        for id in ["producer", "mirror", "heartbeat", "observer"] {
            let component = ComponentId::try_new(id)?;
            assert_eq!(
                observed.components[&component].generation,
                before.components[&component].generation
            );
            assert_eq!(probes[id].created.load(Ordering::Acquire), 1, "{id}");
            assert_eq!(probes[id].starts.load(Ordering::Acquire), 1, "{id}");
            assert_eq!(probes[id].stops.load(Ordering::Acquire), 0, "{id}");
        }
        assert_eq!(
            probes["consumer"].created.load(Ordering::Acquire),
            iteration as usize + 2
        );
        assert_eq!(
            probes["consumer"].drops.load(Ordering::Acquire),
            iteration as usize + 1
        );
        assert_eq!(
            probes["consumer"].handling_failures.load(Ordering::Acquire),
            if fail_handling {
                iteration as usize + 1
            } else {
                0
            }
        );
    }
    for id in ["consumer", "mirror"] {
        let seen = probes[id].handled.borrow();
        assert_eq!(seen.len(), (CYCLES * 4) as usize, "{id}");
        for (index, (version, envelope)) in seen.iter().enumerate() {
            let expected = event(index as u64 + 1)?;
            assert_eq!(envelope.id(), expected.id(), "{id} at {index}");
            assert_eq!(
                GraphChangeCodec::decode_changes(envelope)?,
                GraphChangeCodec::decode_changes(&expected)?
            );
            if id == "consumer" {
                assert_eq!(
                    *version, expected_versions[index],
                    "wrong handler at {index}"
                );
            }
        }
    }
    core.shutdown().await?;
    for probe in probes.values() {
        assert_eq!(
            probe.created.load(Ordering::Acquire),
            probe.drops.load(Ordering::Acquire)
        );
        assert_eq!(
            probe.starts.load(Ordering::Acquire),
            probe.stops.load(Ordering::Acquire)
        );
    }
    let target = traffic_desired(capacity, CYCLES)?;
    let specification = &target.topology.resources[0];
    let reopened = resolver
        .resolve(
            "active-handover",
            &target.topology.graph_id,
            specification,
            &target.topology.resource_configurations[&specification.id],
        )
        .await?;
    let channel = reopened.get::<QosChannel>()?;
    wait_journal(&channel, CYCLES * 4, CYCLES * 4).await?;
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn live_full_queues_preserve_exact_work_across_sink_and_journal_replacement() -> Result<()> {
    for (journal, construction, handling) in [
        (false, false, false),
        (false, false, true),
        (true, false, false),
        (true, true, false),
    ] {
        tokio::time::timeout(
            std::time::Duration::from_secs(180),
            live_traffic_transition(journal, construction, handling),
        )
        .await??;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_full_queues_preserve_exact_work_across_sink_and_journal_replacement_multithread(
) -> Result<()> {
    for (journal, construction, handling) in [
        (false, false, false),
        (false, false, true),
        (true, false, false),
        (true, true, false),
    ] {
        tokio::time::timeout(
            std::time::Duration::from_secs(180),
            live_traffic_transition(journal, construction, handling),
        )
        .await??;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "active full-queue handover crash worker invoked by the parent test"]
async fn active_handover_crash_worker() -> Result<()> {
    let directory = std::path::PathBuf::from(std::env::var("DRASI_ACTIVE_HANDOVER_ROOT")?);
    let Traffic {
        core,
        resolver,
        probes,
    } = open_traffic(&directory).await?;
    core.apply_desired_state(0, "initial", traffic_desired(2, 0)?)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    let gate = Arc::new(Gate::default());
    *probes["consumer"].gate.lock().expect("handler gate") = Some(gate.clone());
    for sequence in 1..=3 {
        probes["producer"].send.send(event(sequence)?).await?;
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
    probes["mirror"].wait_handled(2).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let progress = resolver.channel()?.progress().await?;
            assert_eq!(progress.processed["consumer"], 0);
            if progress.accepted == 2 && progress.processed["mirror"] == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let receipt = core
        .apply_desired_state(1, "replace-active", traffic_desired(3, 1)?)
        .await?;
    assert!(receipt.durable);
    assert_eq!(receipt.revision, 2);
    let graph = core.computation_control()?;
    let producer = ComponentId::try_new("producer")?;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        graph.subscribe_observed().wait_for(|state| {
            state.components[&producer].lifecycle == ComponentLifecycle::Quiescing
        }),
    )
    .await??;
    std::process::exit(83);
}

#[tokio::test]
async fn crash_during_full_queue_handover_preserves_each_subscribers_exact_obligation() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "active_handover_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("DRASI_ACTIVE_HANDOVER_ROOT", directory.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert_eq!(
        output.status.code(),
        Some(83),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let Traffic {
        core,
        resolver,
        probes,
    } = open_traffic(directory.path()).await?;
    assert_eq!(core.desired_configuration()?.revision, 2);
    assert_eq!(
        core.configuration_receipt("replace-active")
            .await?
            .context("committed receipt")?
            .revision,
        2
    );
    let channel = resolver.channel()?;
    let restored = channel.progress().await?;
    assert_eq!(restored.accepted, 2);
    assert_eq!(
        restored.processed,
        BTreeMap::from([("consumer".into(), 0), ("mirror".into(), 2)])
    );
    core.start().await?;
    wait_journal(&channel, 2, 2).await?;
    // Input 3 was offered, but never durably accepted before the crash.
    probes["producer"].send.send(event(3)?).await?;
    wait_journal(&channel, 3, 3).await?;
    core.shutdown().await?;
    let consumer = probes["consumer"].handled.borrow();
    assert_eq!(consumer.len(), 3);
    for (index, (version, envelope)) in consumer.iter().enumerate() {
        assert_eq!(*version, 1);
        assert_eq!(envelope.id(), event(index as u64 + 1)?.id());
        assert_eq!(
            GraphChangeCodec::decode_changes(envelope)?,
            GraphChangeCodec::decode_changes(&event(index as u64 + 1)?)?
        );
    }
    let mirror = probes["mirror"].handled.borrow();
    assert_eq!(
        mirror.len(),
        1,
        "the already-handled prefix must not be replayed"
    );
    assert_eq!(mirror[0].1.id(), event(3)?.id());
    Ok(())
}
