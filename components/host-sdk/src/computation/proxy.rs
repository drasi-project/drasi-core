// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context as _;
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    transport::{
        checked_table, take_reply, take_status, Failure, OperationCancellation, OperationFuture,
        ReceivedBytes,
    },
    wire::{self, InstanceMetadata, LifecycleRequest},
};
use drasi_lib::computation::v1::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use super::{
    control::{self, ControlBinding},
    loader::PluginOwner,
};
use drasi_computation_plugin_sdk::telemetry_transport;
use std::{
    future::Future,
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
};

enum ReplyPayload {
    Base(ReceivedBytes),
    Contextual(Vec<u8>),
}

pub(super) struct NativeReply {
    payload: ReplyPayload,
    contexts: Option<Vec<ExecutionContext>>,
    timing_unavailable: bool,
}

impl std::fmt::Debug for NativeReply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeReply")
            .field("payload_bytes", &self.len())
            .field("contexts", &self.contexts.as_ref().map(Vec::len))
            .finish()
    }
}

impl NativeReply {
    fn decode(
        bytes: ReceivedBytes,
        contextual: bool,
        timing: Option<&super::timing::HostBridge>,
        timing_gap: bool,
    ) -> std::result::Result<Self, Failure> {
        if contextual {
            let decoded = match timing {
                Some(timing) => {
                    drasi_computation_plugin_sdk::timing_transport::decode_reply(&bytes, timing)
                }
                None => telemetry_transport::decode_reply(&bytes),
            }
            .map_err(|error| Failure::protocol(error.to_string()))?;
            let outputs = decoded
                .outputs
                .into_iter()
                .map(|context| {
                    if (timing_gap || timing.is_some()) && context.handled_branch().is_none() {
                        context.with_handled_branch(
                            drasi_lib::observability::HandledBranchContext::Unavailable(
                                drasi_lib::observability::HandledBranchUnavailable::Unsupported,
                            ),
                        )
                    } else {
                        context
                    }
                })
                .collect();
            Ok(Self {
                payload: ReplyPayload::Contextual(decoded.payload),
                contexts: Some(outputs),
                timing_unavailable: false,
            })
        } else {
            Ok(Self {
                payload: ReplyPayload::Base(bytes),
                contexts: None,
                timing_unavailable: timing_gap,
            })
        }
    }

    fn check_contexts(&self, count: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.contexts
                .as_ref()
                .is_none_or(|contexts| contexts.len() == count),
            "native output context count mismatch"
        );
        Ok(())
    }

    fn context(&self, index: usize, count: usize) -> anyhow::Result<ExecutionContext> {
        self.check_contexts(count)?;
        match &self.contexts {
            Some(contexts) => contexts
                .get(index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("native output context index out of range")),
            None => Ok(if self.timing_unavailable {
                ExecutionContext::default().with_handled_branch(
                    drasi_lib::observability::HandledBranchContext::Unavailable(
                        drasi_lib::observability::HandledBranchUnavailable::Unsupported,
                    ),
                )
            } else {
                ExecutionContext::default()
            }),
        }
    }
}

impl Deref for NativeReply {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.payload {
            ReplyPayload::Base(bytes) => bytes,
            ReplyPayload::Contextual(bytes) => bytes,
        }
    }
}

pub(super) struct NativeOperation {
    operation: OperationFuture,
    contextual: bool,
    timing: Option<Arc<super::timing::HostBridge>>,
    timing_gap: bool,
}

impl Future for NativeOperation {
    type Output = std::result::Result<NativeReply, Failure>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let contextual = self.contextual;
        let timing = self.timing.clone();
        let gap = self.timing_gap;
        Pin::new(&mut self.operation).poll(cx).map(|result| {
            result.and_then(|bytes| NativeReply::decode(bytes, contextual, timing.as_deref(), gap))
        })
    }
}

mod updates;

pub(super) fn native_failure_disposition(error: &Failure) -> FailureDisposition {
    if error.code == abi::status::FAILED && error.retryable {
        FailureDisposition::Retryable
    } else {
        FailureDisposition::Terminal
    }
}

fn operation_failure(error: Failure) -> anyhow::Error {
    ComponentOperationError::new(
        native_failure_disposition(&error),
        native_operation_cause(error),
    )
    .into()
}

/// Lifecycle hooks restore the plugin's lifecycle error kind for the graph's
/// retry policy and callers; the native cause stays in the chain.
fn lifecycle_failure(error: Failure) -> anyhow::Error {
    let kind = match error.code {
        abi::status::UNSUPPORTED => Some(LifecycleErrorKind::Unsupported),
        abi::status::CANCELLED => Some(LifecycleErrorKind::Cancelled),
        _ if native_failure_disposition(&error) == FailureDisposition::Retryable => {
            Some(LifecycleErrorKind::Retryable)
        }
        _ => None,
    };
    let error = operation_failure(error);
    match kind {
        Some(kind) => LifecycleError::new(kind, error).into(),
        None => error,
    }
}

