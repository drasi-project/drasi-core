// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The log sink component: lifecycle, management and input handling.

use crate::config::LogSinkConfig;
use crate::output::Emitter;
use crate::render::Renderer;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentUpdateData, GraphChangeBatch, InputEnvelope, SinkCompletion,
    SinkOutputs,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
    LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSink, UpdateRequest,
};

/// Everything derived from the configuration that `handle` reads. Replaced
/// as a whole by an in-place update.
struct Live {
    config: LogSinkConfig,
    renderer: Renderer,
    emitter: Emitter,
}

impl Live {
    fn new(config: LogSinkConfig, outputs: &SinkOutputs) -> Result<Self> {
        Ok(Self {
            renderer: Renderer::new(&config, outputs)?,
            emitter: Emitter {
                output: config.output,
                level: config.level,
                target: config.target.clone(),
            },
            config,
        })
    }
}

pub(crate) struct LogSink {
    descriptor: ComponentDescriptor,
    outputs: SinkOutputs,
    live: Live,
    started: bool,
    paused: bool,
}

impl LogSink {
    pub(crate) fn new(
        descriptor: ComponentDescriptor,
        config: LogSinkConfig,
        outputs: SinkOutputs,
    ) -> Result<Self> {
        Ok(Self {
            descriptor,
            live: Live::new(config, &outputs)?,
            outputs,
            started: false,
            paused: false,
        })
    }

    /// Renders every line of one input. Under the strict policy any render
    /// failure fails the whole input before anything is emitted.
    pub(crate) fn lines(&self, input: &InputEnvelope) -> Result<Vec<String>> {
        ensure!(input.port.as_str() == "in", "unknown log input port");
        let output = self
            .outputs
            .output_for(input.envelope.system().stream())
            .context("log input stream is not bound to an output")?;
        let batch = GraphChangeBatch::from_envelope(&input.envelope)?;
        let notifications = batch.notifications(output);
        let id = self.descriptor.id();
        let config = &self.live.config;
        let mut lines = Vec::with_capacity(notifications.len() + 1);
        let indent = if config.include_header && !notifications.is_empty() {
            lines.push(format!(
                "[{id}] Output '{}' sequence {} ({} changes):",
                output.alias,
                batch.sequence(),
                notifications.len()
            ));
            "   "
        } else {
            " "
        };
        for notification in &notifications {
            let rendered = self.live.renderer.render(notification)?;
            if let Some(error) = rendered.fallback {
                log::warn!(
                    target: config.target.as_str(),
                    "[{id}] log template for output '{}' (sequence {}, {}) failed to render; emitting the {:?} format instead: {error}",
                    notification.alias(),
                    notification.sequence(),
                    notification.change().operation(),
                    config.format,
                );
            }
            lines.push(format!("[{id}]{indent}{}", rendered.message));
        }
        Ok(lines)
    }
}

/// Classifies a configuration change by the weakest safe boundary.
///
/// * Replace: the `outputs` aliases, the incoming bindings, or
///   `onPermanentFailure`. Live input rebinding is a host decision, never an
///   in-place update, and the permanent-failure policy is part of the
///   description the graph captured when it constructed the sink.
/// * Pause (in place): everything else (routes, templates, format, header,
///   level, target, output, render policy). All are read per input.
pub(crate) fn plan(
    current: &LogSinkConfig,
    proposed: &LogSinkConfig,
    inputs_changed: bool,
) -> Assessment {
    let boundary = if inputs_changed
        || current.outputs != proposed.outputs
        || current.on_permanent_failure != proposed.on_permanent_failure
    {
        UpdateBoundary::Replace
    } else {
        UpdateBoundary::Pause
    };
    Assessment::supported(boundary, ComponentUpdateData::Preserve)
}

/// Pure planning used by the factory and the live instance. The proposed
/// configuration is validated as construction would validate it: against
/// the proposed inputs when the request carries them, otherwise templates
/// only.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure!(
        request.current.id == request.proposed.id
            && request.current.implementation == request.proposed.implementation
            && request.current.configuration_version == request.proposed.configuration_version,
        "updates cannot change component identity, implementation or configuration version"
    );
    let current = LogSinkConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = LogSinkConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    if request.proposed.inputs.is_empty() {
        Renderer::compile(&proposed)?;
    } else {
        let outputs = SinkOutputs::from_inputs(&request.proposed.inputs, &proposed.outputs)?;
        Renderer::new(&proposed, &outputs)?;
    }
    Ok(plan(
        &current,
        &proposed,
        request.current.inputs != request.proposed.inputs,
    ))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for LogSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.live.config)?)
    }

    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    /// Templates, helpers and routes were validated at construction; there
    /// is nothing to bind.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }

    /// Opens input handling. A Start cancelled before it runs reports
    /// `Cancelled` and leaves the sink startable.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.started {
            return Err(anyhow::anyhow!("log sink already started").into());
        }
        self.started = true;
        self.paused = false;
        Ok(())
    }

    /// The sink owns no worker: `handle` runs only when the host calls it and
    /// completes before returning. Pause therefore succeeds immediately once
    /// the host has settled the in-flight `handle`; input offered while paused
    /// is refused.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        if self.started {
            self.paused = true;
        }
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }

    /// Closes input handling. Nothing is buffered, so Drain and Abort are the
    /// same and complete immediately. Stop before Start succeeds.
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.started = false;
        self.paused = false;
        Ok(())
    }

    /// The sink owns no state: emitted lines belong to the log destination.
    /// `Preserve` and `Erase` are idempotent no-ops. Dispose is refused while
    /// the sink is running.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        if self.started {
            return Err(anyhow::anyhow!("dispose requires a stopped log sink").into());
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for LogSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        let assessment = assess(request)?;
        // The live bound outputs are the facts an in-place update relies on.
        if matches!(
            assessment,
            Assessment::Supported {
                boundary: UpdateBoundary::Pause,
                ..
            }
        ) {
            Renderer::new(
                &LogSinkConfig::parse(request.proposed.configuration.clone())?,
                &self.outputs,
            )?;
        }
        Ok(assessment)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current =
            LogSinkConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        let proposed =
            LogSinkConfig::parse(update.request.proposed.configuration.clone()).map_err(refused)?;
        if current != self.live.config {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let assessment = plan(
            &current,
            &proposed,
            update.request.current.inputs != update.request.proposed.inputs,
        );
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.started && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
            }
            UpdateBoundary::Stop => {
                if self.started {
                    return Err(refused("restart updates require a stopped sink"));
                }
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        // Built completely before the swap, so a failure leaves the sink unchanged.
        self.live = Live::new(proposed, &self.outputs).map_err(refused)?;
        Ok(())
    }
}

#[async_trait]
impl NativeSink for LogSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    /// Renders every change, then emits all lines. Returns `Ok` only after
    /// the lines were handed to the host's logger or written and flushed.
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        ensure!(self.started, "log sink is not running");
        ensure!(!self.paused, "log sink is paused");
        let lines = self.lines(&input)?;
        self.live.emitter.emit(&lines).await
    }
}
