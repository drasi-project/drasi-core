// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, ComponentUpdate, QueryResult};
use drasi_lib::identity::{CredentialContext, IdentityProvider, PasswordIdentityProvider};
use drasi_lib::reactions::Reaction;
use drasi_lib::sources::Source;
use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreProvider};
use drasi_lib::{ReactionRuntimeContext, SourceRuntimeContext};
use drasi_plugin_sdk::ffi::{
    build_reaction_vtable, build_reaction_vtable_from_boxed, build_source_vtable,
    build_source_vtable_from_boxed,
};
use tokio::sync::Notify;

use super::reaction::ReactionProxy;
use super::source::{resource_observer_tests::test_library, SourceProxy};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hook {
    Bind,
    Initialize,
    Start,
    Stop,
    Dispose,
    Pause,
    Resume,
    Delete,
}

#[derive(Default)]
struct Gate {
    operation: Mutex<Option<Hook>>,
    released: Mutex<bool>,
    changed: Condvar,
    entered: Notify,
}

impl Gate {
    fn arm(self: &Arc<Self>, operation: Hook) -> Release {
        *self.operation.lock().unwrap() = Some(operation);
        *self.released.lock().unwrap() = false;
        Release(self.clone())
    }

    fn wait(&self, operation: Hook) {
        if *self.operation.lock().unwrap() == Some(operation) {
            self.entered.notify_one();
            let released = self.released.lock().unwrap();
            let (_guard, wait) = self
                .changed
                .wait_timeout_while(released, Duration::from_secs(10), |released| !*released)
                .unwrap();
            assert!(
                !wait.timed_out(),
                "test did not release its blocking callback"
            );
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Default)]
struct Probe {
    calls: Mutex<Vec<Hook>>,
    fail: Mutex<Option<Hook>>,
    gate: Arc<Gate>,
    identity: Mutex<Option<String>>,
    received: Mutex<Vec<u64>>,
    instance_releases: AtomicUsize,
    activation_releases: AtomicUsize,
    object_drops: AtomicUsize,
    persisted: Mutex<Vec<u8>>,
}

impl Probe {
    fn enter(&self, operation: Hook) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(operation);
        self.gate.wait(operation);
        anyhow::ensure!(
            *self.fail.lock().unwrap() != Some(operation),
            "injected {operation:?} failure"
        );
        Ok(())
    }

    fn count(&self, operation: Hook) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == operation)
            .count()
    }
}

struct Lease {
    _buffer: Vec<u8>,
    probe: Arc<Probe>,
    activation: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.activation {
            self.probe
                .activation_releases
                .fetch_add(1, Ordering::SeqCst);
        } else {
            self.probe.instance_releases.fetch_add(1, Ordering::SeqCst);
        }
    }
}

struct Fixture {
    probe: Arc<Probe>,
    state_store: Mutex<Option<Arc<dyn StateStoreProvider>>>,
    instance: Mutex<Option<Lease>>,
    activation: Mutex<Option<Lease>>,
}

impl Fixture {
    async fn bind(
        &self,
        instance: &str,
        component: &drasi_core_types::ComponentId,
        provider: Option<Arc<dyn IdentityProvider>>,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) {
        assert_eq!(instance, "instance");
        assert_eq!(
            component,
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000033",)
                .expect("fixture UUID")
        );
        self.probe.enter(Hook::Bind).unwrap();
        let credentials = provider
            .unwrap()
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();
        *self.probe.identity.lock().unwrap() = Some(credentials.try_into_auth_pair().unwrap().0);
        assert!(state_store.is_some());
        *self.state_store.lock().unwrap() = state_store;
    }

    fn initialize(&self) -> anyhow::Result<()> {
        assert!(
            self.state_store.lock().unwrap().is_some(),
            "context must be wired first"
        );
        *self.instance.lock().unwrap() = Some(Lease {
            _buffer: vec![1; 32],
            probe: self.probe.clone(),
            activation: false,
        });
        self.probe.enter(Hook::Initialize)
    }

    fn start(&self) -> anyhow::Result<()> {
        assert!(self.instance.lock().unwrap().is_some());
        *self.activation.lock().unwrap() = Some(Lease {
            _buffer: vec![2; 32],
            probe: self.probe.clone(),
            activation: true,
        });
        self.probe.enter(Hook::Start)
    }

