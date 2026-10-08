// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Real SDK C entrypoints and host proxies. In-process probes check every wrapper;
//! the separately built, unpublished snapshot-test library checks cross-library
//! panic containment without adding failure injection to production plugins.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_host_sdk::{ReactionProxy, SourceProxy};
use drasi_lib::{
    channels::ComponentUpdate, computation::v1::*, ComponentStatus, Reaction,
    ReactionRuntimeContext, Source, SourceRuntimeContext, StateStoreProvider, SubscriptionResponse,
};
use drasi_lib::{
    queries::output_state::{FetchError, OutboxStream, SnapshotStream},
    reactions::{BootstrapBackend, BootstrapContext, ReactionCheckpoint},
};
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use drasi_plugin_sdk::ffi::{
    build_reaction_vtable, build_reaction_vtable_from_boxed, build_source_vtable,
    build_source_vtable_from_boxed, FfiLifecycleEventType, ReactionVtable,
};

#[derive(Default)]
struct Counts {
    initialized: AtomicUsize,
    initialization_exited: AtomicBool,
    started: AtomicUsize,
    mutations: AtomicUsize,
    stops: AtomicUsize,
    allow_cleanup: AtomicBool,
    cleaned: AtomicBool,
    dropped: AtomicBool,
}

struct Probe {
    counts: Arc<Counts>,
    panic: bool,
    store: Mutex<Option<Arc<dyn StateStoreProvider>>>,
}