/// The host-side cause of a native failure. A sink's delivery classification
/// is restored as the typed [`DeliveryFailure`] the graph acts on.
pub(super) fn native_operation_cause(error: Failure) -> anyhow::Error {
    if error.code == abi::status::PANICKED {
        ComponentPanic::new(error).into()
    } else if let Some(class) = error.delivery {
        DeliveryFailure::new(class, error).into()
    } else {
        error.into()
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn native_terminal_status_cannot_be_made_retryable_by_its_flag() {
        for (code, retryable, expected) in [
            (abi::status::FAILED, true, FailureDisposition::Retryable),
            (abi::status::FAILED, false, FailureDisposition::Terminal),
            (abi::status::PANICKED, true, FailureDisposition::Terminal),
            (abi::status::PROTOCOL, true, FailureDisposition::Terminal),
            (abi::status::CANCELLED, true, FailureDisposition::Terminal),
            (abi::status::CLOSED, true, FailureDisposition::Terminal),
            (u32::MAX, true, FailureDisposition::Terminal),
        ] {
            let error = operation_failure(Failure {
                code,
                message: "retained native cause".into(),
                retryable,
                data: None,
                delivery: None,
            })
            .context("host operation");
            assert_eq!(
                error
                    .downcast_ref::<ComponentOperationError>()
                    .unwrap()
                    .disposition,
                expected
            );
            let source = error
                .chain()
                .find_map(|source| source.downcast_ref::<Failure>())
                .unwrap();
            assert_eq!(source.code, code);
            assert_eq!(source.message, "retained native cause");
            assert_eq!(
                error.chain().any(|cause| cause.is::<ComponentPanic>()),
                code == abi::status::PANICKED,
                "only native PANICKED marks the generation as damaged"
            );
        }
    }
}

pub(super) struct RemoteComponent {
    raw: abi::ComponentHandle,
    _plugin: Arc<PluginOwner>,
    pub(super) control: Mutex<ControlBinding>,
    stopping: AtomicBool,
    telemetry: Option<abi::telemetry::PluginTelemetryV1>,
    context_enabled: bool,
    timing: std::sync::OnceLock<Option<abi::telemetry::PluginTelemetryV3>>,
    timing_gap_reported: AtomicBool,
    timing_resolver: Mutex<Option<Arc<super::timing::HostBridge>>>,
    has_timing_resolver: AtomicBool,
    retiring: AtomicBool,
    operations: Mutex<Vec<TrackedOperation>>,
}
struct TrackedOperation {
    code: u32,
    cancellation: OperationCancellation,
}
unsafe impl Send for RemoteComponent {}
unsafe impl Sync for RemoteComponent {}
impl RemoteComponent {
    pub(super) fn state(&self) -> *mut std::ffi::c_void {
        self.raw.state
    }
    unsafe fn new(
        raw: abi::ComponentHandle,
        plugin: Arc<PluginOwner>,
        telemetry: Option<abi::telemetry::PluginTelemetryV1>,
        context_enabled: bool,
    ) -> anyhow::Result<Self> {
        let table = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null()
                && table.release.is_some()
                && table.inspect.is_some()
                && table.configuration.is_some()
                && table.bind_control.is_some()
                && table.begin.is_some(),
            "incomplete native component table"
        );
        Ok(Self {
            raw,
            _plugin: plugin,
            control: Mutex::new(ControlBinding::default()),
            stopping: AtomicBool::new(false),
            telemetry,
            context_enabled,
            timing: Default::default(),
            timing_gap_reported: AtomicBool::new(false),
            timing_resolver: Mutex::new(None),
            has_timing_resolver: AtomicBool::new(false),
            retiring: AtomicBool::new(false),
            operations: Mutex::new(Vec::new()),
        })
    }
    fn table(&self) -> &abi::ComponentVTable {
        unsafe { &*self.raw.vtable }
    }
    fn inspect(&self) -> anyhow::Result<InstanceMetadata> {
        let bytes = unsafe {
            take_reply(self.table().inspect.expect("validated")(self.raw.state))
                .map_err(operation_failure)?
        };
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        let bytes = unsafe {
            take_reply(self.table().configuration.expect("validated")(
                self.raw.state,
            ))
            .map_err(operation_failure)?
        };
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub(super) fn bind(&self, control: &abi::Control) -> std::result::Result<(), Failure> {
        unsafe {
            take_status(self.table().bind_control.expect("validated")(
                self.raw.state,
                control,
            ))
        }
    }
    pub(super) fn begin(
        &self,
        code: u32,
        input: &[u8],
        transaction: Option<&abi::Transaction>,
    ) -> anyhow::Result<NativeOperation> {
        self.begin_inputs(code, input, &[], transaction)
    }

    pub(super) fn begin_inputs(
        &self,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
        transaction: Option<&abi::Transaction>,
    ) -> anyhow::Result<NativeOperation> {
        self.ensure_open(code)?;
        let operation = self.begin_contextual(
            code,
            input,
            contexts,
            transaction,
            self.table().begin.expect("validated"),
            self.telemetry.and_then(|extension| extension.begin),
            true,
        )?;
        self.track(code, &operation.operation);
        Ok(operation)
    }

    pub(super) fn begin_consumer(
        &self,
        extension: abi::consumer::PluginConsumerV1,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
        transaction: Option<&abi::Transaction>,
    ) -> anyhow::Result<NativeOperation> {
        if self.retiring.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        self.begin_contextual(
            code,
            input,
            contexts,
            transaction,
            extension.begin.expect("validated"),
            self.telemetry
                .and_then(|extension| extension.begin_consumer),
            false,
        )
    }

    /// Begin an operation through any table of this binary; the operation is
    /// tracked like an ordinary component operation.
    fn begin_with(
        &self,
        code: u32,
        input: &[u8],
        begin: impl FnOnce(
            *mut std::ffi::c_void,
            abi::BorrowedBytes,
            *mut abi::OperationHandle,
        ) -> abi::Status,
    ) -> anyhow::Result<OperationFuture> {
        self.ensure_open(code)?;
        anyhow::ensure!(
            input.len() <= abi::MAX_MESSAGE_BYTES,
            "native operation input exceeds size limit"
        );
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(begin(
                self.raw.state,
                abi::BorrowedBytes::new(input),
                &mut operation,
            ))
            .map_err(operation_failure)?;
            let operation = OperationFuture::new(operation).map_err(operation_failure)?;
            self.track(code, &operation);
            Ok(operation)
        }
    }

    fn ensure_open(&self, code: u32) -> anyhow::Result<()> {
        if self.retiring.load(Ordering::Acquire) && code != abi::operation::DISPOSE
            || code != abi::operation::STOP && self.stopping.load(Ordering::Acquire)
        {
            return Err(operation_failure(Failure::closed()));
        }
        Ok(())
    }

    /// Begin through a base table entry, or its separately negotiated
    /// execution-context telemetry entry when a context must cross the boundary.
    #[allow(clippy::too_many_arguments)]
    fn begin_contextual(
        &self,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
        transaction: Option<&abi::Transaction>,
        base: abi::telemetry::Begin,
        extension: Option<abi::telemetry::Begin>,
        timing_allowed: bool,
    ) -> anyhow::Result<NativeOperation> {
        anyhow::ensure!(
            input.len() <= abi::MAX_MESSAGE_BYTES,
            "native operation input exceeds size limit"
        );
        let current = ExecutionContext::current();
        let has_timing = current.handled_branch().is_some()
            || contexts
                .iter()
                .any(|context| context.handled_branch().is_some());
        let timing_extension = self.timing.get().copied().flatten();
        let fresh_timing = if timing_allowed && has_timing && timing_extension.is_some() {
            super::timing::HostBridge::new(
                std::iter::once(current.clone()).chain(contexts.iter().cloned()),
            )
        } else {
            None
        };
        let timing = if has_timing || self.has_timing_resolver.load(Ordering::Acquire) {
            let mut resolver = self
                .timing_resolver
                .lock()
                .map_err(|_| anyhow::anyhow!("native timing resolver poisoned"))?;
            if let Some(fresh) = fresh_timing {
                *resolver = Some(fresh);
                self.has_timing_resolver.store(true, Ordering::Release);
            }
            if timing_allowed {
                resolver.clone()
            } else {
                None
            }
        } else {
            None
        };
        if let (Some(timing), Some(extension)) = (&timing, timing_extension) {
            let encoded = drasi_computation_plugin_sdk::timing_transport::encode_request(
                &current, contexts, input,
            )?;
            let table = timing.table();
            let mut operation = abi::OperationHandle::null();
            unsafe {
                take_status(extension.begin.expect("validated")(
                    self.raw.state,
                    code,
                    abi::BorrowedBytes::new(&encoded),
                    transaction.map_or(std::ptr::null(), |value| value),
                    &table,
                    &mut operation,
                ))
                .map_err(operation_failure)?;
            }
            return Ok(NativeOperation {
                operation: unsafe { OperationFuture::new(operation) }.map_err(operation_failure)?,
                contextual: true,
                timing: Some(timing.clone()),
                timing_gap: false,
            });
        }
        if has_timing && !self.timing_gap_reported.swap(true, Ordering::AcqRel) {
            log::warn!("Native component lacks a valid timing transport for this operation; propagation coverage is unavailable (terminal host handling remains observable)");
        }
        let current = current.without_handled_branch();
        let stripped = has_timing.then(|| {
            contexts
                .iter()
                .cloned()
                .map(ExecutionContext::without_handled_branch)
                .collect::<Vec<_>>()
        });
        let contexts = stripped.as_deref().unwrap_or(contexts);
        // A source can discover an upstream parent while next() is running,
        // even when its host caller has no context or local tracing adapter.
        let requested = self.context_enabled
            || (code == abi::operation::NEXT && extension.is_some())
            || !current.is_empty()
            || contexts.iter().any(|context| !context.is_empty());
        anyhow::ensure!(
            !requested || extension.is_some(),
            "native execution context transport is unsupported"
        );
        let encoded = requested
            .then(|| telemetry_transport::encode_request(&current, contexts, input))
            .transpose()?;
        let callback = if requested {
            extension.expect("validated")
        } else {
            base
        };
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(callback(
                self.raw.state,
                code,
                abi::BorrowedBytes::new(encoded.as_deref().unwrap_or(input)),
                transaction.map_or(std::ptr::null(), |value| value),
                &mut operation,
            ))
            .map_err(operation_failure)?;
            Ok(NativeOperation {
                operation: OperationFuture::new(operation).map_err(operation_failure)?,
                contextual: requested,
                timing: None,
                timing_gap: has_timing,
            })
        }
    }

    fn track(&self, code: u32, operation: &OperationFuture) {
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        operations.retain(|operation| operation.cancellation.is_active());
        operations.push(TrackedOperation {
            code,
            cancellation: operation.cancellation(),
        });
    }

    fn cancel_operations(&self) {
        self.cancel_operations_except(None);
    }

    fn cancel_operations_except(&self, preserved: Option<u32>) {
        let mut tracked = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let operations = std::mem::take(&mut *tracked);
        let mut cancelled = Vec::new();
        for operation in operations {
            if preserved == Some(operation.code) && operation.cancellation.is_active() {
                tracked.push(operation);
            } else {
                cancelled.push(operation);
            }
        }
        drop(tracked);
        for operation in cancelled {
            let _ = operation.cancellation.cancel();
        }
    }
    pub(super) async fn call(&self, code: u32, input: &[u8]) -> anyhow::Result<NativeReply> {
        self.begin(code, input, None)?
            .await
            .map_err(operation_failure)
    }
    async fn call_inputs(
        &self,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
    ) -> anyhow::Result<NativeReply> {
        self.begin_inputs(code, input, contexts, None)?
            .await
            .map_err(operation_failure)
    }
    /// A unit lifecycle operation whose retryable failure is classified for
    /// the graph's lifecycle policy.
    /// Runs a forward transition (Initialize, Start, Pause, Resume). An
    /// already cancelled scope never reaches native code; cancellation while
    /// in flight drops the operation, which cancels it natively.
    async fn lifecycle(
        &self,
        code: u32,
        request: &LifecycleRequest,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<()> {
        if cancellation.is_cancelled() {
            return Err(LifecycleError::cancelled().into());
        }
        let operation = self.begin(code, &wire::encode(request)?, None)?;
        let reply = tokio::select! {
            biased;
            reply = operation => reply.map_err(lifecycle_failure)?,
            () = cancellation.cancelled() => return Err(LifecycleError::cancelled().into()),
        };
        reply.check_contexts(0)?;
        anyhow::ensure!(
            reply.is_empty(),
            "native unit operation returned unexpected data"
        );
        Ok(())
    }
    pub(super) async fn unit(&self, code: u32, input: &[u8]) -> anyhow::Result<()> {
        let reply = self.call(code, input).await?;
        reply.check_contexts(0)?;
        anyhow::ensure!(
            reply.is_empty(),
            "native unit operation returned unexpected data"
        );
        Ok(())
    }
    async fn unit_inputs(
        &self,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
    ) -> anyhow::Result<()> {
        let reply = self.call_inputs(code, input, contexts).await?;
        reply.check_contexts(0)?;
        anyhow::ensure!(
            reply.is_empty(),
            "native unit operation returned unexpected data"
        );
        Ok(())
    }
    fn revoke_control(&self) {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revoke();
    }
}
impl Drop for RemoteComponent {
    fn drop(&mut self) {
        self.cancel_operations();
        self.revoke_control();
        unsafe { self.table().release.expect("validated")(self.raw.state) };
    }
}

