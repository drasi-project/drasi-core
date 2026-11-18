// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native MySQL source component: lifecycle and management around the
//! shared replication engine in `drasi-mysql-common`.

use crate::config::SourceConfig;
use crate::management;
use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ConsumerProgressView, InputPositionPolicy, OutputEnvelope,
    SourceProgressKey, SourceProgressView,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
    PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSource, UpdateRequest,
};
use drasi_core_types::interface::StorageDurability;
use drasi_mysql_common::native::source::Activation;
use drasi_mysql_common::native::MySqlSource;
use std::sync::Arc;

pub struct MySqlComponent {
    pub(crate) config: SourceConfig,
    pub(crate) source: MySqlSource,
    /// The activation credentials shared with paired bootstrap providers,
    /// when the bootstrap coordinator constructed this source.
    pub(crate) activation: Option<drasi_mysql_common::native::ActivationCredentials>,
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

impl MySqlComponent {
    /// Whether the host's progress shows no committed cursor for this source.
    /// Only query-owned progress is observable; shared consumer sets are
    /// treated as committed.
    fn uncommitted(&self) -> bool {
        let Some(progress) = self.source.recovery_reader() else {
            return !self.config.mode.tracked();
        };
        progress.snapshot().is_ok_and(|snapshot| {
            !snapshot
                .checkpoints
                .contains_key(&SourceProgressKey::Source(self.descriptor().id().clone()))
        })
    }

    fn live_assessment(&self, request: &UpdateRequest) -> Result<Assessment> {
        let (current, proposed) = management::configurations(self.config.mode, request)?;
        Ok(management::plan(&current, &proposed, self.uncommitted()))
    }
}

#[async_trait]
impl NativeComponent for MySqlComponent {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.source.descriptor()
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.config.to_value()
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
    /// Validates the configuration. No connection is opened.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        self.config.validate()?;
        Ok(())
    }
    /// Waits for progress readiness within the host deadline and
    /// cancellation, then opens replication within `ioTimeoutMs`.
    /// Host-supplied activation credentials replace the configured user and
    /// password for this activation.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        let credentials = ctx
            .take_credentials()
            .map(|credentials| credentials.into_auth_pair());
        if let Some(activation) = &self.activation {
            *activation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = credentials.clone();
        }
        let cancellation = ctx.cancellation().clone();
        self.source
            .start_with(Activation {
                cancellation: Some(&cancellation),
                deadline: ctx.deadline(),
                credentials,
            })
            .await?;
        Ok(())
    }
    /// Stops decoding and emitting at the next transaction boundary while
    /// the replication connection stays open (see `DESIGN.md`).
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.source
            .pause(ctx.deadline(), Some(ctx.cancellation()))
            .await?;
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.source.resume();
        Ok(())
    }
    /// `Drain` closes replication gracefully; `Abort` cancels the worker.
    /// Both honour the deadline and keep ownership on timeout.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.source.stop_with(ctx.mode(), ctx.deadline()).await?;
        // The activation's credentials end with it.
        if let Some(activation) = &self.activation {
            *activation
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        Ok(())
    }
    /// The source owns no server-side state (no slot, no table, no
    /// registration that outlives its connection), so erasing disposal has
    /// nothing to remove. Dispose retires the schema reader, is idempotent
    /// and refuses while a worker is owned.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        self.source.dispose()?;
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for MySqlComponent {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        self.live_assessment(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let (current, proposed) =
            management::configurations(self.config.mode, &update.request).map_err(refused)?;
        if current.to_value().map_err(refused)? != self.config.to_value().map_err(refused)? {
            return Err(refused(
                "the source configuration changed after the update was assessed",
            ));
        }
        let assessment = self.live_assessment(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        proposed.validate().map_err(refused)?;
        let engine = proposed
            .engine_config(self.descriptor().id())
            .map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.source.is_running() && !self.source.is_paused() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                if !management::same_plan(&assessment, UpdateBoundary::Pause) {
                    return Err(refused("only reconnectBudgetMs can change in place"));
                }
                self.source
                    .set_reconnect_budget(proposed.settings.reconnect_budget_ms);
            }
            UpdateBoundary::Stop => {
                if self.source.is_running() {
                    return Err(refused("restart updates require a stopped source"));
                }
                self.source.reconfigure(engine).map_err(refused)?;
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.config = proposed;
        Ok(())
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
