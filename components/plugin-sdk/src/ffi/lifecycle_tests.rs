// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Default)]
struct Probe {
    calls: Mutex<Vec<&'static str>>,
    fail: Mutex<Option<&'static str>>,
    panic_initialize: AtomicBool,
    panic_binding: AtomicBool,
    bound: AtomicBool,
    instance_releases: AtomicUsize,
    activation_releases: AtomicUsize,
    object_drops: AtomicUsize,
    persisted: Mutex<Vec<u8>>,
}

impl Probe {
    fn enter(&self, operation: &'static str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(operation);
        anyhow::ensure!(
            *self.fail.lock().unwrap() != Some(operation),
            "injected {operation} failure"
        );
        Ok(())
    }

    fn count(&self, operation: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == operation)
            .count()
    }
}

struct Lease {
    _allocation: Vec<u8>,
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
    instance: Mutex<Option<Lease>>,
    activation: Mutex<Option<Lease>>,
}

impl Fixture {
    fn new(probe: Arc<Probe>) -> Self {
        Self {
            probe,
            instance: Mutex::new(None),
            activation: Mutex::new(None),
        }
    }

    fn bind(&self, instance_id: &str, component_id: &str) {
        assert_eq!(instance_id, "instance");
        assert_eq!(component_id, "component");
        self.probe.enter("bind").unwrap();
        assert!(
            !self.probe.panic_binding.load(Ordering::SeqCst),
            "binding panic"
        );
        self.probe.bound.store(true, Ordering::SeqCst);
    }

    fn initialize(&self) -> anyhow::Result<()> {
        assert!(self.probe.bound.load(Ordering::SeqCst));
        *self.instance.lock().unwrap() = Some(Lease {
            _allocation: vec![1; 64],
            probe: self.probe.clone(),
            activation: false,
        });
        self.probe.enter("initialize")?;
        assert!(
            !self.probe.panic_initialize.load(Ordering::SeqCst),
            "initializer panic"
        );
        Ok(())
    }

    fn start(&self) -> anyhow::Result<()> {
        assert!(self.instance.lock().unwrap().is_some());
        *self.activation.lock().unwrap() = Some(Lease {
            _allocation: vec![2; 64],
            probe: self.probe.clone(),
            activation: true,
        });
        self.probe.enter("start")
    }

    fn stop(&self) -> anyhow::Result<()> {
        self.probe.enter("stop")?;
        self.activation.lock().unwrap().take();
        Ok(())
    }

    fn dispose(&self) -> anyhow::Result<()> {
        self.probe.enter("dispose")?;
        assert!(self.activation.lock().unwrap().is_none());
        self.instance.lock().unwrap().take();
        Ok(())
    }

    fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.probe.enter("delete")?;
        self.probe.persisted.lock().unwrap().clear();
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.probe.object_drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Source for Fixture {
    fn id(&self) -> &str {
        "component"
    }
    fn type_name(&self) -> &str {
        "lifecycle-fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.bind(&context.instance_id, &context.source_id);
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
        self.probe.enter("pause")
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.probe.enter("resume")
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
    async fn subscribe(
        &self,
        _: SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::SubscriptionResponse> {
        anyhow::bail!("no subscriptions in lifecycle fixture")
    }
}

#[async_trait::async_trait]
impl Reaction for Fixture {
    fn id(&self) -> &str {
        "component"
    }
    fn type_name(&self) -> &str {
        "lifecycle-fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        vec![]
    }
    async fn bind_runtime_context(&self, context: drasi_lib::ReactionRuntimeContext) {
        self.bind(&context.instance_id, &context.reaction_id);
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
        self.probe.enter("pause")
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.probe.enter("resume")
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
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

enum Table {
    Source(SourceVtable),
    Reaction(ReactionVtable),
}

impl Table {
    fn new(source: bool, boxed: bool, probe: Arc<Probe>) -> Self {
        let fixture = Fixture::new(probe);
        match (source, boxed) {
            (true, false) => Self::Source(build_source_vtable(
                fixture,
                unused_executor,
                |_, _, _| {},
                runtime,
            )),
            (true, true) => Self::Source(build_source_vtable_from_boxed(
                Box::new(fixture),
                unused_executor,
                |_, _, _| {},
                runtime,
            )),
            (false, false) => Self::Reaction(build_reaction_vtable(
                fixture,
                unused_executor,
                |_, _, _| {},
                runtime,
            )),
            (false, true) => Self::Reaction(build_reaction_vtable_from_boxed(
                Box::new(fixture),
                unused_executor,
                |_, _, _| {},
                runtime,
            )),
        }
    }

    fn bind(&self) -> Result<(), String> {
        let context = FfiRuntimeContext {
            instance_id: FfiStr::from_str("instance"),
            component_id: FfiStr::from_str("component"),
            state_store: std::ptr::null(),
            log_callback: None,
            log_ctx: std::ptr::null_mut(),
            lifecycle_callback: None,
            lifecycle_ctx: std::ptr::null_mut(),
            identity_provider: std::ptr::null(),
            snapshot_fetcher: std::ptr::null(),
            wal_provider: std::ptr::null(),
        };
        let result = match self {
            Self::Source(table) => (table.bind_runtime_context_fn)(table.state, &context),
            Self::Reaction(table) => (table.bind_runtime_context_fn)(table.state, &context),
        };
        unsafe { result.into_result() }
    }

    fn call(&self, operation: LifecycleOperation) -> Result<(), String> {
        use LifecycleOperation::*;
        macro_rules! invoke {
            ($table:expr) => {{
                let table = $table;
                let callback = match operation {
                    Initialize => table.initialize_fn,
                    Start => table.start_fn,
                    Stop => table.stop_fn,
                    Dispose => table.dispose_fn,
                    PauseProcessing => table.pause_processing_fn,
                    ResumeProcessing => table.resume_processing_fn,
                    DeleteOwnedResources => table.delete_owned_resources_fn,
                };
                unsafe { callback(table.state).into_result() }
            }};
        }
        match self {
            Self::Source(table) => invoke!(table),
            Self::Reaction(table) => invoke!(table),
        }
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        match self {
            Self::Source(table) => (table.drop_fn)(table.state),
            Self::Reaction(table) => (table.drop_fn)(table.state),
        }
    }
}

fn each_table(test: impl Fn(Table, Arc<Probe>)) {
    for source in [true, false] {
        for boxed in [false, true] {
            let probe = Arc::new(Probe::default());
            *probe.persisted.lock().unwrap() = b"checkpoint".to_vec();
            test(Table::new(source, boxed, probe.clone()), probe);
        }
    }
}

#[test]
fn ordinary_lifecycle_binding_is_distinct_and_successful_transitions_run_once() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        assert!(table.call(Initialize).unwrap_err().contains("context"));
        table.bind().unwrap();
        table.bind().unwrap();
        assert_eq!(*probe.calls.lock().unwrap(), ["bind"]);
        table.call(Initialize).unwrap();
        table.call(Initialize).unwrap();
        table.call(Stop).unwrap();
        for _ in 0..2 {
            table.call(Start).unwrap();
            table.call(Start).unwrap();
            table.call(PauseProcessing).unwrap();
            table.call(PauseProcessing).unwrap();
            table.call(ResumeProcessing).unwrap();
            table.call(ResumeProcessing).unwrap();
            table.call(Stop).unwrap();
            table.call(Stop).unwrap();
        }
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 0);
        assert_eq!(probe.activation_releases.load(Ordering::SeqCst), 2);
        table.call(Dispose).unwrap();
        table.call(Dispose).unwrap();
        assert_eq!(
            *probe.calls.lock().unwrap(),
            [
                "bind",
                "initialize",
                "start",
                "pause",
                "resume",
                "stop",
                "start",
                "pause",
                "resume",
                "stop",
                "dispose",
            ]
        );
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 1);
        assert_eq!(*probe.persisted.lock().unwrap(), b"checkpoint");
        assert!(table.call(Start).is_err());
        drop(table);
        assert_eq!(probe.object_drops.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn ordinary_lifecycle_initialized_but_never_started_is_disposed_without_deletion() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        table.bind().unwrap();
        table.call(Initialize).unwrap();
        table.call(Dispose).unwrap();
        assert_eq!(
            *probe.calls.lock().unwrap(),
            ["bind", "initialize", "dispose"]
        );
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 1);
        assert_eq!(probe.activation_releases.load(Ordering::SeqCst), 0);
        assert_eq!(*probe.persisted.lock().unwrap(), b"checkpoint");
    });
}

#[test]
fn ordinary_lifecycle_partial_initialization_is_not_retried_and_disposal_failure_is_retryable() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        table.bind().unwrap();
        *probe.fail.lock().unwrap() = Some("initialize");
        assert!(table
            .call(Initialize)
            .unwrap_err()
            .contains("injected initialize failure"));
        assert!(table.call(Initialize).is_err());
        assert!(table.call(Start).is_err());
        assert_eq!(probe.count("initialize"), 1);
        assert_eq!(probe.count("start"), 0);
        *probe.fail.lock().unwrap() = Some("dispose");
        assert!(table
            .call(Dispose)
            .unwrap_err()
            .contains("injected dispose failure"));
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 0);
        *probe.fail.lock().unwrap() = None;
        table.call(Dispose).unwrap();
        table.call(Dispose).unwrap();
        assert_eq!(probe.count("dispose"), 2);
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 1);
        assert_eq!(*probe.persisted.lock().unwrap(), b"checkpoint");
    });
}