/// A real native graph component. Role-specific calls are checked at both ends;
/// unsupported interfaces return errors rather than falling back to legacy roles.
pub struct NativeComponentProxy {
    pub(super) inner: Arc<RemoteComponent>,
    telemetry_binding: Option<super::telemetry::NativeTelemetryBinding>,
    descriptor: ComponentDescriptor,
    /// The plugin's own declaration for this instance (its factory's pure
    /// describer), forwarded unchanged.
    description: drasi_lib::computation::v1::ComponentDescription,
    metadata: FactoryMetadata,
    creation_request: drasi_computation_plugin_sdk::CreateRequest,
    pub(super) codec: Arc<BinaryEnvelopeCodec>,
    schemas: Vec<Arc<Schema>>,
    pending: bool,
    activated: bool,
    construction_scope: Option<drasi_computation_plugin_sdk::Scope>,
    bindings: SourceBindings,
    input_bindings: Vec<InputBinding>,
    batching: Option<abi::batching::PluginBatchingV1>,
    output_snapshots: Option<super::output_snapshots::OutputSnapshotsBinding>,
    processing: Option<super::processing::ProcessingBinding>,
    bootstrap: Option<Box<super::bootstrap_consumer::BootstrapBinding>>,
    graph_bootstrap: Option<BootstrapPorts>,
    /// An ordinary query's source-input bootstrap, bound at start together
    /// with any bootstrap edges from upstream components.
    query_bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
    bootstrap_relay: Option<Arc<BootstrapRelay>>,
    /// Serves this transformer's committed output on its bootstrap output.
    snapshot_server: Option<Arc<BootstrapSnapshotServer>>,
    /// Whether this activation decided if live delivery waits for the first
    /// bootstrap boundary.
    live_hold_decided: bool,
    /// The source library's bootstrap coordinator, registered for paired
    /// provider nodes while this source exists, and stopped after disposal when
    /// this source created it.
    source_coordinator: Option<SourceCoordinator>,
    graph_control: Option<ComponentControl>,
    bootstrap_replay: std::collections::VecDeque<InputEnvelope>,
    source_schema: Option<Arc<super::source_schema::NativeSourceSchemaReader>>,
    output_reads: Option<super::output_reads::NativeOutputReads>,
    query_descriptions:
        std::collections::BTreeMap<PortId, drasi_computation_contracts::QueryDescription>,
    query_api: Option<Arc<QueryApi>>,
    query_construction: Option<Box<super::query::QueryBinding>>,
    identity: Option<(
        Arc<super::NativeIdentityResource>,
        drasi_computation_plugin_sdk::ActivationCredentialContext,
    )>,
    stop: tokio::sync::Mutex<StopState>,
    disposal: tokio::sync::Mutex<RetirementState>,
    update: tokio::sync::Mutex<Option<updates::UpdateState>>,
    /// Present only when the plugin exports management and this component's
    /// metadata declares it.
    management: Option<abi::management::PluginManagementV1>,
    /// The latest live assessment, whose fingerprint an accepted apply repeats.
    assessment: Mutex<Option<(drasi_computation_plugin_sdk::UpdateRequest, Assessment)>>,
}

pub(super) struct SourceCoordinator {
    pub(super) coordinator: Arc<super::NativeSourceBootstrapResource>,
    pub(super) owned: bool,
    _registration: super::bootstrap_coordination::SourceCoordinatorRegistration,
}

impl SourceCoordinator {
    pub(super) fn new(
        coordinator: Arc<super::NativeSourceBootstrapResource>,
        owned: bool,
        graph: Option<drasi_lib::computation::v1::OutputCatalogue>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            _registration: coordinator.register(graph)?,
            coordinator,
            owned,
        })
    }
}

struct StopState {
    operation: Option<NativeOperation>,
    completed: bool,
    cleanup_complete: bool,
}

impl Default for StopState {
    fn default() -> Self {
        Self {
            operation: None,
            completed: false,
            cleanup_complete: true,
        }
    }
}

#[derive(Default)]
struct RetirementState {
    operation: Option<NativeOperation>,
    completed: bool,
}

#[derive(Default)]
pub(super) struct SourceBindings {
    pub(super) admission: Option<(Arc<SourceAdmission>, super::admission::AdmissionBinding)>,
    pub(super) progress: Option<BoundProgress>,
    pub(super) consumer_progress: Option<BoundConsumerProgress>,
    pub(super) input_progress: Option<Arc<QuerySourceProgress>>,
}

pub(super) struct BoundProgress {
    pub(super) owner: Arc<QuerySourceProgress>,
    pub(super) binding: super::progress::ProgressBinding,
    pub(super) extension: abi::recovery::PluginRecoveryV1,
    pub(super) retention: Option<drasi_core::interface::StorageDurability>,
}

pub(super) struct BoundConsumerProgress {
    pub(super) owner: Arc<ConsumerProgressSet>,
    pub(super) binding: super::consumer_progress::ConsumerProgressBinding,
    pub(super) extension: abi::consumer_progress::PluginConsumerProgressV1,
    pub(super) retention: Option<drasi_core::interface::StorageDurability>,
    pub(super) position_policy: Option<Arc<InputPositionPolicy>>,
}

