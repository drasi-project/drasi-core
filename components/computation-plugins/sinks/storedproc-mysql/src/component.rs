// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::MySqlStoredProcConfig;
use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, InputEnvelope, SinkCompletion,
};
use drasi_computation_plugin_sdk::{ActivationCredentials, NativeComponent, NativeSink};
use drasi_mysql_common::storedproc::runtime::MySqlStoredProcSink as Sink;

pub struct MySqlStoredProcSink(Sink);

impl MySqlStoredProcSink {
    pub fn new(id: ComponentId, config: MySqlStoredProcConfig) -> Result<Self> {
        Ok(Self(Sink::new(id, config)?))
    }
    pub(crate) fn new_with_activation_credentials(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        bound: bool,
    ) -> Result<Self> {
        Ok(Self(Sink::new_with_activation_credentials(
            id, config, bound,
        )?))
    }
    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Sink::describe(id)
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
    async fn start(&mut self) -> Result<()> {
        self.0.start().await
    }
    async fn start_with_credentials(&mut self, credentials: ActivationCredentials) -> Result<()> {
        credentials.validate()?;
        let (username, password) = credentials.into_auth_pair();
        self.0.start_with_credentials(username, password).await
    }
    async fn stop(&mut self) -> Result<()> {
        self.0.stop().await
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
