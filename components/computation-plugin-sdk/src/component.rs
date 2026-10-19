// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Plugin-side operations supported by the native ABI. Host-only query APIs,
//! storage owners, graph control handlers and construction contexts are absent.

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, InputEnvelope, OutputEnvelope, SinkBatching, SinkCompletion,
    SourceProgressView, WakeupSource,
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
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn stop(&mut self) -> anyhow::Result<()>;
}

#[async_trait]
pub trait NativeSource: NativeComponent {
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
    fn supports_snapshot(&self) -> bool {
        false
    }
    async fn replace_snapshot(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::bail!("this sink does not support replacing its state from a snapshot")
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