#[cfg(test)]
mod consumer_position_tests {
    use super::super::{tests::consumer_progress_fixture as fixture, NativePlugin};
    use super::*;
    use drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES;
    use drasi_computation_plugin_sdk as sdk;
    use futures_util::poll;

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_input_policy_is_independent_of_pending_processing_and_component_lifetime(
    ) -> anyhow::Result<()> {
        let linked = NativePlugin::from_definition(sdk::PluginDefinition::new(
            "fixture/consumer-positions",
            "1",
            fixture::factories(),
            vec![GraphChangeCodec::schema()],
        )?)?;
        let dynamic = super::super::factory::recovery_tests::sqlite_plugin()?;
        for plugin in [linked, dynamic] {
            for role in [ComponentRole::Source, ComponentRole::Transformer] {
                let factory = plugin
                    .factories()
                    .iter()
                    .find(|factory| {
                        factory.metadata().role == role
                            && factory
                                .metadata()
                                .implementation
                                .name
                                .starts_with("fixture/consumer-progress-")
                    })
                    .unwrap();
                let id = ComponentId::try_new("cb2d99c8-7382-51f0-b4a1-a4391101b067")?;
                let owner = Arc::new(QuerySourceProgress::new(
                    "graph",
                    ComponentId::try_new("c3ae027b-02d9-5e17-9b44-95861a98f62e")?,
                )?);
                let progress = Arc::new(ConsumerProgressSet::new("graph", id.clone(), [owner])?);
                let mut proxy = factory.create_component_with_consumer_progress(
                    id,
                    serde_json::json!({"mode": "positions"}),
                    sdk::Scope {
                        graph_id: "graph".into(),
                        instance_id: "graph".into(),
                        generation: 1,
                    },
                    progress.clone(),
                )?;
                proxy
                    .initialize(&crate::computation::test_hooks::initialize_ctx())
                    .await?;
                let policy = proxy
                    .bindings
                    .consumer_progress
                    .as_ref()
                    .unwrap()
                    .position_policy
                    .clone()
                    .unwrap();
                assert_eq!(
                    policy.key(),
                    &SourceProgressKey::Source(
                        drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d"
                        )
                        .expect("fixture UUID")
                    )
                );
                assert_eq!(
                    proxy.recovery_contract(),
                    ComponentRecovery::default().with_consumer_positions(progress, policy.clone())
                );
                proxy
                    .start(&crate::computation::test_hooks::start_ctx())
                    .await?;
                let input = InputEnvelope {
                    execution_context: Default::default(),
                    port: PortId::try_new("in")?,
                    envelope: GraphChangeCodec::encode_change(
                        drasi_core::models::SourceChange::Delete {
                            metadata: drasi_core::models::ElementMetadata {
                                reference: drasi_core::models::ElementReference::new(
                                    "source", "one",
                                ),
                                labels: Arc::from([Arc::from("Item")]),
                                effective_from: 1,
                            },
                        },
                        StreamId::try_new("source/out")?,
                        1,
                        None,
                    )?,
                };
                let mut pending = Box::pin(async {
                    if role == ComponentRole::Source {
                        proxy.next().await.map(|_| ())
                    } else {
                        proxy.transform(input).await.map(|_| ())
                    }
                });
                assert!(poll!(pending.as_mut()).is_pending());
                assert!(policy.is_after(&[0, 7], &[6, 90])?);
                assert!(!policy.is_after(&[1, 89], &[6, 90])?);
                assert!(!policy.is_after(&[1, 90], &[6, 90])?);
                assert!(policy.is_after(&[2, 1], &[6, 90]).is_err());
                assert!(policy.is_after(&[0, 1], &vec![0; MAX_SOURCE_POSITION_BYTES])?);
                assert!(policy
                    .is_after(&vec![0; MAX_SOURCE_POSITION_BYTES + 1], &[0])
                    .is_err());
                assert!(poll!(pending.as_mut()).is_pending());
                drop(pending);
                proxy
                    .stop(&crate::computation::test_hooks::stop_ctx())
                    .await?;
                drop(proxy);
                // This is an immutable retained comparator, not a live reader;
                // the graph owns installation/revocation of its input policy.
                assert!(policy.is_after(&[0, 7], &[6, 90])?);
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_comparison_panic_quarantines_pending_native_processing(
    ) -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(sdk::PluginDefinition::new(
            "fixture/position-panic",
            "1",
            fixture::factories(),
            vec![GraphChangeCodec::schema()],
        )?)?;
        let id = ComponentId::try_new("cb2d99c8-7382-51f0-b4a1-a4391101b067")?;
        let progress = Arc::new(ConsumerProgressSet::new(
            "graph",
            id.clone(),
            [Arc::new(QuerySourceProgress::new(
                "graph",
                ComponentId::try_new("c3ae027b-02d9-5e17-9b44-95861a98f62e")?,
            )?)],
        )?);
        let mut proxy = plugin.factories()[0].create_component_with_consumer_progress(
            id,
            serde_json::json!({"mode":"positions"}),
            sdk::Scope {
                graph_id: "graph".into(),
                instance_id: "graph".into(),
                generation: 1,
            },
            progress,
        )?;
        proxy
            .initialize(&crate::computation::test_hooks::initialize_ctx())
            .await?;
        let policy = proxy
            .bindings
            .consumer_progress
            .as_ref()
            .unwrap()
            .position_policy
            .clone()
            .unwrap();
        proxy
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        let mut pending = Box::pin(proxy.next());
        assert!(poll!(pending.as_mut()).is_pending());
        let error = policy.is_after(&[255], &[0]).unwrap_err();
        assert!(error.downcast_ref::<ComponentPanic>().is_some());
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await?
            .unwrap_err();
        assert!(error.chain().any(|cause| cause.is::<ComponentPanic>()));
        assert!(policy
            .is_after(&[0, 1], &[0])
            .unwrap_err()
            .downcast_ref::<ComponentPanic>()
            .is_some());
        proxy
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
        Ok(())
    }
}

impl NativeComponentProxy {
    pub(super) fn with_timing_extension(
        self,
        extension: Option<abi::telemetry::PluginTelemetryV3>,
    ) -> Self {
        self.inner
            .timing
            .set(extension)
            .expect("timing extension is configured once");
        self
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn new(
        raw: abi::ComponentHandle,
        metadata: FactoryMetadata,
        request: &drasi_computation_plugin_sdk::CreateRequest,
        codec: Arc<BinaryEnvelopeCodec>,
        schemas: Vec<Arc<Schema>>,
        plugin: Arc<PluginOwner>,
        bindings: SourceBindings,
        telemetry: Option<abi::telemetry::PluginTelemetryV1>,
        context_enabled: bool,
    ) -> anyhow::Result<Self> {
        let inner =
            Arc::new(unsafe { RemoteComponent::new(raw, plugin, telemetry, context_enabled)? });
        let mut bindings = bindings;
        let actual = inner.inspect()?;
        let descriptor = metadata.descriptor(request.id.clone())?;
        anyhow::ensure!(
            (actual.descriptor == descriptor
                || actual.descriptor == metadata.data_descriptor(request.id.clone())?)
                && actual.capabilities == metadata.capabilities,
            "constructed native component changed its descriptor/capabilities"
        );
        anyhow::ensure!(
            actual
                .queries
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
                == metadata.capabilities.output_queries.iter().collect(),
            "constructed native component changed its query read declarations"
        );
        metadata.configuration.validate(&inner.configuration()?)?;
        if let Some(progress) = &mut bindings.progress {
            let bytes = unsafe {
                take_reply(progress.extension.inspect.expect("validated")(
                    inner.raw.state,
                ))?
            };
            let recovery: drasi_computation_plugin_sdk::progress::InstanceRecovery =
                drasi_computation_plugin_sdk::progress::decode(&bytes)?;
            anyhow::ensure!(
                recovery.version == abi::recovery::VERSION,
                "unsupported native source recovery declaration"
            );
            progress.retention = recovery.retention;
        }
        if let Some(progress) = &mut bindings.consumer_progress {
            let bytes = unsafe {
                take_reply(progress.extension.inspect.expect("validated")(
                    inner.raw.state,
                ))?
            };
            let recovery: drasi_computation_plugin_sdk::consumer_progress::InstanceProgress =
                drasi_computation_plugin_sdk::consumer_progress::decode(&bytes)?;
            anyhow::ensure!(
                recovery.version == abi::consumer_progress::VERSION,
                "unsupported native consumer progress declaration"
            );
            progress.retention = recovery.retention;
            let mut raw = abi::consumer_progress::InputPositionPolicyV1::null();
            unsafe {
                take_status(progress.extension.position_policy.expect("validated")(
                    inner.raw.state,
                    &mut raw,
                ))?;
            }
            progress.position_policy = unsafe { super::input_position::from_borrowed(raw)? };
            anyhow::ensure!(
                progress.position_policy.is_some() == recovery.position_policy,
                "native producer changed its declared input position policy"
            );
        }
        Ok(Self {
            inner,
            telemetry_binding: None,
            descriptor,
            description: drasi_lib::computation::v1::ComponentDescription::undeclared(),
            metadata,
            creation_request: request.clone(),
            codec,
            schemas,
            pending: false,
            activated: false,
            construction_scope: request.scope.clone(),
            bindings,
            batching: None,
            output_snapshots: None,
            processing: None,
            bootstrap: None,
            graph_bootstrap: None,
            query_bootstrap: None,
            bootstrap_relay: None,
            snapshot_server: None,
            live_hold_decided: false,
            source_coordinator: None,
            graph_control: None,
            bootstrap_replay: std::collections::VecDeque::new(),
            source_schema: None,
            output_reads: None,
            query_descriptions: actual.queries,
            query_api: None,
            query_construction: None,
            input_bindings: Vec::new(),
            identity: None,
            stop: tokio::sync::Mutex::new(StopState::default()),
            disposal: tokio::sync::Mutex::new(RetirementState::default()),
            update: tokio::sync::Mutex::new(None),
            management: None,
            assessment: Mutex::new(None),
        })
    }
    pub(super) fn with_telemetry_binding(
        mut self,
        binding: Option<super::telemetry::NativeTelemetryBinding>,
    ) -> Self {
        self.telemetry_binding = binding;
        self
    }

    pub(super) fn with_bootstrap(
        mut self,
        resource: Arc<super::NativeBootstrapResource>,
    ) -> anyhow::Result<Self> {
        self.bind_bootstrap_provider(resource)?;
        Ok(self)
    }

    /// Bind an existing provider once, before activation. A progress-bound
    /// provider must retain this proxy's exact local processing-progress owner.
    pub fn bind_bootstrap_provider(
        &mut self,
        resource: Arc<super::NativeBootstrapResource>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && self.bootstrap.is_none(),
            "bootstrap provider must bind once before activation"
        );
        let scope = self.construction_scope.as_ref().ok_or_else(|| {
            anyhow::anyhow!("bootstrap requires the actual component construction scope")
        })?;
        let input = self
            .metadata
            .capabilities
            .quiescent_bootstrap
            .as_ref()
            .or(self.metadata.capabilities.durable_bootstrap.as_ref())
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("native transformer does not declare provider bootstrap")
            })?;
        self.bootstrap = Some(Box::new(super::bootstrap_consumer::BootstrapBinding::new(
            resource,
            input,
            scope,
            self.descriptor.id(),
            self.processing.as_ref(),
        )?));
        Ok(())
    }

    pub(super) fn with_source_schema(
        mut self,
        extension: Option<abi::source_schema::PluginSourceSchemaV1>,
    ) -> anyhow::Result<Self> {
        self.source_schema = extension
            .map(|extension| {
                super::source_schema::NativeSourceSchemaReader::bind(
                    &self.inner,
                    extension,
                    self.metadata.role,
                )
            })
            .transpose()?
            .flatten();
        Ok(self)
    }
    /// Open the declared providers. The extension is required exactly when
    /// factory metadata declares output reads.
    pub(super) fn with_output_reads(
        mut self,
        extension: Option<abi::output_reads::PluginOutputReadsV2>,
        follow: Option<abi::output_follow::PluginOutputFollowV2>,
    ) -> anyhow::Result<Self> {
        self.output_reads = super::output_reads::NativeOutputReads::bind(
            extension,
            follow,
            &self.inner,
            &self.descriptor,
            &self.metadata,
            &self.query_descriptions,
            &self.schemas,
            &self.codec,
        )?;
        if let Some(reads) = &self.output_reads {
            let readers = reads.readers();
            let queries: Vec<_> = readers
                .ports()
                .values()
                .filter_map(|port| port.query().cloned())
                .collect();
            anyhow::ensure!(
                queries.len() <= 1,
                "query API requires one declared query output"
            );
            if let Some(reader) = queries.into_iter().next() {
                self.query_api = Some(QueryApi::from_native_reader(
                    self.descriptor.id().clone(),
                    reader,
                    self.processing_progress_owner(),
                )?);
            }
        }
        Ok(self)
    }
    pub(super) fn with_identity(
        mut self,
        identity: Option<(
            Arc<super::NativeIdentityResource>,
            drasi_computation_plugin_sdk::ActivationCredentialContext,
        )>,
    ) -> Self {
        self.identity = identity;
        self
    }
    pub(super) fn with_output_snapshots(
        mut self,
        snapshots: Option<super::output_snapshots::OutputSnapshotsBinding>,
    ) -> Self {
        self.output_snapshots = snapshots;
        self
    }
    pub(super) fn with_source_coordinator(
        mut self,
        coordinator: Option<SourceCoordinator>,
    ) -> Self {
        self.source_coordinator = coordinator;
        self
    }
    pub(super) fn with_processing(
        mut self,
        processing: Option<super::processing::ProcessingBinding>,
    ) -> Self {
        self.processing = processing;
        self
    }
    /// Bind the plugin's management table exactly when this component's
    /// metadata declares instance management.
    pub(super) fn set_description(
        &mut self,
        description: drasi_lib::computation::v1::ComponentDescription,
    ) {
        self.description = description;
    }
    /// Attach the plugin's declaration for this instance.
    pub(super) fn with_description(
        mut self,
        description: drasi_lib::computation::v1::ComponentDescription,
    ) -> Self {
        self.description = description;
        self
    }
    pub(super) fn with_management(
        mut self,
        extension: Option<abi::management::PluginManagementV1>,
    ) -> anyhow::Result<Self> {
        if !self.metadata.capabilities.management {
            return Ok(self);
        }
        let extension = extension
            .context("native component declares management without the negotiated extension")?;
        extension
            .header
            .validate::<abi::management::PluginManagementV1>()?;
        anyhow::ensure!(
            extension.version == abi::management::VERSION
                && extension.reserved == 0
                && extension.assess.is_some()
                && extension.apply.is_some(),
            "invalid native management extension"
        );
        self.management = Some(extension);
        Ok(self)
    }
    /// The consumers whose progress this source's live position belongs to.
    fn progress_owners(&self) -> Vec<Arc<QuerySourceProgress>> {
        match (&self.bindings.progress, &self.bindings.consumer_progress) {
            (Some(progress), _) => vec![progress.owner.clone()],
            (None, Some(progress)) => progress.owner.owners().values().cloned().collect(),
            (None, None) => Vec::new(),
        }
    }

    /// Whether this is the source's very first start: no consumer has a saved
    /// live position for it or a completed bootstrap. `None` while a consumer
    /// has not recovered its progress, unless `wait` waits for it.
    async fn fresh_start(&self, wait: bool) -> anyhow::Result<Option<bool>> {
        let owners = self.progress_owners();
        if owners.is_empty() {
            return Ok(None);
        }
        let key = SourceProgressKey::Source(self.descriptor.id().clone());
        let mut fresh = true;
        for owner in owners {
            let snapshot = if wait {
                owner.wait_recovered().await?
            } else {
                let snapshot = owner.snapshot();
                if !snapshot.recovered {
                    return Ok(None);
                }
                snapshot
            };
            fresh &= !snapshot.bootstrap_complete && !snapshot.checkpoints.contains_key(&key);
        }
        Ok(Some(fresh))
    }

    /// The lifecycle payload for one hook, bound to this object's identity.
    fn lifecycle_request(&self, scope: &OperationScope) -> LifecycleRequest {
        LifecycleRequest::new(
            scope.operation_id(),
            self.descriptor.id().as_str(),
            scope.generation(),
        )
        .with_deadline(scope.remaining())
    }
    pub(super) fn with_batching(
        mut self,
        extension: abi::batching::PluginBatchingV1,
    ) -> anyhow::Result<Self> {
        extension
            .header
            .validate::<abi::batching::PluginBatchingV1>()?;
        anyhow::ensure!(
            extension.version == abi::batching::VERSION
                && extension.reserved == 0
                && extension.factory.is_some()
                && extension.policy.is_some()
                && self.metadata.role == ComponentRole::Sink,
            "invalid native sink batching extension"
        );
        self.batching = Some(extension);
        self.batching()?;
        Ok(self)
    }
    pub fn metadata(&self) -> &FactoryMetadata {
        &self.metadata
    }

    /// Bind an existing query-result catalogue before graph registration and activation.
    /// An actual connected `QueryResultsOutlet` supplies live delivery and backpressure.
    pub fn bind_query_catalog(
        &mut self,
        catalog: QueryResultsCatalog,
        mode: drasi_lib::DispatchMode,
        capacity: usize,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && !self.pending,
            "bind the query result catalogue before activation"
        );
        let api = self
            .query_api
            .as_ref()
            .context("native component does not expose a query read capability")?;
        self.query_api = Some(api.with_native_result_catalog(catalog, mode, capacity)?);
        Ok(())
    }

    pub(super) fn bind_query_construction(
        &mut self,
        construction: QueryConstruction,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated && !self.pending && self.query_construction.is_none(),
            "ordinary query resources must bind once before activation"
        );
        self.query_bootstrap = construction.bootstrap.clone();
        if let Some(clock) = &construction.scheduling {
            self.processing
                .as_ref()
                .context("query clock requires indexed processing")?
                .bind_clock(clock)?;
        }
        let api = self
            .query_api
            .as_ref()
            .context("native query has no query reader")?;
        if let Some(counter) = &construction.transport_sequence {
            self.processing
                .as_ref()
                .context("query transport requires indexed processing")?
                .bind_transport(counter.clone())?;
        }
        if let Some(config) = &construction.configuration {
            self.query_api = Some(api.with_native_configuration(
                config.clone(),
                construction.catalog.clone(),
                construction.default_dispatch_capacity,
            )?);
        } else if let Some(catalog) = &construction.catalog {
            self.query_api = Some(api.with_native_result_catalog(
                catalog.clone(),
                drasi_lib::DispatchMode::Channel,
                construction.default_dispatch_capacity,
            )?);
        }
        self.query_construction = Some(Box::new(super::query::QueryBinding {
            construction,
            session: None,
            observer: None,
            draining: false,
        }));
        Ok(())
    }

    async fn finish_query_drain(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        if !outputs.is_empty() || self.pending || !self.bootstrap_replay.is_empty() {
            return Ok(());
        }
        let Some(binding) = self
            .query_construction
            .as_mut()
            .filter(|binding| binding.draining)
        else {
            return Ok(());
        };
        let (sequence, generation, persistent) = self
            .query_api
            .as_ref()
            .context("query drain has no result reader")?
            .output_frontier()
            .await?;
        binding
            .session
            .as_mut()
            .context("query drain has no control session")?
            .complete(
                sequence,
                generation,
                persistent,
                binding.construction.options.publication,
            );
        binding.draining = false;
        Ok(())
    }

    /// Read the actual indexed owner's committed input progress. This data-only
    /// view supplies no local source-retirement proof. Stop revokes old views.
    pub fn committed_input_progress(
        &self,
    ) -> anyhow::Result<Option<drasi_computation_contracts::SourceProgressView>> {
        Ok(self
            .processing
            .as_ref()
            .map(|processing| processing.committed_input_progress())
            .transpose()?
            .flatten())
    }

    /// State used by a bootstrap provider in this component's actual processing
    /// transaction domain. The handle is inactive until its owner is opened.
    pub fn bootstrap_state(&self) -> Option<Arc<dyn drasi_computation_contracts::BootstrapState>> {
        self.processing
            .as_ref()
            .map(|processing| processing.bootstrap_state())
    }

    /// The local progress resource published by this exact processing owner.
    /// Sources/coordinators must bind this handle, not a same-named replacement.
    pub fn processing_progress_owner(&self) -> Option<Arc<QuerySourceProgress>> {
        self.processing
            .as_ref()
            .and_then(|processing| processing.progress_owner())
    }

    /// Acquire actual index resources before an asserted graph captures this
    /// component's recovery contract. Native processing remains inactive until start.
    pub async fn prepare_processing(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.activated,
            "processing resources must prepare before activation"
        );
        let Some(processing) = &mut self.processing else {
            return Ok(());
        };
        let result = processing.prepare().await;
        if let Err(error) = result {
            return match processing.close().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(drasi_lib::error::OperationFailures::new(
                    "native processing preparation and cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
        }
        Ok(())
    }

    fn begin_processing(&self) -> anyhow::Result<Option<super::processing::ProcessingCall>> {
        if let Some(processing) = &self.processing {
            processing.output_ready()?;
            processing.progress_call()
        } else {
            Ok(None)
        }
    }

    async fn outputs(
        &mut self,
        code: u32,
        input: &[u8],
        contexts: &[ExecutionContext],
    ) -> anyhow::Result<Vec<OutputEnvelope>> {
        let _query_timer = self
            .query_api
            .as_ref()
            .map(|api| api.native_operation_timer());
        let progress_call = self
            .processing
            .as_ref()
            .map(|processing| processing.progress_call())
            .transpose()?
            .flatten();
        let reply = self.inner.call_inputs(code, input, contexts).await?;
        let result: wire::OutputBatch<'_> = wire::decode(&reply)?;
        anyhow::ensure!(
            result.query_observation.is_none() || self.metadata.capabilities.query_observations,
            "native component returned undeclared query observations"
        );
        let observation = result
            .query_observation
            .map(|value| value.into_observation())
            .transpose()?;
        anyhow::ensure!(
            !result.pending || self.metadata.capabilities.continuations,
            "native component returned undeclared continuation work"
        );
        reply.check_contexts(result.outputs.len())?;
        let total = result.outputs.len();
        let outputs = result
            .outputs
            .into_iter()
            .enumerate()
            .map(|(index, output)| {
                Ok(output
                    .into_output(&self.codec)?
                    .with_execution_context(reply.context(index, total)?))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        for output in &outputs {
            wire::check_port(
                &self.descriptor,
                &output.port,
                &output.envelope,
                PortDirection::Output,
            )?;
        }
        self.pending = result.pending;
        if let Some(api) = &self.query_api {
            api.observe_native_progress(&outputs, observation.as_ref())?;
            if let Some(reads) = self.output_reads.as_ref().filter(|_| observation.is_some()) {
                reads.published();
            }
        } else {
            anyhow::ensure!(
                observation.is_none(),
                "query observation has no query reader"
            );
        }
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    fn input(&self, input: &InputEnvelope) -> anyhow::Result<Vec<u8>> {
        wire::check_port(
            &self.descriptor,
            &input.port,
            &input.envelope,
            PortDirection::Input,
        )?;
        wire::encode(&wire::Envelope::input(input, &self.codec)?)
    }
    pub(super) fn into_transactional(self) -> anyhow::Result<NativeTransactionalProxy> {
        anyhow::ensure!(
            self.bindings.input_progress.is_none(),
            "graph-owned volatile input progress cannot be used inside a transaction"
        );
        anyhow::ensure!(
            self.metadata.capabilities.transactional,
            "native component is not a transaction participant"
        );
        anyhow::ensure!(
            !self.metadata.capabilities.input_binding_updates,
            "native transaction steps cannot receive graph input-binding updates"
        );
        let schema = |direction| -> anyhow::Result<Arc<Schema>> {
            let port = self
                .descriptor
                .ports()
                .iter()
                .find(|port| port.direction() == direction)
                .ok_or_else(|| anyhow::anyhow!("missing native transaction port"))?;
            self.schemas
                .iter()
                .find(|schema| schema.descriptor() == port.schema())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing native transaction schema"))
        };
        let input = schema(PortDirection::Input)?;
        let output = schema(PortDirection::Output)?;
        Ok(NativeTransactionalProxy {
            component: self,
            input,
            output,
        })
    }
}
/// A native source's control handler: bootstrap requests are relayed from
/// the provider on the source's bootstrap input; other messages reach the
/// plugin's own handler, if it has one.
/// Routes bootstrap requests to the component's relay or snapshot server and
/// every other control message to the native component.
struct BootstrapControl {
    bootstrap: Arc<dyn ControlHandler>,
    native: Option<Arc<dyn ControlHandler>>,
}

#[async_trait]
impl ControlHandler for BootstrapControl {
    async fn on_message(
        &self,
        message: PeerMessage,
        control: ComponentControl,
    ) -> anyhow::Result<()> {
        if matches!(
            message.notification,
            ControlNotification::BootstrapRequest(_)
        ) {
            return self.bootstrap.on_message(message, control).await;
        }
        match &self.native {
            Some(native) => native.on_message(message, control).await,
            None => Ok(()),
        }
    }
}

impl NativeComponentProxy {
    /// Bind graph bootstrap: a transformer with connected bootstrap inputs
    /// reads its snapshot from them. Without connected inputs it keeps a
    /// directly bound provider, or none.
    fn bind_graph_bootstrap(&mut self) -> anyhow::Result<()> {
        if self.bootstrap.is_some() {
            return Ok(());
        }
        let sources = self.query_bootstrap.take();
        let edges = match (&self.graph_bootstrap, &self.graph_control) {
            (Some(ports), Some(control)) if !ports.inputs().is_empty() => {
                let scope = self
                    .construction_scope
                    .as_ref()
                    .context("graph bootstrap requires the actual component construction scope")?;
                let provider: Arc<dyn ComputationBootstrapProvider> = PipeBootstrapProvider::new(
                    scope.graph_id.clone(),
                    ports.clone(),
                    control.clone(),
                    self.processing
                        .as_ref()
                        .and_then(super::processing::ProcessingBinding::progress_owner),
                )?;
                Some(provider)
            }
            _ => None,
        };
        // A query reading both sources and upstream components bootstraps
        // from each, as the ordinary pipeline does.
        let provider: Arc<dyn ComputationBootstrapProvider> = match (sources, edges) {
            (Some(sources), Some(edges)) => {
                let scope = self
                    .construction_scope
                    .as_ref()
                    .context("graph bootstrap requires the actual component construction scope")?;
                Arc::new(CompositeBootstrapProvider::new(
                    scope.instance_id.clone(),
                    scope.graph_id.clone(),
                    self.descriptor.id().clone(),
                    vec![
                        (ResourceId::try_new("source-inputs")?, sources),
                        (ResourceId::try_new("component-inputs")?, edges),
                    ],
                )?)
            }
            (Some(provider), None) | (None, Some(provider)) => provider,
            (None, None) => return Ok(()),
        };
        self.bind_bootstrap_provider(Arc::new(super::NativeBootstrapResource(provider)))
    }
}

impl Drop for NativeComponentProxy {
    fn drop(&mut self) {
        if let Some(schema) = self.source_schema.take() {
            schema.retire();
        }
        if let Some(reads) = self.output_reads.take() {
            reads.retire();
        }
        self.output_snapshots.take();
        self.bindings.admission.take();
        self.bindings.progress.take();
        self.bindings.consumer_progress.take();
        self.inner.revoke_control();
    }
}

#[async_trait]
impl ComputationComponent for NativeComponentProxy {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        self.description.clone()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_query_test_support(
        &mut self,
        inspector: &ComputationInspector,
        generation: ComponentGeneration,
    ) {
        if let Some(binding) = &mut self.query_construction {
            binding.observer = binding
                .construction
                .scheduling
                .as_ref()
                .and_then(|scheduling| {
                    scheduling.observe_query(inspector, self.descriptor.id().clone(), generation)
                });
        }
    }
    fn query_api(&self) -> Option<Arc<QueryApi>> {
        self.query_api.clone()
    }
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        self.source_schema
            .as_ref()
            .map(|reader| reader.clone() as Arc<dyn SourceSchemaReader>)
    }
    fn output_readers(&self) -> OutputReaders {
        self.output_reads
            .as_ref()
            .map_or_else(OutputReaders::default, |reads| reads.readers())
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        if let Some(processing) = &self.processing {
            if processing.progress_owner().is_some() {
                return processing.recovery_contract();
            }
        }
        let recovery = if let Some(progress) = &self.bindings.consumer_progress {
            let recovery = progress
                .retention
                .map_or_else(ComponentRecovery::default, ComponentRecovery::admitted)
                .with_consumer_progress(progress.owner.clone());
            match &progress.position_policy {
                Some(policy) => {
                    recovery.with_consumer_positions(progress.owner.clone(), policy.clone())
                }
                None => recovery,
            }
        } else {
            match &self.bindings.progress {
                Some(progress) => {
                    progress
                        .retention
                        .map_or_else(ComponentRecovery::default, |retention| {
                            ComponentRecovery::admitted(retention)
                                .replay_until(progress.owner.component_id().clone())
                        })
                }
                None => ComponentRecovery::default(),
            }
        };
        match &self.bindings.input_progress {
            Some(progress) => recovery.with_volatile_input_progress(progress.clone()),
            None => recovery,
        }
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        let value = self.inner.configuration()?;
        self.metadata.configuration.validate(&value)?;
        Ok(value)
    }
    #[allow(
        clippy::misnamed_getters,
        reason = "factory construction bindings and component updates are distinct capabilities"
    )]
    fn binds_inputs(&self) -> bool {
        self.metadata.capabilities.input_binding_updates
    }
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata.capabilities.input_binding_updates,
            "native component does not support input-binding updates"
        );
        self.inner
            .unit(abi::operation::BIND_INPUTS, &wire::encode(&bindings)?)
            .await?;
        self.input_bindings = bindings.to_vec();
        Ok(())
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.graph_control = Some(control.clone());
        if self.metadata.capabilities.control {
            control::bind(&self.inner, control);
        }
    }
    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        match self.metadata.role {
            ComponentRole::Source => {
                // The provider's watermarks are the native source's own
                // positions, so they are the handover boundary unchanged.
                // One relay serves every activation, so the published control
                // handler stays current. Requests wait until `start` begins
                // an activation with the bound provider input.
                let relay = self.bootstrap_relay.get_or_insert_with(|| {
                    BootstrapRelay::new(Arc::new(|_, watermarks| Ok(watermarks.to_vec())))
                });
                relay.end_activation();
                relay.bind(ports);
            }
            ComponentRole::Transformer => {
                if let Some(snapshot) = self
                    .metadata
                    .capabilities
                    .output_snapshots
                    .first()
                    .and_then(|port| {
                        self.output_readers()
                            .ports()
                            .get(port)
                            .and_then(|readers| readers.snapshot().cloned())
                    })
                {
                    let server = BootstrapSnapshotServer::new(snapshot);
                    server.bind(ports.clone());
                    self.snapshot_server = Some(server);
                }
                if self.metadata.capabilities.provider_bootstrap().is_some() {
                    self.graph_bootstrap = Some(ports);
                }
            }
            _ => {}
        }
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        let native = self
            .metadata
            .capabilities
            .control
            .then(|| Arc::new(control::Handler(self.inner.clone())) as Arc<dyn ControlHandler>);
        let bootstrap = match (&self.bootstrap_relay, &self.snapshot_server) {
            (Some(relay), _) => relay.clone() as Arc<dyn ControlHandler>,
            (None, Some(server)) => server.clone() as Arc<dyn ControlHandler>,
            (None, None) => return native,
        };
        Some(Arc::new(BootstrapControl { bootstrap, native }))
    }
    fn requires_readiness_confirmation(&self) -> bool {
        self.metadata.capabilities.readiness
    }
    async fn initialize(&mut self, context: &InitializeContext) -> anyhow::Result<()> {
        let request = self.lifecycle_request(context.scope());
        self.inner
            .lifecycle(
                abi::operation::INITIALIZE,
                &request,
                context.scope().cancellation(),
            )
            .await
    }
    async fn pause(&mut self, context: &PauseContext) -> anyhow::Result<()> {
        let request = self.lifecycle_request(context.scope());
        self.inner
            .lifecycle(
                abi::operation::PAUSE,
                &request,
                context.scope().cancellation(),
            )
            .await
    }
    async fn resume(&mut self, context: &ResumeContext) -> anyhow::Result<()> {
        let request = self.lifecycle_request(context.scope());
        self.inner
            .lifecycle(
                abi::operation::RESUME,
                &request,
                context.scope().cancellation(),
            )
            .await
    }
    fn management(&self) -> Option<&dyn ComponentManagement> {
        self.management
            .is_some()
            .then_some(self as &dyn ComponentManagement)
    }
    fn management_mut(&mut self) -> Option<&mut dyn ComponentManagement> {
        if self.management.is_some() {
            Some(self)
        } else {
            None
        }
    }
    async fn start(&mut self, context: &StartContext) -> anyhow::Result<()> {
        if context.scope().cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled().into());
        }
        anyhow::ensure!(
            self.update
                .get_mut()
                .as_ref()
                .is_none_or(updates::UpdateState::permits_activation),
            "native component cannot start with an unfinished or damaging update"
        );
        if self.inner.stopping.load(Ordering::Acquire)
            || self.inner.retiring.load(Ordering::Acquire)
        {
            return Err(operation_failure(Failure::closed()));
        }
        self.bind_graph_bootstrap()?;
        anyhow::ensure!(
            self.metadata.capabilities.quiescent_bootstrap.is_none()
                || self.metadata.capabilities.bootstrap_optional
                || self.bootstrap.is_some(),
            "native bootstrap consumer requires a bound provider"
        );
        anyhow::ensure!(
            !self
                .bootstrap
                .as_ref()
                .is_some_and(|binding| binding.needs_cleanup()),
            "native bootstrap requires successful cleanup before restarting"
        );
        if self.metadata.capabilities.durable_bootstrap.is_some()
            || self.metadata.capabilities.bootstrap_optional
        {
            let status: drasi_computation_contracts::BootstrapStatus = wire::decode(
                &self
                    .inner
                    .call(abi::operation::BOOTSTRAP_STATUS, &[])
                    .await?,
            )?;
            anyhow::ensure!(
                status.configured == self.bootstrap.is_some(),
                "bootstrap configuration requires its actual provider before initialization"
            );
        }
        self.activated = true;
        *self.stop.get_mut() = StopState::default();
        self.stop.get_mut().cleanup_complete = false;
        if let Some(reads) = &self.output_reads {
            reads.advance();
        }
        if let Some(schema) = &self.source_schema {
            schema.advance();
        }
        {
            let binding = self
                .inner
                .control
                .lock()
                .map_err(|_| anyhow::anyhow!("native control binding poisoned"))?;
            if let Some(error) = &binding.error {
                return Err(operation_failure(error.clone()));
            }
        }
        // Caller-owned resolution is cancelled with this activation. Nothing is
        // cached on the proxy or sent to native code until resolution succeeds.
        let mut request = self.lifecycle_request(context.scope());
        if let Some(relay) = &self.bootstrap_relay {
            relay.begin_activation();
            self.live_hold_decided = false;
            request = request.with_fresh_start(self.fresh_start(false).await?);
        }
        if let Some((identity, endpoint)) = &self.identity {
            request = request.with_credentials(identity.resolve(endpoint).await?);
        }
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        let activation = self
            .bindings
            .consumer_progress
            .as_ref()
            .map(|progress| progress.binding.activate())
            .transpose()?;
        if let Some(binding) = &self.output_snapshots {
            binding.activate()?;
        }
        let progress_call = self
            .processing
            .as_ref()
            .map(|processing| processing.progress_call())
            .transpose()?
            .flatten();
        let result = async {
            if let Some(binding) = &self.query_construction {
                let construction = &binding.construction;
                validate_query_sources(
                    !construction.provider.is_volatile(),
                    &construction.definition.id,
                    &construction.source_hosts,
                )?;
            }
            if let Some(processing) = &mut self.processing {
                processing.open().await?;
            }
            self.inner
                .lifecycle(
                    abi::operation::START,
                    &request,
                    context.scope().cancellation(),
                )
                .await?;
            if let Some(bootstrap) = &mut self.bootstrap {
                let bindings = self
                    .metadata
                    .capabilities
                    .input_binding_updates
                    .then_some(self.input_bindings.as_slice());
                self.bootstrap_replay =
                    Box::pin(bootstrap.initialize(&self.inner, &self.codec, bindings))
                        .await?
                        .into();
            } else if self.metadata.capabilities.durable_bootstrap.is_some()
                || self.metadata.capabilities.bootstrap_optional
            {
                let status: drasi_computation_contracts::BootstrapStatus = wire::decode(
                    &self
                        .inner
                        .call(abi::operation::BOOTSTRAP_STATUS, &[])
                        .await?,
                )?;
                anyhow::ensure!(
                    !status.configured && status.complete,
                    "durable bootstrap requires a bound provider"
                );
                let completion = if self.metadata.capabilities.durable_bootstrap.is_some() {
                    wire::encode(&drasi_computation_plugin_sdk::bootstrap::Completed::from(
                        drasi_computation_contracts::BootstrapCompletion::default(),
                    ))?
                } else {
                    Vec::new()
                };
                self.inner
                    .unit(abi::operation::BOOTSTRAP_COMPLETE, &completion)
                    .await?;
            }
            if let Some(processing) = &self.processing {
                processing.progress_ready()?;
                if let Some(binding) = &mut self.query_construction {
                    if let Some(scheduling) = &binding.construction.scheduling {
                        processing.bind_scheduling(scheduling)?;
                        binding.session = scheduling.connect_test_control()?;
                    }
                }
            }
            if let Some(api) = &self.query_api {
                api.initialize_native_progress().await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if result.is_err()
            && (self.bootstrap.is_some()
                || self
                    .metadata
                    .capabilities
                    .committed_input_progress
                    .is_some())
        {
            let error = result.expect_err("native initialization failed");
            let scope = context.scope();
            let cleanup = StopContext::new(
                OperationScope::new(
                    format!("{}/abort", scope.operation_id()),
                    scope.component_id(),
                    scope.generation(),
                    scope.deadline(),
                    scope.cancellation().clone(),
                ),
                StopMode::Abort,
            );
            return match self.stop(&cleanup).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(drasi_lib::error::OperationFailures::new(
                    "native startup and owned initialization cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
        }
        if result.is_err() {
            if let Some(binding) = &self.output_snapshots {
                binding.deactivate();
            }
            if let Some(processing) = &mut self.processing {
                if let Err(cleanup) = processing.close().await {
                    return Err(drasi_lib::error::OperationFailures::new(
                        "native startup and indexed processing cleanup failed",
                        vec![result.expect_err("startup failed"), cleanup],
                    )
                    .into());
                }
            }
        } else if let Some(activation) = activation {
            activation.complete();
        }
        if result.is_ok() {
            self.pending = self.metadata.capabilities.startup_continuation;
            if let Some(reads) = &self.output_reads {
                reads.recovery_ready();
            }
            if let Some(call) = progress_call {
                call.complete();
            }
        }
        result
    }
    async fn stop(&mut self, context: &StopContext) -> anyhow::Result<()> {
        if self.inner.retiring.load(Ordering::Acquire) {
            return Err(operation_failure(Failure::closed()));
        }
        let cleanup_call = self
            .processing
            .as_ref()
            .map(|processing| processing.cleanup_call())
            .transpose()?
            .flatten();
        self.inner
            .cancel_operations_except(Some(abi::operation::STOP));
        if let Some(relay) = &self.bootstrap_relay {
            relay.end_activation();
        }
        if let Some(reads) = &self.output_reads {
            reads.advance();
        }
        if let Some(schema) = &self.source_schema {
            schema.advance();
        }
        if let Some(binding) = &mut self.query_construction {
            binding.session.take();
            binding.draining = false;
            if let Some(scheduling) = &binding.construction.scheduling {
                scheduling.suspend();
            }
        }
        self.inner.stopping.store(true, Ordering::Release);
        if let Some(processing) = &self.processing {
            processing.suspend_progress()?;
        }
        if let Some(bootstrap) = &mut self.bootstrap {
            bootstrap.stop().await?;
        }
        if !self.stop.get_mut().completed {
            if self.stop.get_mut().operation.is_none() {
                let request = self
                    .lifecycle_request(context.scope())
                    .with_stop_mode(context.mode());
                self.stop.get_mut().operation = Some(self.inner.begin(
                    abi::operation::STOP,
                    &wire::encode(&request)?,
                    None,
                )?);
            }
            // Retain the accepted STOP and its readers across caller cancellation
            // or timeout. A retry polls this same operation.
            let result = self
                .stop
                .get_mut()
                .operation
                .as_mut()
                .expect("owned stop operation")
                .await;
            self.stop.get_mut().operation.take();
            anyhow::ensure!(
                result.map_err(lifecycle_failure)?.is_empty(),
                "native unit operation returned unexpected data"
            );
            self.stop.get_mut().completed = true;
        }
        if let Some(progress) = &self.bindings.consumer_progress {
            progress.binding.deactivate();
        }
        if let Some(binding) = &self.output_snapshots {
            binding.deactivate();
        }
        if let Some(binding) = &self.output_snapshots {
            binding.ensure_quiesced()?;
        }
        if let Some(progress) = &self.bindings.consumer_progress {
            progress.binding.stopped()?;
        }
        self.inner
            .timing_resolver
            .lock()
            .map_err(|_| anyhow::anyhow!("native timing resolver poisoned"))?
            .take();
        self.inner
            .has_timing_resolver
            .store(false, Ordering::Release);
        if let Some(processing) = &mut self.processing {
            processing.close().await?;
        }
        self.stop.get_mut().cleanup_complete = true;
        self.bootstrap_replay.clear();
        if let Some(reads) = &self.output_reads {
            reads.recovery_ready();
        }
        self.inner.stopping.store(false, Ordering::Release);
        if let Some(call) = cleanup_call {
            call.complete();
        }
        Ok(())
    }
    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        let erase = context.erases_owned_state();
        anyhow::ensure!(
            !erase || self.metadata.capabilities.erases_owned_state,
            "native component does not support permanent state removal"
        );
        anyhow::ensure!(
            self.stop.get_mut().cleanup_complete && !self.inner.stopping.load(Ordering::Acquire),
            "native disposal requires successfully stopped work"
        );
        self.inner
            .cancel_operations_except(Some(abi::operation::DISPOSE));
        self.inner.retiring.store(true, Ordering::Release);
        if let Some(schema) = &self.source_schema {
            schema.retire();
        }
        if let Some(reads) = &self.output_reads {
            reads.retire();
        }
        let mut request = self.lifecycle_request(context.scope()).with_dispose(
            context.reason(),
            context.intent(),
            context.attempt(),
        );
        if !erase {
            if let Some(processing) = &mut self.processing {
                processing.close().await?;
            }
            return self.run_dispose(&request).await;
        }
        // Erasure may have to reach the external system without any start in
        // this process, so a bound factory receives freshly resolved
        // activation credentials. A retry that polls the accepted operation
        // resolves nothing.
        if let Some((identity, endpoint)) = &self.identity {
            let state = self.disposal.get_mut();
            if !state.completed && state.operation.is_none() {
                request = request.with_credentials(identity.resolve(endpoint).await?);
            }
        }
        // Erasure needs the owned processing state open while native code
        // removes it; it is closed again whatever the outcome.
        self.activated = true;
        let result = async {
            let state = self.disposal.get_mut();
            if !state.completed && state.operation.is_none() {
                if let Some(processing) = &mut self.processing {
                    if processing.needs_cleanup()? {
                        processing.close().await?;
                    }
                    processing.open().await?;
                }
            }
            self.run_dispose(&request).await
        }
        .await;
        let cleanup = match &mut self.processing {
            Some(processing) => processing.close().await,
            None => Ok(()),
        };
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
                "native state erasure and processing cleanup failed",
                vec![error, cleanup],
            )
            .into()),
        }
    }
}

