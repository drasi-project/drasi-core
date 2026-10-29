// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    abi::bootstrap_coordination::*,
    bootstrap_coordination::{SourceBootstrapLiveProgress, SourceBootstrapProgress},
    NativeSourceBootstrapCoordinator,
};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentId,
    ComputationBootstrapSnapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll},
};

pub(super) static EXTENSION: PluginBootstrapCoordinationV1 = PluginBootstrapCoordinationV1 {
    header: Header::new::<PluginBootstrapCoordinationV1>(),
    version: VERSION,
    reserved: 0,
    source_factory: Some(source_factory),
    bootstrap_factory: Some(bootstrap_factory),
    create: Some(create),
    create_source: Some(create_source),
    create_bootstrap: Some(create_bootstrap),
};

struct Lifecycle {
    retiring: bool,
    source: Option<Arc<SourceState>>,
    providers: BTreeMap<ComponentId, Arc<ProviderState>>,
}

struct SourceState {
    released: AtomicBool,
    cleanup_required: AtomicBool,
}

struct ProviderState {
    stopped: AtomicBool,
    retired: AtomicBool,
}

pub(super) struct Coordinator {
    plugin: Arc<PluginState>,
    index: usize,
    request: CreateRequest,
    progress: SourceBootstrapProgress,
    engine: Arc<dyn NativeSourceBootstrapCoordinator>,
    lifecycle: Mutex<Lifecycle>,
    stopping: AtomicBool,
    stopped: AtomicBool,
    damaged: Arc<transport::PanicState>,
}

impl Coordinator {
    fn active(&self) -> Result<std::sync::MutexGuard<'_, Lifecycle>, Failure> {
        if self.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let state = self
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("bootstrap coordinator lifecycle lock is poisoned"))?;
        if state.retiring {
            return Err(Failure::closed());
        }
        Ok(state)
    }

    pub(super) fn check_active(&self) -> anyhow::Result<()> {
        drop(self.active()?);
        Ok(())
    }

    fn invoke<T>(&self, operation: impl FnOnce() -> anyhow::Result<T>) -> Result<T, Failure> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
            Ok(result) => result.map_err(Failure::from),
            Err(_) => {
                self.damaged.quarantine();
                Err(Failure::panicked())
            }
        }
    }

    fn member(&self, query: &ComponentId) -> Result<SourceProgressView, Failure> {
        match &self.progress {
            SourceBootstrapProgress::Single(reader) if reader.component_id() == query => {
                Ok(reader.clone())
            }
            SourceBootstrapProgress::Shared { members, .. } => members
                .get(query)
                .cloned()
                .ok_or_else(|| Failure::protocol("query is not a bootstrap coordinator member")),
            _ => Err(Failure::protocol(
                "query is not the bootstrap coordinator owner",
            )),
        }
    }
}

pub(super) struct SourceLease {
    owner: Arc<Coordinator>,
    state: Arc<SourceState>,
}

impl SourceLease {
    pub(super) fn check_active(&self) -> anyhow::Result<()> {
        self.owner.check_active()
    }

    pub(super) fn starting(&self) -> anyhow::Result<()> {
        self.check_active()?;
        self.state
            .cleanup_required
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                anyhow::anyhow!("coordinated source requires successful cleanup before restart")
            })?;
        Ok(())
    }

    pub(super) fn stopping(&self) {
        self.state.cleanup_required.store(true, Ordering::Release);
    }

    pub(super) fn stopped(&self) {
        self.state.cleanup_required.store(false, Ordering::Release);
    }

    pub(super) fn create_source(
        &self,
        request: &CreateRequest,
        control: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> anyhow::Result<CreatedComponent> {
        let _state = self.owner.active()?;
        anyhow::ensure!(
            matches!(
                (&self.owner.progress, &live),
                (
                    SourceBootstrapProgress::Single(_),
                    SourceBootstrapLiveProgress::Single(_)
                ) | (
                    SourceBootstrapProgress::Shared { .. },
                    SourceBootstrapLiveProgress::Shared(_)
                )
            ),
            "bootstrap coordinator and live source progress modes differ"
        );
        match (&self.owner.progress, &live) {
            (
                SourceBootstrapProgress::Single(expected),
                SourceBootstrapLiveProgress::Single(actual),
            ) => {
                anyhow::ensure!(
                    expected.graph_id() == actual.graph_id()
                        && expected.component_id() == actual.component_id(),
                    "live source progress does not match its bootstrap owner"
                );
            }
            (
                SourceBootstrapProgress::Shared {
                    consumers: expected,
                    ..
                },
                SourceBootstrapLiveProgress::Shared(actual),
            ) => {
                anyhow::ensure!(
                    expected.graph_id() == actual.graph_id()
                        && expected.producer_id() == actual.producer_id(),
                    "live source group does not match its bootstrap group"
                );
            }
            _ => unreachable!("validated progress mode"),
        }
        self.owner
            .invoke(|| self.owner.engine.create_source(request, control, live))
            .map_err(Into::into)
    }

    pub(super) fn validate_consumers(&self, consumers: &[ComponentId]) -> anyhow::Result<()> {
        let SourceBootstrapProgress::Shared { members, .. } = &self.owner.progress else {
            anyhow::bail!("live group cannot bind a single-owner bootstrap coordinator");
        };
        anyhow::ensure!(
            consumers.iter().collect::<BTreeSet<_>>() == members.keys().collect::<BTreeSet<_>>(),
            "live source and bootstrap coordinator member sets differ"
        );
        Ok(())
    }
}

