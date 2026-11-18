// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Plugin-side operations supported by the native ABI. Host-only query APIs,
//! storage owners, graph control handlers and construction contexts are absent.

use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ComponentDescriptor, ComponentId, ConsumerProgressView,
    InputBinding, InputEnvelope, InputPositionPolicy, OutputEnvelope, OutputReaders, PortId,
    SinkBatching, SinkCompletion, SnapshotConsumer, SourceProgressView, SourceSchemaReader,
    WakeupSource,
};
use drasi_core_types::interface::StorageDurability;
use std::sync::Arc;

use crate::{
    lifecycle::{
        ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
        PauseContext, ResumeContext, StartContext, StopContext,
    },
    ApplyUpdateRequest, UpdateRequest,
};

#[async_trait]
pub trait NativeComponent: Send + Sync {
    fn descriptor(&self) -> &ComponentDescriptor;
    /// The component's current literal configuration. Required: the Host
    /// validates it against the factory schema at construction and uses it for
    /// update planning and reconstruction.
    fn configuration(&self) -> anyhow::Result<serde_json::Value>;
    /// Instance management for Pause and Stop boundary updates. Presence must
    /// match `Capabilities::management`; it is fixed for the object's lifetime.
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        None
    }
    /// The input-binding update handler. Present exactly when the factory
    /// declares `Capabilities::input_binding_updates`; fixed for the object's
    /// lifetime.
    fn input_bindings(&mut self) -> Option<&mut dyn NativeInputBindings> {
        None
    }
    /// Snapshot and/or replay providers for this component's declared output
    /// ports. Obtaining them performs no I/O and does not take the component's
    /// processing slot; each provider must read its own state independently of
    /// pending `next`/`transform` work and return a typed error when it cannot
    /// answer. Registration is fixed at construction: factory metadata lists
    /// exactly the output ports with a snapshot (`output_snapshots`) and replay
    /// (`output_replays`) provider, and activation cannot add or remove them.
    /// Retained updates must return clones of the same provider Arcs, not
    /// replacement readers. A query provider's description may change after an
    /// approved update; the exporter reflects it only after successful validation.
    /// Reads are served only while the component is started and not paused.
    fn output_readers(&self) -> OutputReaders {
        OutputReaders::default()
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        None
    }
    /// Actual upstream retention, not a guarantee inferred from consumer states.
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        None
    }
    /// Stable source-defined cursor semantics, independent of mutable processing.
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        None
    }
    /// Set up instance-lifetime resources once after construction has bound
    /// context/services, before the first start. Do not start normal processing
    /// or wait for the graph to become live. Failure/cancellation retires this
    /// object: only Dispose(InitializationFailure, Preserve) follows.
    async fn initialize(&mut self, ctx: &InitializeContext) -> LifecycleResult;
    /// Start an activation on an initialized object. Credentials, when the
    /// factory declares them, are in the context; never persist them. A partial
    /// failure or cancellation still requires Stop before retry or Dispose.
    /// Readiness, when declared, is signalled separately with `Ready`.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult;
    /// Pause processing without ending activation: no new output, workers
    /// gated, obligations accounted for. Return `unsupported` rather than
    /// succeeding when independent work cannot actually be settled.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult;
    /// Resume a completed pause, without another Start or Initialize.
    async fn resume(&mut self, ctx: &ResumeContext) -> LifecycleResult;
    /// Settle activation-owned work, including a partial Start, and retain
    /// resources needed for a later Start. `Drain` completes in-flight work;
    /// `Abort` cancels promptly. Incomplete cleanup must be retry-safe;
    /// cancellation is not completion. This does not end the object's lifetime.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult;
    /// Retire the object, including after partial initialization. Called only
    /// after activation cleanup has completed. `StateIntent::Erase` permits
    /// permanently erasing state and external resources this component owns;
    /// never borrowed or shared state. A failed attempt is retried with the
    /// same reason and intent and must be retry-safe. Success is final.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult;
}

/// Receives the graph's current incoming bindings (declared by
/// `Capabilities::input_binding_updates`).
#[async_trait]
pub trait NativeInputBindings: Send + Sync {
    /// Called before start and before processing with the current declared
    /// incoming edges. Calls are serialized with processing and may repeat the
    /// same bindings; implementations must tolerate repetition and cancellation.
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()>;
}

/// Provider-driven initialization of a transformer input. Declared by
/// `Capabilities::quiescent_bootstrap` (the handler is
/// [`BootstrapInput::Quiescent`]) or `Capabilities::durable_bootstrap`
/// ([`BootstrapInput::Durable`]); the input port comes from that declaration.
#[async_trait]
pub trait NativeBootstrapInput: Send + Sync {
    /// Whether this instance's configuration requests initialization and
    /// whether it is complete. A durable input also says whether a reset is
    /// allowed.
    fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus>;
    async fn bootstrap(&mut self, input: InputEnvelope) -> anyhow::Result<()>;
    async fn bootstrap_completed(&mut self, completion: BootstrapCompletion) -> anyhow::Result<()>;
}