impl NativeComponentProxy {
    /// Retain an accepted DISPOSE across caller cancellation; a retry polls it.
    async fn run_dispose(&mut self, request: &LifecycleRequest) -> anyhow::Result<()> {
        let state = self.disposal.get_mut();
        if state.completed {
            return Ok(());
        }
        if state.operation.is_none() {
            state.operation = Some(self.inner.begin(
                abi::operation::DISPOSE,
                &wire::encode(request)?,
                None,
            )?);
        }
        let result = state
            .operation
            .as_mut()
            .expect("owned dispose operation")
            .await;
        state.operation.take();
        anyhow::ensure!(
            result.map_err(lifecycle_failure)?.is_empty(),
            "native disposal returned unexpected data"
        );
        state.completed = true;
        if let Some(source) = self
            .source_coordinator
            .as_ref()
            .filter(|source| source.owned)
        {
            // Each bootstrap session reports its own failure to its consumer; a
            // session cut short by the stop must not fail the source's disposal.
            if let Err(error) =
                drasi_lib::computation::v1::ResourceCleanup::shutdown(source.coordinator.as_ref())
                    .await
            {
                log::warn!(
                    "Source {} bootstrap coordinator stopped with a failed session: {error:#}",
                    self.descriptor.id()
                );
            }
        }
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for NativeComponentProxy {
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.bindings
            .progress
            .as_ref()
            .map(|progress| progress.owner.clone())
    }
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        self.bindings
            .admission
            .as_ref()
            .map(|(admission, _)| admission.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(relay) = self.bootstrap_relay.clone() {
            if !self.live_hold_decided {
                if relay.relays_sessions() && self.fresh_start(true).await? == Some(true) {
                    relay.hold_live();
                }
                self.live_hold_decided = true;
            }
            relay.live_released().await;
        }
        let reply = self.inner.call(abi::operation::NEXT, &[]).await?;
        let output: Option<wire::Envelope<'_>> = wire::decode(&reply)?;
        reply.check_contexts(usize::from(output.is_some()))?;
        let output = output
            .map(|output| {
                Ok::<_, anyhow::Error>(
                    output
                        .into_output(&self.codec)?
                        .with_execution_context(reply.context(0, 1)?),
                )
            })
            .transpose()?;
        if let Some(output) = &output {
            wire::check_port(
                &self.descriptor,
                &output.port,
                &output.envelope,
                PortDirection::Output,
            )?;
        }
        Ok(output)
    }
}
#[async_trait]
impl Transformer for NativeComponentProxy {
    async fn bind_output_destinations(
        &mut self,
        bindings: &drasi_lib::computation::v1::OutputBindings,
    ) -> anyhow::Result<()> {
        match &mut self.processing {
            Some(processing) => processing.bind_outputs(bindings).await?,
            None => {
                anyhow::ensure!(
                    bindings.destinations().is_empty(),
                    "native transformer does not track durable output destinations"
                );
            }
        }
        if self.metadata.capabilities.durable_output {
            let call = self
                .processing
                .as_ref()
                .expect("durable processing")
                .progress_call()?;
            self.inner
                .unit(abi::operation::DELIVERY_RESUME, &[])
                .await?;
            self.pending = true;
            if let Some(call) = call {
                call.complete();
            }
        }
        Ok(())
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let progress_call = self.begin_processing()?;
        let scope = drasi_lib::observability::ExecutionScope::for_input(&input.execution_context);
        let bytes = self.input(&input)?;
        let outputs = scope
            .scope(self.outputs(
                abi::operation::TRANSFORM,
                &bytes,
                &[input.execution_context],
            ))
            .await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        let progress_call = self.begin_processing()?;
        let contexts = outputs
            .iter()
            .map(|output| output.execution_context.clone())
            .collect::<Vec<_>>();
        let outputs = outputs
            .iter()
            .map(|output| wire::Envelope::output(output, &self.codec))
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.inner
            .unit_inputs(
                abi::operation::DELIVERY_COMPLETED,
                &wire::encode(&outputs)?,
                &contexts,
            )
            .await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(())
    }
    fn has_pending_emissions(&self) -> bool {
        self.pending
            || !self.bootstrap_replay.is_empty()
            || self
                .query_construction
                .as_ref()
                .is_some_and(|binding| binding.draining)
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending {
            if let Some(input) = self.bootstrap_replay.front().cloned() {
                let outputs = self.transform(input).await?;
                self.bootstrap_replay.pop_front();
                return Ok(outputs);
            }
        }
        let progress_call = self.begin_processing()?;
        let outputs = self.outputs(abi::operation::CONTINUE, &[], &[]).await?;
        self.finish_query_drain(&outputs).await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        if let Some(binding) = &self.query_construction {
            if binding.construction.scheduling.is_some() {
                return binding
                    .session
                    .as_ref()
                    .and_then(QuerySchedulingSession::wakeup_source);
            }
        }
        self.metadata
            .capabilities
            .wakeups
            .then(|| Arc::new(NativeWakeup(self.inner.clone())) as Arc<dyn WakeupSource>)
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.pending && self.bootstrap_replay.is_empty() {
            if let Some(binding) = &mut self.query_construction {
                if let Some(session) = &mut binding.session {
                    if !session.begin() {
                        return Ok(Vec::new());
                    }
                    binding.draining = true;
                }
            }
        }
        let progress_call = self.begin_processing()?;
        let outputs = self.outputs(abi::operation::ON_WAKEUP, &[], &[]).await?;
        self.finish_query_drain(&outputs).await?;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(outputs)
    }
}
#[async_trait]
impl EnvelopeSink for NativeComponentProxy {
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        let Some(extension) = &self.batching else {
            return Ok(None);
        };
        let bytes = unsafe {
            take_reply(extension.policy.expect("validated")(self.inner.state()))
                .map_err(operation_failure)?
        };
        let mut policy: Option<SinkBatching> = wire::decode(&bytes)?;
        if let Some(policy) = &mut policy {
            policy.validate()?;
            if self.inner.context_enabled || !ExecutionContext::current().is_empty() {
                policy.max_bytes = policy
                    .max_bytes
                    .min(abi::MAX_MESSAGE_BYTES - telemetry_transport::CONTEXT_FRAME_OVERHEAD);
            }
        }
        Ok(policy)
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        let contexts = inputs
            .iter()
            .map(|input| input.execution_context.clone())
            .collect::<Vec<_>>();
        let policy = self
            .batching()?
            .ok_or_else(|| anyhow::anyhow!("native batching is disabled"))?;
        policy.validate_inputs(&inputs)?;
        let batch = inputs
            .iter()
            .map(|input| {
                wire::check_port(
                    &self.descriptor,
                    &input.port,
                    &input.envelope,
                    PortDirection::Input,
                )?;
                wire::Envelope::input(input, &self.codec)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.inner
            .unit_inputs(
                abi::operation::HANDLE_BATCH,
                &wire::encode(&batch)?,
                &contexts,
            )
            .await
            .map_err(delivery_cause)
    }
    /// Exposed only for the input ports the factory metadata lists, which the
    /// plugin validated against its actual handlers at construction.
    fn snapshot_consumer(&mut self, port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        if self.metadata.capabilities.snapshot_inputs.contains(port) {
            Some(self)
        } else {
            None
        }
    }
    fn completion(&self) -> SinkCompletion {
        self.metadata.completion.expect("native sink role")
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let scope = drasi_lib::observability::ExecutionScope::for_input(&input.execution_context);
        scope
            .scope(self.inner.unit_inputs(
                abi::operation::HANDLE,
                &self.input(&input)?,
                &[input.execution_context],
            ))
            .await
            .map_err(delivery_cause)
    }
}

/// A native delivery that failed because the activation is closed or broke
/// the protocol cannot succeed by delivering the same input again, so it is
/// marked terminal for the activation; the graph fails the node instead of
/// retrying it.
fn delivery_cause(error: anyhow::Error) -> anyhow::Error {
    let closed = error.chain().any(|cause| {
        cause.downcast_ref::<Failure>().is_some_and(|failure| {
            matches!(failure.code, abi::status::CLOSED | abi::status::PROTOCOL)
        })
    });
    if closed {
        LifecycleError::terminal(error).into()
    } else {
        error
    }
}
#[async_trait]
impl SnapshotConsumer for NativeComponentProxy {
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.metadata
                .capabilities
                .snapshot_inputs
                .contains(&input.port),
            "native sink input {} does not support snapshot replacement",
            input.port
        );
        self.inner
            .unit_inputs(
                abi::operation::SNAPSHOT,
                &self.input(&input)?,
                &[input.execution_context],
            )
            .await
    }
}
#[async_trait]
impl ComputationService for NativeComponentProxy {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.inner.unit(abi::operation::RUN, &[]).await
    }
}
struct NativeWakeup(Arc<RemoteComponent>);
#[async_trait]
impl WakeupSource for NativeWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        self.0.unit(abi::operation::WAKE_WAIT, &[]).await
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        wire::decode(&self.0.call(abi::operation::WAKE_PENDING, &[]).await?)
    }
}