impl Drop for SourceLease {
    fn drop(&mut self) {
        self.state.released.store(true, Ordering::Release);
    }
}

struct ProviderLease {
    owner: Arc<Coordinator>,
    state: Arc<ProviderState>,
}

impl ProviderLease {
    fn check(&self, begin: bool) -> anyhow::Result<()> {
        let _state = self.owner.active()?;
        anyhow::ensure!(
            !self.state.retired.load(Ordering::Acquire),
            "paired bootstrap provider was replaced"
        );
        if begin {
            self.state.stopped.store(false, Ordering::Release);
        }
        Ok(())
    }
}

struct Provider {
    inner: Arc<dyn crate::NativeBootstrapProvider>,
    lease: Arc<ProviderLease>,
}

impl Provider {
    fn active(&self) -> anyhow::Result<()> {
        self.lease.check(false)
    }

    fn wrap(&self, mut snapshot: ComputationBootstrapSnapshot) -> ComputationBootstrapSnapshot {
        snapshot.changes = Box::pin(PairedStream {
            inner: snapshot.changes,
            lease: self.lease.clone(),
            failed: false,
        });
        snapshot
    }
}

#[async_trait]
impl crate::NativeBootstrapProvider for Provider {
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        query: &ComponentId,
    ) -> anyhow::Result<()> {
        self.active()?;
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.inner.recovery_reader()
    }
    async fn stop(&self) -> anyhow::Result<()> {
        {
            let _state =
                self.lease.owner.lifecycle.lock().map_err(|_| {
                    anyhow::anyhow!("bootstrap coordinator lifecycle lock is poisoned")
                })?;
            if self.lease.state.retired.load(Ordering::Acquire) {
                return Ok(());
            }
            self.lease.state.stopped.store(false, Ordering::Release);
        }
        self.inner.stop().await?;
        self.lease.state.stopped.store(true, Ordering::Release);
        Ok(())
    }
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        self.lease.check(true)?;
        let result = self.inner.prepare().await?;
        self.active()?;
        Ok(result)
    }
    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        self.lease.check(true)?;
        let result = self.inner.prepare_with_state(state).await?;
        self.active()?;
        Ok(result)
    }
    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        self.active()?;
        self.inner.has_pending_snapshot()
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.lease.check(true)?;
        let snapshot = self.inner.snapshot().await?;
        self.active()?;
        Ok(self.wrap(snapshot))
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.lease.check(true)?;
        let snapshot = self.inner.snapshot_with_state(state).await?;
        self.active()?;
        Ok(self.wrap(snapshot))
    }
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        self.lease.check(true)?;
        let result = self.inner.complete_snapshot().await?;
        self.active()?;
        Ok(result)
    }
    fn completion_state(&self) -> anyhow::Result<Option<bytes::Bytes>> {
        self.active()?;
        self.inner.completion_state()
    }
}

struct PairedStream {
    inner: Pin<
        Box<
            dyn futures_core::Stream<
                    Item = anyhow::Result<drasi_computation_contracts::ChangeEnvelope>,
                > + Send,
        >,
    >,
    lease: Arc<ProviderLease>,
    failed: bool,
}

impl futures_core::Stream for PairedStream {
    type Item = anyhow::Result<drasi_computation_contracts::ChangeEnvelope>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.failed {
            return Poll::Ready(None);
        }
        if let Err(error) = self.lease.check(false) {
            self.failed = true;
            return Poll::Ready(Some(Err(error)));
        }
        self.inner.as_mut().poll_next(cx)
    }
}