/// Durable initialization: atomic completion with the processing owner's
/// state, and an explicit reset when the provider can no longer resume.
#[async_trait]
pub trait NativeDurableBootstrapInput: NativeBootstrapInput {
    /// Discard partial initialization state; called only while initializing
    /// and only when `bootstrap_status` allowed a reset.
    async fn reset_bootstrap(&mut self) -> anyhow::Result<()>;
}

/// The bootstrap handler a transformer exposes, matching its declaration.
pub enum BootstrapInput<'a> {
    Quiescent(&'a mut dyn NativeBootstrapInput),
    Durable(&'a mut dyn NativeDurableBootstrapInput),
}

impl BootstrapInput<'_> {
    pub fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus> {
        match self {
            Self::Quiescent(input) => input.bootstrap_status(),
            Self::Durable(input) => input.bootstrap_status(),
        }
    }
    pub async fn bootstrap(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        match self {
            Self::Quiescent(handler) => handler.bootstrap(input).await,
            Self::Durable(handler) => handler.bootstrap(input).await,
        }
    }
    pub async fn bootstrap_completed(
        &mut self,
        completion: BootstrapCompletion,
    ) -> anyhow::Result<()> {
        match self {
            Self::Quiescent(handler) => handler.bootstrap_completed(completion).await,
            Self::Durable(handler) => handler.bootstrap_completed(completion).await,
        }
    }
}

/// Delivery tracking for a transformer declaring `Capabilities::durable_output`:
/// retained output trims only through these hooks, so both are required.
#[async_trait]
pub trait NativeDurableOutput: Send + Sync {
    /// The Host delivered `outputs` to every destination; trim them.
    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()>;
    /// Resume delivering retained, unacknowledged output after the Host bound
    /// its output destinations.
    fn resume_delivery(&mut self) -> anyhow::Result<()>;
}

/// Updates a retained object at a Pause or Stop boundary. Replace is never an
/// instance operation: the host disposes the old object and constructs anew.
#[async_trait]
pub trait NativeManagement: Send + Sync {
    /// Side-effect-free assessment of a proposed change using live facts. A
    /// failure is an error, never an implicit Replace.
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment>;
    /// Apply an accepted change at the established boundary. Expose the
    /// proposed configuration from `configuration()` on success; omitted
    /// fields may expose the factory's published defaults only. `Refused` and
    /// `Failed` must leave the object unchanged and may be retried; `Damaged`
    /// retires it. An interrupted or cancelled attempt is never retried on the
    /// same object: only Stop and Dispose remain admissible.
    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError>;
}

#[async_trait]
pub trait NativeSource: NativeComponent {
    /// An independent reader of the live source's data schema. Obtaining the
    /// reader performs no I/O and must not acquire the source processing lock.
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        None
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        None
    }
    /// Retention at the actual source boundary, paired with its negotiated reader.
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        None
    }
    /// None means permanently exhausted, not temporarily idle.
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>>;
}

#[async_trait]
pub trait NativeTransformer: NativeComponent {
    /// The provider-driven initialization handler. Present exactly when the
    /// factory declares quiescent or durable bootstrap, as the matching variant.
    fn bootstrap_input(&mut self) -> Option<BootstrapInput<'_>> {
        None
    }
    /// The durable delivery handler. Present exactly when the factory
    /// declares `Capabilities::durable_output`.
    fn durable_output(&mut self) -> Option<&mut dyn NativeDurableOutput> {
        None
    }
    /// Delivery notification for a transformer without durable output, which
    /// retains nothing; a durable transformer receives it through
    /// [`NativeDurableOutput::delivery_completed`] instead.
    async fn delivery_completed(&mut self, _outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        Ok(())
    }
    fn has_pending_emissions(&self) -> bool {
        false
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::bail!("this transformer has no continuation")
    }
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        None
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::bail!("this transformer does not implement scheduled processing")
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>>;
}

#[async_trait]
pub trait NativeSink: NativeComponent {
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        Ok(None)
    }
    async fn handle_batch(&mut self, _inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        anyhow::bail!("this sink does not implement host-owned batching")
    }
    /// The handler that replaces this sink's state from a snapshot on `port`, if
    /// that input registered one. Obtaining it performs no I/O and its presence
    /// is fixed at construction: factory metadata lists exactly the input ports
    /// that return a handler, and activation cannot add or remove support.
    fn snapshot_consumer(&mut self, _port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        None
    }
    fn completion(&self) -> SinkCompletion;
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()>;
}

#[async_trait]
pub trait NativeService: NativeComponent {
    /// Perform service processing. This is not a lifecycle alias; temporary
    /// quiescence uses NativeComponent::pause/resume.
    async fn run(&mut self) -> anyhow::Result<()>;
}