pub(super) struct NativeTransactionalProxy {
    component: NativeComponentProxy,
    input: Arc<Schema>,
    output: Arc<Schema>,
}
#[async_trait]
impl ComputationComponent for NativeTransactionalProxy {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        self.component.description()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        self.component.descriptor()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        self.component.configuration()
    }
    fn output_readers(&self) -> OutputReaders {
        self.component.output_readers()
    }
    async fn initialize(&mut self, context: &InitializeContext) -> anyhow::Result<()> {
        self.component.initialize(context).await
    }
    async fn start(&mut self, context: &StartContext) -> anyhow::Result<()> {
        self.component.start(context).await
    }
    async fn stop(&mut self, context: &StopContext) -> anyhow::Result<()> {
        self.component.stop(context).await
    }
    async fn pause(&mut self, context: &PauseContext) -> anyhow::Result<()> {
        self.component.pause(context).await
    }
    async fn resume(&mut self, context: &ResumeContext) -> anyhow::Result<()> {
        self.component.resume(context).await
    }
    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        self.component.dispose(context).await
    }
    fn management(&self) -> Option<&dyn ComponentManagement> {
        self.component.management()
    }
    fn management_mut(&mut self) -> Option<&mut dyn ComponentManagement> {
        self.component.management_mut()
    }
}
#[async_trait]
impl Transformer for NativeTransactionalProxy {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.component.transform(input).await
    }
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.component.delivery_completed(outputs).await
    }
}
#[async_trait]
impl TransactionalTransformer for NativeTransactionalProxy {
    fn transaction_input_schema(&self) -> Arc<Schema> {
        self.input.clone()
    }
    fn transaction_output_schema(&self) -> Arc<Schema> {
        self.output.clone()
    }
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope> {
        anyhow::ensure!(
            context.step_id() == self.component.descriptor.id(),
            "native transaction step identity mismatch"
        );
        anyhow::ensure!(
            input.changes().schema() == self.input.descriptor(),
            "native transaction input schema mismatch"
        );
        let output = super::transaction::run(&self.component, input, context).await?;
        anyhow::ensure!(
            output.changes().schema() == self.output.descriptor(),
            "native transaction output schema mismatch"
        );
        Ok(output)
    }
}