    fn stop(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Stop)?;
        self.activation.lock().unwrap().take();
        Ok(())
    }

    fn dispose(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Dispose)?;
        assert!(self.activation.lock().unwrap().is_none());
        self.instance.lock().unwrap().take();
        Ok(())
    }

    fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Delete)?;
        self.probe.persisted.lock().unwrap().clear();
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.probe.object_drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Source for Fixture {
    fn id(&self) -> &drasi_core_types::ComponentId {
        static ID: std::sync::LazyLock<drasi_core_types::ComponentId> =
            std::sync::LazyLock::new(|| {
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000033")
                    .expect("fixture UUID")
            });
        &ID
    }
    fn type_name(&self) -> &str {
        "ordinary-lifecycle-fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.bind(
            &context.instance_id,
            &context.source_id,
            context.identity_provider,
            context.state_store,
        )
        .await;
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        Fixture::initialize(self)
    }
    async fn start(&self) -> anyhow::Result<()> {
        Fixture::start(self)
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Fixture::stop(self)
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        Fixture::dispose(self)
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        Fixture::delete_owned_resources(self)
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Pause)
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Resume)
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
    async fn subscribe(
        &self,
        _: drasi_lib::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::SubscriptionResponse> {
        anyhow::bail!("no subscriptions in lifecycle fixture")
    }
}

#[async_trait]
impl Reaction for Fixture {
    fn id(&self) -> &drasi_core_types::ComponentId {
        static ID: std::sync::LazyLock<drasi_core_types::ComponentId> =
            std::sync::LazyLock::new(|| {
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000033")
                    .expect("fixture UUID")
            });
        &ID
    }
    fn type_name(&self) -> &str {
        "ordinary-lifecycle-fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
        vec![]
    }
    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        self.bind(
            &context.instance_id,
            &context.reaction_id,
            context.identity_provider,
            context.state_store,
        )
        .await;
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        Fixture::initialize(self)
    }
    async fn start(&self) -> anyhow::Result<()> {
        Fixture::start(self)
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Fixture::stop(self)
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        Fixture::dispose(self)
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        Fixture::delete_owned_resources(self)
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Pause)
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.probe.enter(Hook::Resume)
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        assert!(
            self.activation.lock().unwrap().is_some(),
            "forwarding outlived activation"
        );
        self.probe.received.lock().unwrap().push(result.sequence);
        Ok(())
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