impl Probe {
    async fn initialize_store(&self, store: Option<Arc<dyn StateStoreProvider>>) {
        struct Exit<'a>(&'a AtomicBool);
        impl Drop for Exit<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _exit = Exit(&self.counts.initialization_exited);
        self.counts.initialized.fetch_add(1, Ordering::SeqCst);
        *self.store.lock().expect("partial initialization") = store;
        tokio::task::yield_now().await;
        assert!(!self.panic, "original initialization panic");
    }
    async fn start_instance(&self) -> anyhow::Result<()> {
        self.counts.started.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop_instance(&self) -> anyhow::Result<()> {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            self.counts.allow_cleanup.load(Ordering::SeqCst),
            "cleanup is blocked"
        );
        self.store.lock().expect("partial initialization").take();
        self.counts.cleaned.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn status_value(&self) -> ComponentStatus {
        if self.counts.started.load(Ordering::SeqCst) != 0
            && !self.counts.cleaned.load(Ordering::SeqCst)
        {
            ComponentStatus::Running
        } else {
            ComponentStatus::Stopped
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.dropped.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Source for Probe {
    fn id(&self) -> &str {
        "initializer"
    }
    fn type_name(&self) -> &str {
        "initialization-probe"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn auto_start(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.initialize_store(context.state_store).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.start_instance().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_instance().await
    }
    async fn status(&self) -> ComponentStatus {
        self.status_value()
    }
    async fn subscribe(
        &self,
        _: drasi_lib::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.counts.mutations.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("this lifecycle probe does not produce data")
    }
    async fn set_bootstrap_provider(&self, _: Box<dyn drasi_lib::bootstrap::BootstrapProvider>) {
        self.counts.mutations.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Reaction for Probe {
    fn id(&self) -> &str {
        "initializer"
    }
    fn type_name(&self) -> &str {
        "initialization-probe"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        vec![]
    }
    fn auto_start(&self) -> bool {
        false
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.initialize_store(context.state_store).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.start_instance().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_instance().await
    }
    async fn status(&self) -> ComponentStatus {
        self.status_value()
    }
    async fn bootstrap(&self, _: drasi_lib::reactions::BootstrapContext) -> anyhow::Result<()> {
        self.counts.mutations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct RejectedBootstrap(Arc<AtomicBool>);

struct RejectedBackend;

#[async_trait]
impl BootstrapBackend for RejectedBackend {
    async fn fetch_snapshot(&self) -> std::result::Result<SnapshotStream, FetchError> {
        panic!("rejected bootstrap must not fetch a snapshot")
    }
    async fn fetch_outbox(&self, _: u64) -> std::result::Result<OutboxStream, FetchError> {
        panic!("rejected bootstrap must not fetch an outbox")
    }
    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        panic!("rejected bootstrap must not read progress")
    }
    async fn write_checkpoint(&self, _: &ReactionCheckpoint) -> anyhow::Result<()> {
        panic!("rejected bootstrap must not write progress")
    }
}

impl Drop for RejectedBootstrap {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl drasi_lib::bootstrap::BootstrapProvider for RejectedBootstrap {
    async fn bootstrap(
        &self,
        _: drasi_lib::bootstrap::BootstrapRequest,
        _: &drasi_lib::bootstrap::BootstrapContext,
        _: drasi_lib::channels::events::BootstrapEventSender,
        _: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<drasi_lib::bootstrap::BootstrapResult> {
        anyhow::bail!("rejected bootstrap must not run")
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("plugin runtime")
    })
}

extern "C" fn unused_executor(_: *mut c_void) -> *mut c_void {
    panic!("the lifecycle bridge must use the plugin runtime")
}

fn unexpected_fallback(_: &str, _: FfiLifecycleEventType, _: &str) {
    panic!("initialized instances must use their own lifecycle callback")
}

fn host_library() -> Arc<libloading::Library> {
    #[cfg(unix)]
    let library = libloading::os::unix::Library::this();
    #[cfg(windows)]
    let library = libloading::os::windows::Library::this().expect("host executable");
    Arc::new(library.into())
}

struct Factory {
    boxed: bool,
    library: Arc<libloading::Library>,
    counts: Mutex<Vec<Arc<Counts>>>,
}

impl Factory {
    fn new(boxed: bool) -> Self {
        Self {
            boxed,
            library: host_library(),
            counts: Mutex::new(vec![]),
        }
    }
    fn next(&self) -> Probe {
        let mut instances = self.counts.lock().expect("created instances");
        assert!(
            instances
                .iter()
                .all(|instance| instance.cleaned.load(Ordering::SeqCst)),
            "replacement construction preceded confirmed cleanup"
        );
        let counts = Arc::new(Counts::default());
        let panic = instances.is_empty();
        instances.push(counts.clone());
        Probe {
            counts,
            panic,
            store: Mutex::new(None),
        }
    }
    fn instances(&self) -> Vec<Arc<Counts>> {
        self.counts.lock().expect("created instances").clone()
    }
    fn reaction_table(&self) -> ReactionVtable {
        let reaction = self.next();
        if self.boxed {
            build_reaction_vtable_from_boxed(
                Box::new(reaction),
                unused_executor,
                unexpected_fallback,
                runtime,
            )
        } else {
            build_reaction_vtable(reaction, unused_executor, unexpected_fallback, runtime)
        }
    }
}

#[async_trait]
impl SourcePluginConstructor for Factory {
    async fn create(&self) -> anyhow::Result<Box<dyn Source>> {
        let source = self.next();
        let table = if self.boxed {
            build_source_vtable_from_boxed(
                Box::new(source),
                unused_executor,
                unexpected_fallback,
                runtime,
            )
        } else {
            build_source_vtable(source, unused_executor, unexpected_fallback, runtime)
        };
        Ok(Box::new(SourceProxy::new(table, self.library.clone())))
    }
}

#[async_trait]
impl ReactionPluginConstructor for Factory {
    async fn create(&self) -> anyhow::Result<Box<dyn Reaction>> {
        Ok(Box::new(ReactionProxy::new(
            self.reaction_table(),
            self.library.clone(),
        )))
    }
}

enum Plugin {
    Source(Box<dyn Source>),
    Reaction(Box<dyn Reaction>),
}

impl Plugin {
    async fn initialize(
        &self,
        store: Option<Arc<dyn StateStoreProvider>>,
    ) -> tokio::sync::mpsc::Receiver<ComponentUpdate> {
        let (sender, observer) = tokio::sync::mpsc::channel(32);
        match self {
            Self::Source(source) => {
                source
                    .initialize(SourceRuntimeContext::new(
                        "init-failure",
                        "initializer",
                        store,
                        sender,
                        None,
                    ))
                    .await
            }
            Self::Reaction(reaction) => {
                reaction
                    .initialize(ReactionRuntimeContext::new(
                        "init-failure",
                        "initializer",
                        store,
                        sender,
                        None,
                    ))
                    .await
            }
        }
        observer
    }
    async fn start(&self) -> anyhow::Result<()> {
        match self {
            Self::Source(source) => source.start().await,
            Self::Reaction(reaction) => reaction.start().await,
        }
    }
    async fn stop(&self) -> anyhow::Result<()> {
        match self {
            Self::Source(source) => source.stop().await,
            Self::Reaction(reaction) => reaction.stop().await,
        }
    }
    async fn status(&self) -> ComponentStatus {
        match self {
            Self::Source(source) => source.status().await,
            Self::Reaction(reaction) => reaction.status().await,
        }
    }
    async fn rejected_operations(&self) {
        match self {
            Self::Source(source) => {
                assert!(source
                    .subscribe(drasi_lib::config::SourceSubscriptionSettings {
                        source_id: "initializer".into(),
                        enable_bootstrap: false,
                        query_id: "query".into(),
                        nodes: Default::default(),
                        relations: Default::default(),
                        resume_from: None,
                        resume_sequence: None,
                        request_position_handle: false,
                    })
                    .await
                    .is_err());
                let released = Arc::new(AtomicBool::new(false));
                source
                    .set_bootstrap_provider(Box::new(RejectedBootstrap(released.clone())))
                    .await;
                assert!(
                    released.load(Ordering::SeqCst),
                    "rejected provider must be released"
                );
            }
            Self::Reaction(reaction) => {
                let error = reaction
                    .bootstrap(BootstrapContext::from_backend(
                        "query".into(),
                        false,
                        Box::new(RejectedBackend),
                    ))
                    .await
                    .expect_err("a poisoned reaction must reject bootstrap");
                assert!(
                    error.to_string().contains("original initialization panic"),
                    "{error:#}"
                );
            }
        }
    }
}

async fn wait_dropped(counts: &Counts) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !counts.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cleaned instance must release ownership");
}

#[tokio::test]
async fn generated_c_initializers_preserve_panics_and_forbid_reusing_the_instance() {
    for boxed in [false, true] {
        for source in [false, true] {
            let factory = Factory::new(boxed);
            let plugin = if source {
                Plugin::Source(SourcePluginConstructor::create(&factory).await.unwrap())
            } else {
                Plugin::Reaction(ReactionPluginConstructor::create(&factory).await.unwrap())
            };
            let counts = factory.instances().remove(0);
            let store = Arc::new(drasi_lib::MemoryStateStoreProvider::new());
            let weak = Arc::downgrade(&store);
            let mut observer = plugin.initialize(Some(store)).await;
            assert!(
                counts.initialization_exited.load(Ordering::SeqCst),
                "the initializer must finish unwinding before returning to the host"
            );
            let event = observer
                .try_recv()
                .expect("initialization failure must be reported");
            assert!(matches!(event, ComponentUpdate::Status {
                status: ComponentStatus::Error, message: Some(message), ..
            } if message.contains("original initialization panic")));
            assert_eq!(plugin.status().await, ComponentStatus::Error);
            let error = plugin.start().await.unwrap_err();
            assert!(
                error.to_string().contains("original initialization panic"),
                "{error:#}"
            );
            assert!(
                error.to_string().contains("recreate this plugin instance"),
                "{error:#}"
            );
            let rejected_store = Arc::new(drasi_lib::MemoryStateStoreProvider::new());
            let rejected_weak = Arc::downgrade(&rejected_store);
            let _retry_observer = plugin.initialize(Some(rejected_store)).await;
            assert!(
                rejected_weak.upgrade().is_none(),
                "rejected initialization must release its transferred store"
            );
            plugin.rejected_operations().await;
            assert_eq!(counts.mutations.load(Ordering::SeqCst), 0);
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(counts.started.load(Ordering::SeqCst), 0);
            assert!(plugin
                .stop()
                .await
                .unwrap_err()
                .to_string()
                .contains("cleanup is blocked"));
            assert!(
                weak.upgrade().is_some(),
                "failed cleanup must retain partial state"
            );
            counts.allow_cleanup.store(true, Ordering::SeqCst);
            plugin.stop().await.unwrap();
            let _cleanup_observer = plugin.initialize(None).await;
            plugin.rejected_operations().await;
            assert!(
                plugin.start().await.is_err(),
                "cleanup cannot rehabilitate a poisoned instance"
            );
            assert_eq!(counts.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(counts.started.load(Ordering::SeqCst), 0);
            assert_eq!(counts.mutations.load(Ordering::SeqCst), 0);
            drop(plugin);
            wait_dropped(&counts).await;
            assert!(weak.upgrade().is_none());
        }
    }
}

#[derive(Default)]
struct PushCounts {
    requested: AtomicUsize,
    exited: AtomicUsize,
}

extern "C" fn count_push(context: *mut c_void, sentinel: *mut c_void) -> *mut c_void {
    if sentinel.is_null() {
        let counts = unsafe { &*(context as *const PushCounts) };
        counts.requested.fetch_add(1, Ordering::SeqCst);
    } else {
        // The sentinel returns the one reference transferred by start_result_push_fn.
        let counts = unsafe { Arc::from_raw(context as *const PushCounts) };
        counts.exited.fetch_add(1, Ordering::SeqCst);
    }
    std::ptr::null_mut()
}

#[tokio::test]
async fn poisoned_reactions_reject_delivery_without_leaving_a_pending_forwarder() {
    for boxed in [false, true] {
        let factory = Factory::new(boxed);
        let table = factory.reaction_table();
        let state = table.state;
        let push = table.start_result_push_fn;
        let plugin = Plugin::Reaction(Box::new(ReactionProxy::new(table, factory.library.clone())));
        let _observer = plugin.initialize(None).await;
        let counts = Arc::new(PushCounts::default());
        push(
            state,
            count_push,
            Arc::into_raw(counts.clone()) as *mut c_void,
        );
        assert_eq!(counts.requested.load(Ordering::SeqCst), 0);
        assert_eq!(counts.exited.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&counts), 1);
        let instance = factory.instances().remove(0);
        instance.allow_cleanup.store(true, Ordering::SeqCst);
        plugin.stop().await.unwrap();
        drop(plugin);
        wait_dropped(&instance).await;
    }
}

#[tokio::test]
async fn separately_built_initializer_panic_retains_partial_state_until_cleanup() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(libloading::library_filename("drasi_reaction_snapshot_test"));
    assert!(
        path.is_file(),
        "build the fault fixture with `make build-test-plugins`: {}",
        path.display()
    );
    let library = drasi_host_sdk::loader::load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        drasi_host_sdk::callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        drasi_host_sdk::callbacks::default_lifecycle_callback_fn(),
    )
    .expect("load separately compiled failure fixture");
    let descriptor = library
        .reaction_plugins
        .first()
        .expect("reaction descriptor");
    assert_eq!(descriptor.kind(), "snapshot-test");
    let reaction = descriptor
        .create_reaction(
            "initializer",
            vec![],
            &serde_json::json!({"panic_on_initialize": true}),
            false,
        )
        .await
        .expect("construct fault fixture");
    assert_eq!(
        reaction.properties()["panic_on_initialize"],
        serde_json::json!(true),
        "an old fixture without failure injection cannot satisfy this case"
    );
    let plugin = Plugin::Reaction(reaction);
    let store = Arc::new(drasi_lib::MemoryStateStoreProvider::new());
    let weak = Arc::downgrade(&store);
    let mut observer = plugin.initialize(Some(store)).await;
    let event = observer.try_recv().expect("foreign initializer failure");
    assert!(matches!(event, ComponentUpdate::Status {
        status: ComponentStatus::Error, message: Some(message), ..
    } if message.contains("snapshot-test initialization panic")));
    assert!(
        weak.upgrade().is_some(),
        "partial state still belongs to the failed instance"
    );
    let error = plugin
        .start()
        .await
        .expect_err("failed instance cannot activate");
    assert!(
        error
            .to_string()
            .contains("snapshot-test initialization panic"),
        "{error:#}"
    );
    plugin.stop().await.expect("cleanup the failed instance");
    assert!(
        weak.upgrade().is_none(),
        "confirmed cleanup releases partial state"
    );
    let _retry_observer = plugin.initialize(None).await;
    assert_eq!(plugin.status().await, ComponentStatus::Error);
    assert!(
        plugin.start().await.is_err(),
        "successful stop does not undo initialization failure"
    );
    drop(plugin);

    let fresh = Plugin::Reaction(
        descriptor
            .create_reaction("initializer", vec![], &serde_json::json!({}), false)
            .await
            .expect("construct fresh instance after cleanup"),
    );
    let _fresh_observer = fresh.initialize(None).await;
    fresh.start().await.expect("fresh instance can activate");
    assert_eq!(fresh.status().await, ComponentStatus::Running);
    fresh.stop().await.expect("stop fresh instance");
}

#[tokio::test]
async fn framework_reconstructs_only_after_failed_initializer_cleanup_is_confirmed() {
    for boxed in [false, true] {
        for source in [false, true] {
            let factory = Arc::new(Factory::new(boxed));
            let mut services = LegacyPluginServices::empty("init-reconstruction");
            services.state_store = Some(Arc::new(drasi_lib::MemoryStateStoreProvider::new()));
            let id = ComponentId::try_new("initializer").unwrap();
            let mut component: Box<dyn ComputationComponent> = if source {
                let host = SourcePluginHost::recreatable(factory.clone(), services)
                    .await
                    .unwrap();
                let subscription = LegacySourceSubscription::new(
                    host,
                    SourceSubscriptionOptions::default(),
                    StreamId::try_new("initializer/out").unwrap(),
                    None,
                )
                .unwrap();
                Box::new(SourcePluginAdapter::new(id, subscription))
            } else {
                let host = ReactionPluginHost::recreatable(
                    factory.clone(),
                    services,
                    QueryResultsCatalog::new("init-reconstruction").unwrap(),
                    ReactionPluginOptions::default(),
                )
                .await
                .unwrap();
                Box::new(ReactionPluginAdapter::new(id, host).unwrap())
            };
            let error = component.start().await.unwrap_err();
            assert!(
                format!("{error:#}").contains("original initialization panic"),
                "{error:#}"
            );
            assert!(component.stop().await.is_err());
            assert!(component.start().await.is_err());
            assert_eq!(
                factory.instances().len(),
                1,
                "failed cleanup cannot construct a replacement"
            );
            let first = factory.instances().remove(0);
            assert!(!first.dropped.load(Ordering::SeqCst));
            first.allow_cleanup.store(true, Ordering::SeqCst);
            component.stop().await.unwrap();
            component.start().await.unwrap();
            assert_eq!(factory.instances().len(), 2);
            assert_eq!(first.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(first.started.load(Ordering::SeqCst), 0);
            wait_dropped(&first).await;
            let second = factory.instances().remove(1);
            assert_eq!(second.initialized.load(Ordering::SeqCst), 1);
            assert_eq!(second.started.load(Ordering::SeqCst), 1);
            second.allow_cleanup.store(true, Ordering::SeqCst);
            component.stop().await.unwrap();
            drop(component);
            wait_dropped(&second).await;
        }
    }
}
