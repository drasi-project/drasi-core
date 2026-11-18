// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Runs the plugin SDK conformance kit against a built dynamic library.
//!
//! [`LoadedTarget`] drives each component through [`NativeComponentProxy`],
//! the same proxy a host graph uses, so every hook, envelope and error crosses
//! the exported C ABI:
//!
//! ```ignore
//! let target = LoadedTarget::load(library_path, "drasi.network/http-source")?;
//! SourceConformance::with_target(target, "orders")?
//!     .lifecycle(configuration, &probe)
//!     .await?;
//! ```
//!
//! Live update application stays linked-only; assessment, lifecycle, dispose
//! and configuration scenarios all run loaded.

use super::{load, NativeComponentProxy, NativeFactory, NativePlugin};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::lifecycle::{
    DisposeReason, LifecycleError, LifecycleResult, OperationScope, StateIntent, StopMode,
};
use drasi_computation_contracts::{ComponentId, InputBinding, InputEnvelope};
use drasi_computation_plugin_sdk::{
    lifecycle::UpdateBoundary,
    testing::{ConformanceTarget, LifecycleSubject, SinkSubject, SourceSubject},
    ConfigDetails, FactoryMetadata, UpdateRequest,
};
use drasi_lib::computation::v1::{
    ComputationComponent, DisposeContext, EnvelopeSink, EnvelopeSource, InitializeContext,
    PauseContext, ResumeContext, StartContext, StopContext,
};
use serde_json::Value;
use std::{path::Path, sync::Arc};

/// A conformance target backed by one factory of a loaded dynamic library.
pub struct LoadedTarget {
    factory: Arc<NativeFactory>,
    _plugin: Arc<NativePlugin>,
}

impl LoadedTarget {
    /// Loads `library` and selects the factory implementing `name`.
    pub fn load(library: impl AsRef<Path>, name: &str) -> Result<Self> {
        let library = library.as_ref();
        let plugin = load(library)
            .with_context(|| format!("failed to load native plugin {}", library.display()))?;
        Self::select(plugin, name)
    }

    /// Selects the factory implementing `name` from an already loaded plugin.
    pub fn select(plugin: Arc<NativePlugin>, name: &str) -> Result<Self> {
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == name)
            .cloned()
            .with_context(|| format!("native plugin exports no `{name}` factory"))?;
        Ok(Self {
            factory,
            _plugin: plugin,
        })
    }

    pub fn factory(&self) -> &Arc<NativeFactory> {
        &self.factory
    }
}

impl ConformanceTarget for LoadedTarget {
    fn acknowledges_repeated_transitions(&self) -> bool {
        true
    }
    fn metadata(&self) -> FactoryMetadata {
        self.factory.metadata().clone()
    }
    fn configuration_details(&self) -> Option<ConfigDetails> {
        self.factory.configuration_details().cloned()
    }
    fn supports_update_planning(&self) -> bool {
        !self.factory.metadata().capabilities.update_modes.is_empty()
    }
    fn assess(&self, request: &UpdateRequest) -> Option<UpdateBoundary> {
        self.factory
            .plan_update_request(request)
            .ok()
            .map(|plan| plan.mode.into())
    }
    fn create_source(
        &self,
        id: &ComponentId,
        configuration: Value,
    ) -> Result<Box<dyn SourceSubject>> {
        Ok(Box::new(Loaded(
            self.factory.create_component(id.clone(), configuration)?,
        )))
    }
    fn create_sink(
        &self,
        id: &ComponentId,
        configuration: Value,
        inputs: &[InputBinding],
    ) -> Result<Box<dyn SinkSubject>> {
        Ok(Box::new(Loaded(
            self.factory.create_component_with_inputs(
                id.clone(),
                configuration,
                inputs.to_vec(),
            )?,
        )))
    }
}

struct Loaded(NativeComponentProxy);

// The proxy carries the plugin's classified lifecycle error across the ABI
// inside its `anyhow` chain; `LifecycleError::from` recovers the kind.
fn classified(result: Result<()>) -> LifecycleResult {
    result.map_err(LifecycleError::from)
}

#[async_trait]
impl LifecycleSubject for Loaded {
    fn configuration(&self) -> Result<Value> {
        ComputationComponent::configuration(&self.0)
    }
    async fn initialize(&mut self, scope: OperationScope) -> LifecycleResult {
        classified(self.0.initialize(&InitializeContext::new(scope)).await)
    }
    async fn start(&mut self, scope: OperationScope) -> LifecycleResult {
        classified(self.0.start(&StartContext::new(scope)).await)
    }
    async fn pause(&mut self, scope: OperationScope) -> LifecycleResult {
        classified(self.0.pause(&PauseContext::new(scope)).await)
    }
    async fn resume(&mut self, scope: OperationScope) -> LifecycleResult {
        classified(self.0.resume(&ResumeContext::new(scope)).await)
    }
    async fn stop(&mut self, scope: OperationScope, mode: StopMode) -> LifecycleResult {
        classified(self.0.stop(&StopContext::new(scope, mode)).await)
    }
    async fn dispose(
        &mut self,
        scope: OperationScope,
        reason: DisposeReason,
        intent: StateIntent,
        attempt: u32,
    ) -> LifecycleResult {
        let context = DisposeContext::new(scope, reason, intent, attempt)?;
        classified(self.0.dispose(&context).await)
    }
}

#[async_trait]
impl SourceSubject for Loaded {
    async fn next(&mut self) -> Result<bool> {
        Ok(EnvelopeSource::next(&mut self.0).await?.is_some())
    }
}

#[async_trait]
impl SinkSubject for Loaded {
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        EnvelopeSink::handle(&mut self.0, input).await
    }
}