extern "C" fn unused_executor(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

enum Proxy {
    Source(SourceProxy),
    Reaction(ReactionProxy),
}

struct Harness {
    proxy: Proxy,
    probe: Arc<Probe>,
    library: Weak<libloading::Library>,
    store: Arc<MemoryStateStoreProvider>,
    // Retain earlier contexts' receivers when duplicate binding is ignored.
    updates: Mutex<Vec<tokio::sync::mpsc::Receiver<ComponentUpdate>>>,
}

impl Harness {
    fn set_wait_timeout(&mut self, timeout: Duration) {
        match &mut self.proxy {
            Proxy::Source(proxy) => proxy.set_call_wait_timeout(timeout),
            Proxy::Reaction(proxy) => proxy.set_call_wait_timeout(timeout),
        }
    }

    fn new(source: bool, boxed: bool) -> Self {
        let probe = Arc::new(Probe::default());
        *probe.persisted.lock().unwrap() = b"checkpoint".to_vec();
        let fixture = Fixture {
            probe: probe.clone(),
            state_store: Mutex::new(None),
            instance: Mutex::new(None),
            activation: Mutex::new(None),
        };
        let library = test_library();
        let weak = Arc::downgrade(&library);
        let proxy = match (source, boxed) {
            (true, false) => Proxy::Source(
                SourceProxy::new(
                    build_source_vtable(fixture, unused_executor, |_, _, _| {}, runtime),
                    library,
                )
                .expect("source proxy"),
            ),
            (true, true) => Proxy::Source(
                SourceProxy::new(
                    build_source_vtable_from_boxed(
                        Box::new(fixture),
                        unused_executor,
                        |_, _, _| {},
                        runtime,
                    ),
                    library,
                )
                .expect("boxed source proxy"),
            ),
            (false, false) => Proxy::Reaction(
                ReactionProxy::new(
                    build_reaction_vtable(fixture, unused_executor, |_, _, _| {}, runtime),
                    library,
                )
                .expect("reaction proxy"),
            ),
            (false, true) => Proxy::Reaction(
                ReactionProxy::new(
                    build_reaction_vtable_from_boxed(
                        Box::new(fixture),
                        unused_executor,
                        |_, _, _| {},
                        runtime,
                    ),
                    library,
                )
                .expect("boxed reaction proxy"),
            ),
        };
        Self {
            proxy,
            probe,
            library: weak,
            store: Arc::new(MemoryStateStoreProvider::new()),
            updates: Mutex::new(Vec::new()),
        }
    }

    async fn call(&self, operation: Hook) -> anyhow::Result<()> {
        if operation == Hook::Bind {
            let identity: Arc<dyn IdentityProvider> =
                Arc::new(PasswordIdentityProvider::new("injected", "test"));
            let default_identity: Arc<dyn IdentityProvider> =
                Arc::new(PasswordIdentityProvider::new("default", "test"));
            let (updates, receiver) = tokio::sync::mpsc::channel(256);
            self.updates.lock().unwrap().push(receiver);
            match &self.proxy {
                Proxy::Source(proxy) => {
                    proxy.set_identity_provider(identity).await;
                    proxy
                        .bind_runtime_context(SourceRuntimeContext::new(
                            "instance",
                            drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-000000000033",
                            )
                            .expect("fixture UUID"),
                            Some(self.store.clone()),
                            updates,
                            Some(default_identity),
                        ))
                        .await;
                }
                Proxy::Reaction(proxy) => {
                    proxy.set_identity_provider(identity).await;
                    proxy
                        .bind_runtime_context(ReactionRuntimeContext::new(
                            "instance",
                            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000033")
                                .expect("fixture UUID"),
                            vec![],
                            Some(self.store.clone()),
                            updates,
                            Some(default_identity),
                        ))
                        .await;
                }
            }
            return Ok(());
        }
        macro_rules! invoke {
            ($proxy:expr) => {
                match operation {
                    Hook::Initialize => $proxy.initialize().await,
                    Hook::Start => $proxy.start().await,
                    Hook::Stop => $proxy.stop().await,
                    Hook::Dispose => $proxy.dispose().await,
                    Hook::Pause => $proxy.pause_processing().await,
                    Hook::Resume => $proxy.resume_processing().await,
                    Hook::Delete => $proxy.delete_owned_resources().await,
                    Hook::Bind => unreachable!(),
                }
            };
        }
        match &self.proxy {
            Proxy::Source(proxy) => invoke!(proxy),
            Proxy::Reaction(proxy) => invoke!(proxy),
        }
    }

    async fn send_result(&self, sequence: u64) {
        if let Proxy::Reaction(proxy) = &self.proxy {
            proxy
                .enqueue_query_result(QueryResult::new(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                        .expect("fixture UUID"),
                    sequence,
                    chrono::Utc::now(),
                    vec![],
                    HashMap::new(),
                ))
                .await
                .unwrap();
        }
    }

    fn assert_released(self) {
        let Self {
            proxy,
            probe,
            library,
            store,
            updates: _updates,
        } = self;
        let store_weak = Arc::downgrade(&store);
        drop(store);
        drop(proxy);
        assert_eq!(probe.object_drops.load(Ordering::SeqCst), 1);
        assert!(
            library.upgrade().is_none(),
            "plugin library ownership leaked after joined disposal"
        );
        assert!(
            store_weak.upgrade().is_none(),
            "transferred state-store ownership leaked"
        );
    }
}