unsafe extern "C" fn source_factory(raw: *mut c_void, index: u32) -> Reply {
    transport::reply_boundary(|| {
        let plugin = unsafe { state_ref::<Arc<PluginState>>(raw)? };
        let factory = plugin
            .definition
            .factories
            .get(index as usize)
            .ok_or_else(|| Failure::protocol("unknown source/bootstrap factory"))?;
        wire::encode(&factory.supports_bootstrap_coordination()).map_err(Failure::from)
    })
}

unsafe extern "C" fn bootstrap_factory(raw: *mut c_void, index: u32) -> Reply {
    transport::reply_boundary(|| {
        let plugin = unsafe { state_ref::<Arc<PluginState>>(raw)? };
        let factory = plugin
            .definition
            .bootstrap_factories
            .get(index as usize)
            .ok_or_else(|| Failure::protocol("unknown paired bootstrap factory"))?;
        wire::encode(&factory.requires_source_bootstrap()).map_err(Failure::from)
    })
}

unsafe fn progress(
    raw: *const BootstrapProgressV1,
    request: &CreateRequest,
) -> anyhow::Result<SourceBootstrapProgress> {
    let raw = unsafe { transport::checked_table(raw)? };
    let scope = request
        .scope
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("bootstrap coordinator requires graph scope"))?;
    anyhow::ensure!(
        raw.reserved == 0,
        "invalid coordinator progress reserved field"
    );
    if !raw.single.is_null() {
        anyhow::ensure!(
            raw.consumers.is_null() && raw.members.is_null() && raw.member_count == 0,
            "single bootstrap owner cannot also have group members"
        );
        let reader = unsafe { crate::NativeSourceProgress::from_borrowed(raw.single)? }.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id && reader.component_id() != &request.id,
            "bootstrap owner belongs to another graph or the source itself"
        );
        return Ok(SourceBootstrapProgress::Single(reader));
    }
    anyhow::ensure!(
        !raw.consumers.is_null()
            && !raw.members.is_null()
            && (1..=MAX_MEMBERS).contains(&(raw.member_count as usize))
            && (raw.members as usize)
                .is_multiple_of(std::mem::align_of::<BootstrapProgressMemberV1>()),
        "invalid bootstrap group member array"
    );
    let consumers = unsafe { crate::NativeConsumerProgress::from_borrowed(raw.consumers)? };
    let view = consumers.reader();
    anyhow::ensure!(
        view.graph_id() == scope.graph_id && view.producer_id() == &request.id,
        "bootstrap group belongs to another graph or source"
    );
    let expected: BTreeSet<_> = consumers.consumer_ids().iter().cloned().collect();
    let raw_members = unsafe { std::slice::from_raw_parts(raw.members, raw.member_count as usize) };
    let mut members = BTreeMap::new();
    for member in raw_members {
        let id = unsafe { crate::bootstrap_coordination::decode_query(member.id)? };
        let reader =
            unsafe { crate::NativeSourceProgress::from_borrowed(&member.progress)? }.reader();
        anyhow::ensure!(
            expected.contains(&id)
                && reader.graph_id() == scope.graph_id
                && reader.component_id() == &id,
            "bootstrap member reader does not match its declared group"
        );
        anyhow::ensure!(
            members.insert(id, reader).is_none(),
            "duplicate bootstrap group member"
        );
    }
    anyhow::ensure!(
        members.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "bootstrap group must include every actual member reader"
    );
    Ok(SourceBootstrapProgress::Shared {
        consumers: view,
        members,
    })
}

