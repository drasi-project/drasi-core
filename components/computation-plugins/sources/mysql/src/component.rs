// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ConsumerProgressView, InputPositionPolicy, OutputEnvelope,
    SourceProgressView,
};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, DisposeContext, InitializeContext, LifecycleResult, PauseContext, ResumeContext,
    StartContext, StopContext,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
use drasi_core_types::interface::StorageDurability;
use drasi_mysql_common::native::MySqlSource;
use std::sync::Arc;

pub struct MySqlComponent {
    pub(crate) source: MySqlSource,
}

#[async_trait]
impl NativeComponent for MySqlComponent {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.source.descriptor()
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.source.configuration()
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        self.source.consumer_progress().map(|binding| binding.view)
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        self.source
            .consumer_progress()
            .and(self.source.replay_retention())
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.source.position_policy()
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn start(&mut self, _ctx: &mut StartContext) -> LifecycleResult {
        Ok(self.start_activation().await?)
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

impl MySqlComponent {
    async fn start_activation(&mut self) -> Result<()> {
        self.source.start().await
    }
    async fn stop_activation(&mut self) -> Result<()> {
        self.source.stop().await
    }
}

#[async_trait]
impl NativeSource for MySqlComponent {
    fn source_schema_reader(
        &self,
    ) -> Option<Arc<dyn drasi_computation_contracts::SourceSchemaReader>> {
        Some(self.source.schema_reader())
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.source.recovery_reader()
    }
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        self.source
            .replay_retention()
            .zip(self.source.recovery_reader())
            .map(|(durability, reader)| (durability, reader.component_id().clone()))
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.source.next().await
    }
}
