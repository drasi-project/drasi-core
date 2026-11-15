// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::MySqlStoredProcConfig;
use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, InputBinding, InputEnvelope, PortDescriptor, SinkCompletion,
};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, DisposeContext, InitializeContext, LifecycleResult, PauseContext, ResumeContext,
    StartContext, StopContext,
};
use drasi_computation_plugin_sdk::{ActivationCredentials, NativeComponent, NativeSink};
use drasi_mysql_common::storedproc::runtime::MySqlStoredProcSink as Sink;

pub struct MySqlStoredProcSink(Sink);

impl MySqlStoredProcSink {
    pub fn new(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        inputs: &[InputBinding],
    ) -> Result<Self> {
        Ok(Self(Sink::new(id, config, inputs)?))
    }
    pub(crate) fn new_with_activation_credentials(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        bound: bool,
        inputs: &[InputBinding],
    ) -> Result<Self> {
        Ok(Self(Sink::new_with_activation_credentials(
            id, config, bound, inputs,
        )?))
    }
    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Sink::describe(id)
    }
    pub(crate) fn ports() -> Result<Vec<PortDescriptor>> {
        Sink::ports()
    }
}

#[async_trait]
impl NativeComponent for MySqlStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.0.descriptor()
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.0.configuration()
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        match ctx.take_credentials() {
            Some(credentials) => Ok(self.start_with_credentials(credentials).await?),
            None => Ok(self.start_activation().await?),
        }
    }
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        Ok(self.stop_activation().await?)
    }
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        nothing_to_do()
    }
}

impl MySqlStoredProcSink {
    async fn start_activation(&mut self) -> Result<()> {
        self.0.start().await
    }
    async fn stop_activation(&mut self) -> Result<()> {
        self.0.stop().await
    }
    async fn start_with_credentials(&mut self, credentials: ActivationCredentials) -> Result<()> {
        credentials.validate()?;
        let (username, password) = credentials.into_auth_pair();
        self.0.start_with_credentials(username, password).await
    }
}
#[async_trait]
impl NativeSink for MySqlStoredProcSink {
    fn completion(&self) -> SinkCompletion {
        self.0.completion()
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.0.handle(input).await
    }
}
