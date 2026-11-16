// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Lifecycle, delivery and management of the native MySQL consumer.

use crate::{config::MySqlStoredProcConfig, management};
use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, DeliveryHandler, DeliveryItem, DeliveryScope, InputBinding,
    PortDescriptor,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
    LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentials, ApplyUpdateRequest, NativeComponent, NativeManagement, UpdateRequest,
};
use drasi_mysql_common::storedproc::runtime::{CleanupPending, MySqlStoredProcSink as Runtime};

/// The native MySQL stored-procedure consumer (`ConsumerMode::External`).
pub struct MySqlStoredProcSink {
    runtime: Runtime,
    credentials_bound: bool,
}

impl MySqlStoredProcSink {
    pub fn new(
        id: ComponentId,
        config: MySqlStoredProcConfig,
        credentials_bound: bool,
        inputs: &[InputBinding],
        scope: DeliveryScope,
    ) -> Result<Self> {
        Ok(Self {
            runtime: Runtime::new_with_activation_credentials(
                id,
                config,
                credentials_bound,
                inputs,
                scope,
            )?,
            credentials_bound,
        })
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Runtime::describe(id)
    }

    pub(crate) fn ports() -> Result<Vec<PortDescriptor>> {
        Runtime::ports()
    }

    /// The cursor owner of this component.
    pub fn owner(&self) -> &str {
        self.runtime.owner()
    }
}

fn lifecycle_error(error: anyhow::Error) -> LifecycleError {
    if error.downcast_ref::<CleanupPending>().is_some() {
        LifecycleError::retryable(error)
    } else {
        error.into()
    }
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for MySqlStoredProcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.runtime.descriptor()
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.runtime.configuration()
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Templates, routes and the cursor table name were validated and
    /// compiled at construction; nothing is opened here.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    /// Opens the activation session and creates or verifies the cursor
    /// table within the start deadline; a cancelled start leaves nothing open.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        let credentials = match ctx.take_credentials() {
            Some(credentials) => Some(pair(credentials)?),
            None => None,
        };
        let cancellation = ctx.cancellation().clone();
        let deadline = ctx.deadline();
        match self
            .runtime
            .start(credentials, &cancellation, deadline)
            .await
        {
            Ok(()) => Ok(()),
            Err(_) if cancellation.is_cancelled() => Err(LifecycleError::cancelled()),
            Err(error) => Err(lifecycle_error(error)),
        }
    }
    /// Host-driven (D-6): the runner calls no `handle` while paused and has
    /// settled the in-flight one. Pause is the in-place update boundary.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.runtime.pause();
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.runtime.resume();
        Ok(())
    }
    /// `Drain`: the host awaited the in-flight operation; the idle session
    /// closes. `Abort`: the host dropped it; the session it left mid-command
    /// is KILLed. On deadline expiry the session stays owned (retryable).
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.runtime
            .stop(ctx.mode(), ctx.deadline())
            .await
            .map_err(lifecycle_error)
    }
    /// `Erase` deletes only this component's cursor rows, idempotently, then
    /// closes everything; `Preserve` keeps them.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        self.runtime
            .dispose(ctx.erases_owned_state(), ctx.deadline())
            .await
            .map_err(lifecycle_error)
    }
}

fn pair(credentials: ActivationCredentials) -> Result<(String, String)> {
    credentials.validate()?;
    Ok(credentials.into_auth_pair())
}

#[async_trait]
impl NativeManagement for MySqlStoredProcSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        if !self.credentials_bound {
            MySqlStoredProcConfig::parse(request.proposed.configuration.clone())?;
        }
        management::assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = MySqlStoredProcConfig::parse_with_activation_credentials(
            update.request.current.configuration.clone(),
            self.credentials_bound,
        )
        .map_err(refused)?;
        let proposed = MySqlStoredProcConfig::parse_with_activation_credentials(
            update.request.proposed.configuration.clone(),
            self.credentials_bound,
        )
        .map_err(refused)?;
        if &current != self.runtime.config() {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let assessment = self.assess(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        let required = UpdateBoundary::from(plan.mode);
        let boundary = ctx.boundary();
        if required == UpdateBoundary::Replace || boundary < required {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: format!("the update requires the {required:?} boundary"),
            });
        }
        match boundary {
            UpdateBoundary::Pause if self.runtime.is_started() && !self.runtime.is_paused() => {
                return Err(refused("in-place updates require a completed pause"));
            }
            UpdateBoundary::Stop if self.runtime.is_started() => {
                return Err(refused("restart updates require a stopped sink"));
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
            _ => {}
        }
        self.runtime.reconfigure(proposed).map_err(refused)
    }
}

#[async_trait]
impl DeliveryHandler for MySqlStoredProcSink {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        self.runtime.retryable(error)
    }
    fn delivery_guarantee(&self) -> drasi_computation_contracts::DeliveryGuarantee {
        self.runtime.delivery_guarantee()
    }
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        self.runtime.handle(item).await
    }
}