#[test]
fn ordinary_lifecycle_partial_start_stop_and_destructive_cleanup_remain_fenced_until_retry() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        table.bind().unwrap();
        table.call(Initialize).unwrap();
        *probe.fail.lock().unwrap() = Some("start");
        assert!(table
            .call(Start)
            .unwrap_err()
            .contains("injected start failure"));
        assert!(table.call(Start).is_err());
        assert!(table.call(Dispose).is_err());
        assert_eq!(probe.count("start"), 1);
        *probe.fail.lock().unwrap() = Some("stop");
        assert!(table
            .call(Stop)
            .unwrap_err()
            .contains("injected stop failure"));
        assert!(table.call(Start).is_err());
        assert_eq!(probe.activation_releases.load(Ordering::SeqCst), 0);
        *probe.fail.lock().unwrap() = None;
        table.call(Stop).unwrap();
        table.call(Start).unwrap();
        table.call(Stop).unwrap();
        assert_eq!(probe.activation_releases.load(Ordering::SeqCst), 2);
        *probe.fail.lock().unwrap() = Some("delete");
        assert!(table
            .call(DeleteOwnedResources)
            .unwrap_err()
            .contains("injected delete failure"));
        assert!(table.call(Start).is_err());
        assert!(table.call(Dispose).is_err());
        assert_eq!(*probe.persisted.lock().unwrap(), b"checkpoint");
        *probe.fail.lock().unwrap() = None;
        table.call(DeleteOwnedResources).unwrap();
        table.call(DeleteOwnedResources).unwrap();
        table.call(Dispose).unwrap();
        assert_eq!(probe.count("delete"), 2);
        assert!(probe.persisted.lock().unwrap().is_empty());
    });
}

#[test]
fn ordinary_lifecycle_partial_pause_and_resume_do_not_claim_successful_recovery() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        table.bind().unwrap();
        table.call(Initialize).unwrap();
        table.call(Start).unwrap();
        *probe.fail.lock().unwrap() = Some("pause");
        assert!(table
            .call(PauseProcessing)
            .unwrap_err()
            .contains("injected pause failure"));
        assert!(table.call(PauseProcessing).is_err());
        assert_eq!(probe.count("pause"), 2);
        *probe.fail.lock().unwrap() = Some("resume");
        assert!(table
            .call(ResumeProcessing)
            .unwrap_err()
            .contains("injected resume failure"));
        *probe.fail.lock().unwrap() = None;
        table.call(ResumeProcessing).unwrap();
        assert_eq!(probe.count("resume"), 2);
        table.call(Stop).unwrap();
        table.call(Dispose).unwrap();
    });
}

#[test]
fn ordinary_lifecycle_panics_cross_the_real_vtable_as_errors_and_allow_disposal() {
    use LifecycleOperation::*;
    each_table(|table, probe| {
        table.bind().unwrap();
        probe.panic_initialize.store(true, Ordering::SeqCst);
        assert!(table
            .call(Initialize)
            .unwrap_err()
            .contains("initializer panic"));
        assert!(table.call(Start).is_err());
        table.call(Dispose).unwrap();
        assert_eq!(probe.instance_releases.load(Ordering::SeqCst), 1);
    });
    each_table(|table, probe| {
        probe.panic_binding.store(true, Ordering::SeqCst);
        assert!(table.bind().unwrap_err().contains("binding panic"));
        assert!(table
            .call(Initialize)
            .unwrap_err()
            .contains("binding panic"));
        assert_eq!(probe.count("initialize"), 0);
        table.call(Dispose).unwrap();
    });
}

struct Noop(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Source for Noop {
    fn id(&self) -> &str {
        "component"
    }
    fn type_name(&self) -> &str {
        "noop"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        assert_eq!(context.source_id, "component");
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
    async fn subscribe(
        &self,
        _: SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::SubscriptionResponse> {
        anyhow::bail!("no subscriptions")
    }
}

#[async_trait::async_trait]
impl Reaction for Noop {
    fn id(&self) -> &str {
        "component"
    }
    fn type_name(&self) -> &str {
        "noop"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        vec![]
    }
    async fn bind_runtime_context(&self, context: drasi_lib::ReactionRuntimeContext) {
        assert_eq!(context.reaction_id, "component");
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
}

#[test]
fn ordinary_lifecycle_noop_customization_still_requires_context_binding() {
    use LifecycleOperation::*;
    for source in [true, false] {
        let count = Arc::new(AtomicUsize::new(0));
        let component = Noop(count.clone());
        let table = if source {
            Table::Source(build_source_vtable(
                component,
                unused_executor,
                |_, _, _| {},
                runtime,
            ))
        } else {
            Table::Reaction(build_reaction_vtable(
                component,
                unused_executor,
                |_, _, _| {},
                runtime,
            ))
        };
        assert!(table.call(Initialize).is_err());
        assert_eq!(count.load(Ordering::SeqCst), 0);
        table.bind().unwrap();
        for operation in [Initialize, Start, Stop, Start, Stop, Dispose] {
            table.call(operation).unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