async fn binding_restart_and_disposal_case() {
    use Hook::*;
    for source in [true, false] {
        for boxed in [false, true] {
            let harness = Harness::new(source, boxed);
            assert!(harness.call(Initialize).await.is_err());
            harness.call(Bind).await.unwrap();
            harness.call(Bind).await.unwrap();
            assert_eq!(harness.probe.count(Bind), 1);
            assert_eq!(harness.probe.count(Initialize), 0);
            assert_eq!(
                harness.probe.identity.lock().unwrap().as_deref(),
                Some("injected")
            );
            harness.call(Initialize).await.unwrap();
            harness.call(Initialize).await.unwrap();
            for sequence in 1..=2 {
                let (first, repeated) = tokio::join!(harness.call(Start), harness.call(Start));
                first.unwrap();
                repeated.unwrap();
                harness.call(Pause).await.unwrap();
                harness.call(Resume).await.unwrap();
                harness.send_result(sequence).await;
                harness.call(Stop).await.unwrap();
                harness.call(Stop).await.unwrap();
            }
            assert_eq!(harness.probe.count(Initialize), 1);
            assert_eq!(harness.probe.count(Start), 2);
            assert_eq!(harness.probe.count(Stop), 2);
            assert_eq!(harness.probe.activation_releases.load(Ordering::SeqCst), 2);
            if !source {
                assert_eq!(*harness.probe.received.lock().unwrap(), [1, 2]);
            }
            harness.call(Dispose).await.unwrap();
            harness.call(Dispose).await.unwrap();
            assert_eq!(harness.probe.count(Dispose), 1);
            assert_eq!(harness.probe.count(Delete), 0);
            assert_eq!(*harness.probe.persisted.lock().unwrap(), b"checkpoint");
            harness.assert_released();
        }
    }
}