unsafe extern "C" fn create(
    raw: *mut c_void,
    input: BorrowedBytes,
    readers: *const BootstrapProgressV1,
    out: *mut BootstrapCoordinatorHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let factory = unsafe { state_ref::<FactoryState>(raw)? };
        let request: CreateRequest = crate::activation_credentials::decode_create_request(unsafe {
            transport::borrowed_bytes(input, abi::bootstrap::MAX_CONTROL_BYTES)?
        })
        .map_err(Failure::from)?;
        let metadata = &factory.plugin.definition.metadata.factories[factory.index];
        let implementation = &factory.plugin.definition.factories[factory.index];
        let construct = || -> anyhow::Result<Arc<Coordinator>> {
            anyhow::ensure!(
                implementation.supports_bootstrap_coordination()
                    && metadata.role == drasi_computation_contracts::ComponentRole::Source
                    && request.implementation == metadata.implementation
                    && request.configuration_version == metadata.configuration_version,
                "source/bootstrap coordinator factory mismatch"
            );
            validate_create_request(factory, &request)?;
            let readers = unsafe { progress(readers, &request)? };
            anyhow::ensure!(
                matches!(&readers, SourceBootstrapProgress::Single(_))
                    == implementation.supports_source_progress()
                    && matches!(&readers, SourceBootstrapProgress::Shared { .. })
                        == implementation.supports_consumer_progress(),
                "source/bootstrap coordinator progress mode mismatch"
            );
            let engine = implementation.create_bootstrap_coordinator(&request, readers.clone())?;
            Ok(Arc::new(Coordinator {
                plugin: factory.plugin.clone(),
                index: factory.index,
                request,
                progress: readers,
                engine,
                lifecycle: Mutex::new(Lifecycle {
                    retiring: false,
                    source: None,
                    providers: BTreeMap::new(),
                }),
                stopping: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                damaged: Arc::new(transport::PanicState::default()),
            }))
        };
        let coordinator = construct().map_err(Failure::from)?;
        unsafe {
            transport::write_out(
                out,
                BootstrapCoordinatorHandle {
                    state: Arc::into_raw(coordinator).cast_mut().cast(),
                    vtable: &VTABLE,
                },
            )
        }
    })
}

pub(super) unsafe fn source_lease(
    raw: *const BootstrapCoordinatorHandle,
    factory: &FactoryState,
    request: &CreateRequest,
) -> anyhow::Result<SourceLease> {
    anyhow::ensure!(
        !raw.is_null()
            && (raw as usize).is_multiple_of(std::mem::align_of::<BootstrapCoordinatorHandle>()),
        "missing or misaligned source/bootstrap coordinator"
    );
    let raw = unsafe { &*raw };
    anyhow::ensure!(
        std::ptr::eq(raw.vtable, &VTABLE) && !raw.state.is_null(),
        "source/bootstrap coordinator belongs to another native producer"
    );
    let owner = unsafe { clone_owner(raw.state)? };
    let mut lifecycle = owner.active()?;
    anyhow::ensure!(
        Arc::ptr_eq(&owner.plugin, &factory.plugin)
            && owner.index == factory.index
            && owner.request.id == request.id
            && owner.request.implementation == request.implementation
            && owner.request.configuration_version == request.configuration_version
            && owner.request.configuration == request.configuration
            && owner.request.activation_credentials_bound == request.activation_credentials_bound
            && owner
                .request
                .scope
                .as_ref()
                .zip(request.scope.as_ref())
                .is_some_and(|(a, b)| {
                    a.instance_id == b.instance_id && a.graph_id == b.graph_id
                }),
        "source does not match its actual retained bootstrap coordinator"
    );
    if let Some(previous) = &lifecycle.source {
        anyhow::ensure!(
            previous.released.load(Ordering::Acquire)
                && !previous.cleanup_required.load(Ordering::Acquire),
            "previous coordinated source remains owned or requires successful cleanup"
        );
    }
    let state = Arc::new(SourceState {
        released: AtomicBool::new(false),
        cleanup_required: AtomicBool::new(false),
    });
    lifecycle.source = Some(state.clone());
    Ok(SourceLease {
        owner: owner.clone(),
        state,
    })
}

unsafe extern "C" fn create_source(
    factory: *mut c_void,
    request: BorrowedBytes,
    coordinator: *const BootstrapCoordinatorHandle,
    progress: *const abi::recovery::SourceProgressV1,
    consumers: *const abi::consumer_progress::ConsumerProgressV1,
    out: *mut abi::ComponentHandle,
) -> Status {
    unsafe {
        create_component_with_services(
            factory,
            request,
            std::ptr::null(),
            progress,
            consumers,
            std::ptr::null(),
            false,
            coordinator,
            out,
        )
    }
}

static VTABLE: BootstrapCoordinatorVTable = BootstrapCoordinatorVTable {
    header: Header::new::<BootstrapCoordinatorVTable>(),
    retain: Some(retain),
    release: Some(release),
    describe: Some(describe),
    open: Some(open),
    stop: Some(stop),
};

