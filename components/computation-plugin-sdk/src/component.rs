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

#[async_trait]
pub trait NativeComponent: Send + Sync {
    fn descriptor(&self) -> &ComponentDescriptor;
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!(
            "component {} does not expose its configuration",
            self.descriptor().id()
        )
    }
    fn requires_readiness_confirmation(&self) -> bool {
        false
    }
    /// Whether this component implements explicit, permanent deletion of owned
    /// state/resources. Must match `Capabilities::delete_owned_resources`.
    fn supports_delete_owned_resources(&self) -> bool {
        false
    }
    /// Whether the retained-instance update hook is implemented. Must agree with
    /// the factory declaring an InPlace or Restart update mode. Recreate-only
    /// factories do not need this hook. This is not a data-loss authorization.
    fn supports_apply_update(&self) -> bool {
        false
    }
    /// Whether this component consumes graph input-binding updates. This must
    /// agree with the factory's `input_binding_updates` capability.
    fn supports_input_binding_updates(&self) -> bool {
        false
    }
    /// Called before start and before processing with the current declared
    /// incoming edges. Calls are serialized with processing and may repeat the
    /// same bindings; implementations must tolerate repetition and cancellation.
    async fn bind_inputs(&mut self, _bindings: &[InputBinding]) -> anyhow::Result<()> {
        anyhow::bail!("component does not support input-binding updates")
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
    /// object: dispose it, and construct a fresh object to retry initialization.
    async fn initialize(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Start an activation on an initialized object. A partial failure or
    /// cancellation still requires stop before retry or final disposal.
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Called instead of `start` by the normal owned START operation when an
    /// identity resource was bound at construction. Never update configuration
    /// with these ephemeral values or fall back to literal authentication.
    async fn start_with_credentials(
        &mut self,
        _credentials: crate::ActivationCredentials,
    ) -> anyhow::Result<()> {
        anyhow::bail!("component does not implement activation credentials")
    }
    /// Settle activation-owned work, including a partial start, and retain
    /// resources needed for a later start. Incomplete cleanup must be retry-safe;
    /// cancellation is not completion. This does not end the object's lifetime.
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Release instance-lifetime resources at final retirement, including after
    /// partial initialization. Called only after activation cleanup has completed.
    /// Incomplete disposal must be retry-safe. Successful disposal is final and
    /// does not delete persistent state or component-owned external artifacts.
    async fn dispose(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Temporarily pause processing without ending activation. Components with
    /// independently running work must actually settle that work before claiming
    /// an update plan that relies on this boundary. No-op defaults prove nothing
    /// about data safety, draining or persistence.
    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Resume a completed temporary pause, without another start/initialize.
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    /// Execute an approved retained-instance update at its required boundary.
    /// InPlace requires completed pause; Restart requires completed stop.
    /// Recreate is never an instance operation. Expose the proposed configuration
    /// from `configuration()` on success; omitted fields may expose the factory's
    /// published literal defaults, but never arbitrary extra/different values.
    /// Comparison does not rewrite the accepted request. An incomplete attempt may be retried
    /// with the identical request/plan; it is not rolled back by cancellation.
    /// Core, not this declaration, validates actual data and downstream safety.
    /// Success refreshes inspected query descriptions from the same registered
    /// readers. Hosts must re-inspect rather than retaining creation-time metadata.
    async fn apply_update(&mut self, _update: &crate::ApplyUpdateRequest) -> anyhow::Result<()> {
        Err(crate::transport::Failure::new(
            crate::abi::status::UNSUPPORTED,
            "component does not implement retained-instance updates",
        )
        .into())
    }
    /// Permanently delete owned persistent state and external resources, only
    /// when explicitly requested after positive activation cleanup. Ordinary
    /// stop, disposal and updates must not invoke this hook implicitly. A failed
    /// or cancelled deletion may be partial and must be retry-safe. Borrowed or
    /// shared host-owned resources are never covered by this permission.
    async fn delete_owned_resources(&mut self) -> anyhow::Result<()> {
        Err(crate::transport::Failure::new(
            crate::abi::status::UNSUPPORTED,
            "component does not implement permanent owned-resource deletion",
        )
        .into())
    }
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
    /// Optional provider-driven initialization before ordinary processing.
    /// This first profile requires quiescent writers and has no durable handover.
    fn quiescent_bootstrap_input(&self) -> Option<PortId> {
        None
    }
    fn durable_bootstrap_input(&self) -> Option<PortId> {
        None
    }
    fn bootstrap_status(&self) -> anyhow::Result<BootstrapStatus> {
        anyhow::bail!("transformer does not support durable initialization")
    }
    async fn reset_bootstrap(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("transformer does not support bootstrap reset")
    }
    async fn bootstrap(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::bail!("transformer does not support provider-driven bootstrap")
    }
    async fn bootstrap_completed(
        &mut self,
        _completion: BootstrapCompletion,
    ) -> anyhow::Result<()> {
        anyhow::bail!("transformer does not support provider-driven bootstrap")
    }
    async fn delivery_completed(&mut self, _outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        Ok(())
    }
    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("transformer does not support durable output resumption")
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
    /// quiescence uses NativeComponent::pause_processing/resume_processing.
    async fn run(&mut self) -> anyhow::Result<()>;
}