async fn failures_and_cleanup_retry_case() {
    use Hook::*;
    for source in [true, false] {
        let binding = Harness::new(source, true);
        *binding.probe.fail.lock().unwrap() = Some(Bind);
        binding.call(Bind).await.unwrap();
        let error = binding.call(Initialize).await.unwrap_err();
        assert!(
            error.to_string().contains("injected Bind failure"),
            "{error:#}"
        );
        assert_eq!(binding.probe.count(Initialize), 0);
        assert!(binding.call(Start).await.is_err());
        *binding.probe.fail.lock().unwrap() = None;
        binding.call(Dispose).await.unwrap();
        binding.assert_released();

        for failure in [Initialize, Start] {
            let harness = Harness::new(source, true);
            harness.call(Bind).await.unwrap();
            if failure == Start {
                harness.call(Initialize).await.unwrap();
            }
            *harness.probe.fail.lock().unwrap() = Some(failure);
            assert!(harness
                .call(failure)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected"));
            assert!(harness.call(Start).await.is_err());
            assert_eq!(harness.probe.count(failure), 1);
            if failure == Start {
                *harness.probe.fail.lock().unwrap() = Some(Stop);
                assert!(harness.call(Stop).await.is_err());
                assert!(harness.call(Start).await.is_err());
                assert!(harness.call(Dispose).await.is_err());
                assert_eq!(harness.probe.activation_releases.load(Ordering::SeqCst), 0);
                *harness.probe.fail.lock().unwrap() = None;
                harness.call(Stop).await.unwrap();
                harness.call(Start).await.unwrap();
                harness.call(Stop).await.unwrap();
            }
            *harness.probe.fail.lock().unwrap() = Some(Dispose);
            assert!(harness
                .call(Dispose)
                .await
                .unwrap_err()
                .to_string()
                .contains("injected Dispose failure"));
            assert_eq!(harness.probe.instance_releases.load(Ordering::SeqCst), 0);
            *harness.probe.fail.lock().unwrap() = None;
            harness.call(Dispose).await.unwrap();
            assert_eq!(harness.probe.instance_releases.load(Ordering::SeqCst), 1);
            assert_eq!(harness.probe.count(Delete), 0);
            harness.assert_released();
        }
        let harness = Harness::new(source, true);
        harness.call(Bind).await.unwrap();
        harness.call(Initialize).await.unwrap();
        *harness.probe.fail.lock().unwrap() = Some(Delete);
        assert!(harness.call(Delete).await.is_err());
        assert!(harness.call(Dispose).await.is_err());
        assert_eq!(*harness.probe.persisted.lock().unwrap(), b"checkpoint");
        *harness.probe.fail.lock().unwrap() = None;
        harness.call(Delete).await.unwrap();
        harness.call(Delete).await.unwrap();
        harness.call(Dispose).await.unwrap();
        assert_eq!(harness.probe.count(Delete), 2);
        assert!(harness.probe.persisted.lock().unwrap().is_empty());
        harness.assert_released();
    }
}

async fn cancelled_wait_retains_callback_and_prevents_overlap_case() {
    use Hook::*;
    for source in [true, false] {
        for operation in [Bind, Initialize, Start, Stop, Dispose, Delete] {
            let harness = Harness::new(source, true);
            if operation != Bind {
                harness.call(Bind).await.unwrap();
            }
            if !matches!(operation, Bind | Initialize) {
                harness.call(Initialize).await.unwrap();
            }
            if operation == Stop {
                harness.call(Start).await.unwrap();
            }
            let release = harness.probe.gate.arm(operation);
            let mut waiting = Box::pin(harness.call(operation));
            tokio::select! {
                result = &mut waiting => panic!("blocking {operation:?} completed early: {result:?}"),
                _ = harness.probe.gate.entered.notified() => {}
            }
            // This timer must run while the plugin is synchronously blocked,
            // including on a current_thread host runtime.
            assert!(
                tokio::time::timeout(Duration::from_millis(15), waiting.as_mut())
                    .await
                    .is_err()
            );
            drop(waiting);
            assert_eq!(harness.probe.object_drops.load(Ordering::SeqCst), 0);
            assert!(harness.library.upgrade().is_some());
            let conflicting = if operation == Dispose { Start } else { Dispose };
            let before = harness.probe.count(conflicting);
            assert!(
                tokio::time::timeout(Duration::from_millis(15), harness.call(conflicting))
                    .await
                    .is_err()
            );
            assert_eq!(
                harness.probe.count(conflicting),
                before,
                "conflicting callback overlapped {operation:?}"
            );
            drop(release);
            harness.call(operation).await.unwrap();
            assert_eq!(
                harness.probe.count(operation),
                1,
                "retry repeated a retained {operation:?} callback"
            );
            if operation == Start {
                harness.call(Stop).await.unwrap();
            }
            if operation != Dispose {
                harness.call(Dispose).await.unwrap();
            }
            harness.assert_released();
        }
    }
}

async fn timeout_and_late_failure_case() {
    use Hook::*;
    for source in [true, false] {
        let mut harness = Harness::new(source, true);
        for operation in [Bind, Initialize, Start] {
            harness.call(operation).await.unwrap();
        }
        harness.set_wait_timeout(Duration::from_millis(20));
        let release = harness.probe.gate.arm(Stop);
        *harness.probe.fail.lock().unwrap() = Some(Stop);
        let error = harness.call(Stop).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot be force-cancelled safely"),
            "{error:#}"
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            harness.probe.gate.entered.notified(),
        )
        .await
        .unwrap();
        assert_eq!(harness.probe.count(Stop), 1);
        let error = harness.call(Start).await.unwrap_err();
        assert!(
            error.to_string().contains("Start has not been submitted"),
            "{error:#}"
        );
        assert!(format!("{error:#}").contains("retained"), "{error:#}");
        assert_eq!(harness.probe.count(Start), 1);
        drop(release);
        harness.set_wait_timeout(Duration::from_secs(2));
        let error = harness.call(Stop).await.unwrap_err();
        assert!(
            error.to_string().contains("injected Stop failure"),
            "{error:#}"
        );
        assert_eq!(
            harness.probe.count(Stop),
            1,
            "late failure must be observed before retry"
        );
        assert_eq!(harness.probe.activation_releases.load(Ordering::SeqCst), 0);
        *harness.probe.fail.lock().unwrap() = None;
        harness.call(Stop).await.unwrap();
        assert_eq!(harness.probe.count(Stop), 2);
        harness.call(Dispose).await.unwrap();
        harness.assert_released();
    }
}

macro_rules! runtime_cases {
    ($case:ident, $current:ident, $multi:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $current() {
            $case().await
        }
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $multi() {
            $case().await
        }
    };
}

runtime_cases!(
    binding_restart_and_disposal_case,
    ordinary_lifecycle_binding_restart_disposal_current_thread,
    ordinary_lifecycle_binding_restart_disposal_multi_thread
);
runtime_cases!(
    failures_and_cleanup_retry_case,
    ordinary_lifecycle_failures_and_cleanup_retry_current_thread,
    ordinary_lifecycle_failures_and_cleanup_retry_multi_thread
);
runtime_cases!(
    cancelled_wait_retains_callback_and_prevents_overlap_case,
    ordinary_lifecycle_cancelled_wait_and_responsiveness_current_thread,
    ordinary_lifecycle_cancelled_wait_and_responsiveness_multi_thread
);
runtime_cases!(
    timeout_and_late_failure_case,
    ordinary_lifecycle_timeout_and_late_failure_current_thread,
    ordinary_lifecycle_timeout_and_late_failure_multi_thread
);