unsafe fn clone_owner(raw: *mut c_void) -> Result<Arc<Coordinator>, Failure> {
    crate::bootstrap_coordination::require_context(raw)?;
    unsafe {
        Arc::increment_strong_count(raw.cast::<Coordinator>());
        Ok(Arc::from_raw(raw.cast::<Coordinator>()))
    }
}
unsafe extern "C" fn retain(raw: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(raw.cast::<Coordinator>()) });
}
unsafe extern "C" fn release(raw: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Arc::from_raw(raw.cast::<Coordinator>())) });
}
unsafe extern "C" fn describe(raw: *mut c_void) -> Reply {
    transport::reply_boundary(|| {
        let owner = unsafe { clone_owner(raw)? };
        drop(owner.active()?);
        crate::bootstrap::encode(&owner.request).map_err(Failure::from)
    })
}
unsafe extern "C" fn open(
    raw: *mut c_void,
    query: BorrowedBytes,
    out: *mut abi::bootstrap::BootstrapHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let owner = unsafe { clone_owner(raw)? };
        let query = unsafe { crate::bootstrap_coordination::decode_query(query)? };
        let mut lifecycle = owner.active()?;
        let reader = owner.member(&query)?;
        if let Some(previous) = lifecycle.providers.get(&query) {
            if !previous.stopped.load(Ordering::Acquire) {
                return Err(Failure::protocol(
                    "query already owns an active paired bootstrap provider",
                ));
            }
        }
        let scope = owner.request.scope.as_ref().expect("validated scope");
        let inner = owner.invoke(|| {
            let inner = owner.engine.open(&query, reader.clone())?;
            anyhow::ensure!(
                inner
                    .recovery_reader()
                    .is_some_and(|actual| actual.same_owner(&reader)),
                "paired bootstrap did not retain its actual member reader"
            );
            inner.validate_query_scope(&scope.instance_id, &scope.graph_id, &query)?;
            Ok(inner)
        })?;
        let state = Arc::new(ProviderState {
            stopped: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        });
        if let Some(previous) = lifecycle.providers.insert(query, state.clone()) {
            previous.retired.store(true, Ordering::Release);
        }
        let lease = Arc::new(ProviderLease {
            owner: owner.clone(),
            state,
        });
        let provider = Arc::new(Provider { inner, lease });
        unsafe {
            transport::write_out(
                out,
                crate::bootstrap::export_provider(
                    provider,
                    owner.plugin.codec.clone(),
                    Some(reader),
                ),
            )
        }
    })
}

struct StopGuard(Arc<Coordinator>);
impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.stopping.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn stop(raw: *mut c_void, out: *mut abi::OperationHandle) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let owner = unsafe { clone_owner(raw)? };
        owner
            .stopping
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                Failure::new(
                    abi::status::BUSY,
                    "bootstrap coordinator stop is already active",
                )
            })?;
        let guard = StopGuard(owner.clone());
        owner
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retiring = true;
        let runtime = io_runtime()?;
        let damaged = owner.damaged.clone();
        let operation = transport::export_component_operation(
            async move {
                let _guard = guard;
                if !owner.stopped.load(Ordering::Acquire) {
                    owner.engine.stop().await.map_err(Failure::from)?;
                    owner.stopped.store(true, Ordering::Release);
                }
                Ok(Vec::new())
            },
            Some(runtime),
            damaged,
            true,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

unsafe extern "C" fn create_bootstrap(
    raw: *mut c_void,
    index: u32,
    input: BorrowedBytes,
    source: *const SourceBootstrapV1,
    out: *mut abi::bootstrap::BootstrapHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let plugin = unsafe { state_ref::<Arc<PluginState>>(raw)? };
        let factory = plugin
            .definition
            .bootstrap_factories
            .get(index as usize)
            .ok_or_else(|| Failure::protocol("unknown paired bootstrap factory"))?;
        if !factory.requires_source_bootstrap() {
            return Err(Failure::protocol(
                "bootstrap factory did not declare source pairing",
            ));
        }
        let metadata = &plugin.definition.bootstrap_metadata[index as usize];
        let request: CreateRequest = crate::bootstrap::decode(unsafe {
            transport::borrowed_bytes(input, abi::bootstrap::MAX_CONTROL_BYTES)?
        })
        .map_err(Failure::from)?;
        metadata.validate_request(&request).map_err(Failure::from)?;
        if !metadata.source_progress {
            return Err(Failure::protocol(
                "paired bootstrap requires its actual query progress",
            ));
        }
        let source = unsafe { crate::NativeSourceBootstrap::from_borrowed(source)? };
        let binding = source.metadata();
        if binding.query != request.id
            || binding.source.id == request.id
            || !binding
                .source
                .scope
                .as_ref()
                .zip(request.scope.as_ref())
                .is_some_and(|(a, b)| a.instance_id == b.instance_id && a.graph_id == b.graph_id)
        {
            return Err(Failure::protocol("paired bootstrap scope/query mismatch"));
        }
        let provider = factory
            .create_with_source_bootstrap(&request, source)
            .map_err(Failure::from)?;
        unsafe { transport::write_out(out, provider.into_raw()) }
    })
}
