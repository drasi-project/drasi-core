// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Plugin-side operations supported by the native ABI. Host-only query APIs,
//! storage owners, graph control handlers and construction contexts are absent.

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ConsumerProgressView, InputEnvelope, InputPositionPolicy,
    OutputEnvelope, OutputReaders, PortId, SinkBatching, SinkCompletion, SnapshotConsumer,
    SourceProgressView, SourceSchemaReader, WakeupSource,
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
    /// Snapshot and/or replay providers for this component's declared output
    /// ports. Obtaining them performs no I/O and does not take the component's
    /// processing slot; each provider must read its own state independently of
    /// pending `next`/`transform` work and return a typed error when it cannot
    /// answer. Registration is fixed at construction: factory metadata lists
    /// exactly the output ports with a snapshot (`output_snapshots`) and replay
    /// (`output_replays`) provider, and activation cannot add or remove them.
    /// Reads are served only while the component is started.
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
    async fn start(&mut self) -> anyhow::Result<()>;
    /// Called instead of `start` by the normal owned START operation when an
    /// identity resource was bound at construction. Never update configuration
    /// with these ephemeral values or fall back to literal authentication.
    async fn start_with_credentials(
        &mut self,
        _credentials: crate::ActivationCredentials,
    ) -> anyhow::Result<()> {
        anyhow::bail!("component does not implement activation credentials")
    }
    async fn stop(&mut self) -> anyhow::Result<()>;
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
    async fn run(&mut self) -> anyhow::Result<()>;
    async fn quiesce(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
