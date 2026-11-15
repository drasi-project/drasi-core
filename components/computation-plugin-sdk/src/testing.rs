// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Conformance helpers for native component tests (feature `test-support`).
//!
//! [`SourceConformance`] drives a source factory through the lifecycle
//! scenarios every production native source must pass: start/stop/restart,
//! real Pause gating (or an explicit `unsupported`), Drain and Abort stops,
//! cancelled starts, idempotent erasing disposal, configuration round-trips
//! and the factory's update-planning matrix. A plugin supplies only a
//! [`SourceProbe`] that can offer one event to its source from outside.
//!
//! [`SinkConformance`] does the same for sinks: start/stop/restart, Pause
//! (host-driven, gated or `unsupported`), Drain and Abort, retrying a failed
//! input without a restart, idempotent erasure, configuration round-trips,
//! the update matrix and live updates. A plugin supplies a [`SinkProbe`] that
//! builds inputs and observes its destination.
//!
//! These helpers run the linked factory in-process. Run the same plugin's
//! dynamic library through the Host SDK to cover the exported ABI path.

use crate::{
    lifecycle::{
        ApplyContext, Assessment, CancellationToken, DisposeContext, DisposeReason,
        InitializeContext, LifecycleErrorKind, OperationScope, PauseContext, ResumeContext,
        StartContext, StateIntent, StopContext, StopMode, UpdateBoundary,
    },
    ApplyUpdateRequest, Component, ControlSender, CreateRequest, Factory, NativeComponent,
    NativeSink, NativeSource, UpdateRequest,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentId, InputBinding, InputEnvelope};
use serde_json::Value;
use std::time::Duration;

/// Bound for each awaited step, so a hung component fails instead of hanging.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Lifecycle hook contexts with fresh operation identities.
pub mod contexts {
    use super::*;

    pub fn scope(operation: &str) -> OperationScope {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        OperationScope::new(
            format!("{operation}-{id}"),
            "conformance",
            1,
            Some(tokio::time::Instant::now() + STEP_TIMEOUT),
            CancellationToken::new(),
        )
    }
    pub fn initialize() -> InitializeContext {
        InitializeContext::new(scope("initialize"))
    }
    pub fn start() -> StartContext {
        StartContext::new(scope("start"), None)
    }
    /// A Start whose cancellation was already requested.
    pub fn cancelled_start() -> StartContext {
        let scope = scope("cancelled-start");
        scope.cancellation().cancel();
        StartContext::new(scope, None)
    }
    pub fn pause() -> PauseContext {
        PauseContext::new(scope("pause"))
    }
    pub fn resume() -> ResumeContext {
        ResumeContext::new(scope("resume"))
    }
    pub fn stop(mode: StopMode) -> StopContext {
        StopContext::new(scope("stop"), mode)
    }
    pub fn dispose(
        reason: DisposeReason,
        intent: StateIntent,
        attempt: u32,
    ) -> Result<DisposeContext> {
        Ok(DisposeContext::new(
            scope("dispose"),
            reason,
            intent,
            attempt,
        )?)
    }
    pub fn apply(boundary: UpdateBoundary) -> ApplyContext {
        ApplyContext::new(scope("apply"), boundary)
    }
}

/// A creation request for `factory` with the given configuration.
pub fn create_request(
    factory: &dyn Factory,
    id: &ComponentId,
    configuration: Value,
) -> CreateRequest {
    let metadata = factory.metadata();
    CreateRequest {
        id: id.clone(),
        implementation: metadata.implementation,
        configuration_version: metadata.configuration_version,
        configuration,
        scope: None,
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
    }
}

/// An update request between two configurations of the same component.
pub fn update_request(
    factory: &dyn Factory,
    id: &ComponentId,
    current: Value,
    proposed: Value,
) -> UpdateRequest {
    UpdateRequest {
        current: create_request(factory, id, current),
        proposed: create_request(factory, id, proposed),
    }
}

/// Constructs a volatile source through the factory's standalone path.
pub fn create_source(
    factory: &dyn Factory,
    id: &ComponentId,
    configuration: Value,
) -> Result<Box<dyn NativeSource>> {
    match factory
        .create(
            &create_request(factory, id, configuration),
            ControlSender::default(),
        )?
        .component
    {
        Component::Source(source) => Ok(source),
        _ => anyhow::bail!("factory did not create a source"),
    }
}

/// Result of offering one event to a source from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// The source acknowledged the event.
    Accepted,
    /// The source refused it (paused, stopped or not listening).
    Refused,
}

/// Plugin-specific access to a running source.
#[async_trait]
pub trait SourceProbe: Send + Sync {
    /// Offers one event through the source's external interface. The probe
    /// reads the endpoint from `configuration`, the source's current exposed
    /// configuration, so updated listener settings are followed.
    async fn offer(&self, configuration: &Value) -> Result<Offer>;
}

/// Runs the standard native-source scenarios against one factory.
pub struct SourceConformance<'a> {
    pub factory: &'a dyn Factory,
    pub id: ComponentId,
}

async fn step<T>(name: &str, future: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .with_context(|| format!("conformance step `{name}` timed out"))
}

impl<'a> SourceConformance<'a> {
    pub fn new(factory: &'a dyn Factory, id: &str) -> Result<Self> {
        Ok(Self {
            factory,
            id: ComponentId::try_new(id)?,
        })
    }

    fn create(&self, configuration: Value) -> Result<Box<dyn NativeSource>> {
        create_source(self.factory, &self.id, configuration)
    }

    async fn offer_and_receive(
        &self,
        source: &mut dyn NativeSource,
        probe: &dyn SourceProbe,
    ) -> Result<()> {
        let configuration = source.configuration()?;
        ensure!(
            step("offer", probe.offer(&configuration)).await?? == Offer::Accepted,
            "a running source must accept an offered event"
        );
        let output = step("next", source.next()).await??;
        ensure!(output.is_some(), "an accepted event must reach next");
        Ok(())
    }

    /// Initialize, start, offer, restart, Drain and Abort stops, and a
    /// preserving dispose. Pause must gate admission or report `unsupported`.
    pub async fn lifecycle(&self, configuration: Value, probe: &dyn SourceProbe) -> Result<()> {
        let mut source = self.create(configuration)?;
        source.initialize(&contexts::initialize()).await?;
        source
            .stop(&contexts::stop(StopMode::Drain))
            .await
            .context("stop before start must succeed")?;
        step("start", source.start(&mut contexts::start())).await??;
        ensure!(
            source.start(&mut contexts::start()).await.is_err(),
            "a second start must be refused while running"
        );
        self.offer_and_receive(source.as_mut(), probe).await?;

        match step("pause", source.pause(&contexts::pause())).await? {
            Ok(()) => {
                let configuration = source.configuration()?;
                ensure!(
                    step("offer while paused", probe.offer(&configuration)).await?? == Offer::Refused,
                    "a paused source must refuse new events; return `unsupported` from pause if it cannot"
                );
                step("resume", source.resume(&contexts::resume())).await??;
                self.offer_and_receive(source.as_mut(), probe).await?;
            }
            Err(error) => ensure!(
                error.kind() == LifecycleErrorKind::Unsupported,
                "pause must succeed with real gating or return unsupported: {error:#}"
            ),
        }

        step("drain stop", source.stop(&contexts::stop(StopMode::Drain))).await??;
        let configuration = source.configuration()?;
        ensure!(
            step("offer while stopped", probe.offer(&configuration)).await?? == Offer::Refused,
            "a stopped source must not accept events"
        );
        step("restart", source.start(&mut contexts::start())).await??;
        self.offer_and_receive(source.as_mut(), probe).await?;
        step("abort stop", source.stop(&contexts::stop(StopMode::Abort))).await??;
        step(
            "dispose",
            source.dispose(&contexts::dispose(
                DisposeReason::Shutdown,
                StateIntent::Preserve,
                1,
            )?),
        )
        .await??;
        Ok(())
    }

    /// A Start that was cancelled before it ran leaves nothing listening and
    /// the object startable.
    pub async fn cancelled_start(
        &self,
        configuration: Value,
        probe: &dyn SourceProbe,
    ) -> Result<()> {
        let mut source = self.create(configuration)?;
        source.initialize(&contexts::initialize()).await?;
        let error = step(
            "cancelled start",
            source.start(&mut contexts::cancelled_start()),
        )
        .await?
        .err()
        .context("a pre-cancelled start must not succeed")?;
        ensure!(
            error.kind() == LifecycleErrorKind::Cancelled,
            "a pre-cancelled start must report cancellation: {error:#}"
        );
        source.stop(&contexts::stop(StopMode::Abort)).await?;
        step(
            "start after cancellation",
            source.start(&mut contexts::start()),
        )
        .await??;
        self.offer_and_receive(source.as_mut(), probe).await?;
        source.stop(&contexts::stop(StopMode::Drain)).await?;
        Ok(())
    }

    /// A removal that erases owned state succeeds and is idempotent on retry.
    pub async fn erase(&self, configuration: Value) -> Result<()> {
        ensure!(
            self.factory.metadata().capabilities.erases_owned_state,
            "factory does not declare erases_owned_state"
        );
        let mut source = self.create(configuration)?;
        source.initialize(&contexts::initialize()).await?;
        step("start", source.start(&mut contexts::start())).await??;
        step("stop", source.stop(&contexts::stop(StopMode::Drain))).await??;
        for attempt in [1, 2] {
            step(
                "erase",
                source.dispose(&contexts::dispose(
                    DisposeReason::Removal,
                    StateIntent::Erase,
                    attempt,
                )?),
            )
            .await?
            .with_context(|| format!("erase attempt {attempt} failed"))?;
        }
        Ok(())
    }

    /// Construction from `configuration()` reproduces the same configuration,
    /// and published defaults agree with the constructed values.
    pub fn configuration_round_trip(&self, configurations: &[Value]) -> Result<()> {
        let metadata = self.factory.metadata();
        let details = self
            .factory
            .configuration_details()
            .context("production sources publish configuration details")?;
        details.validate_schema(&metadata.configuration)?;
        for configuration in configurations {
            let exposed = self.create(configuration.clone())?.configuration()?;
            ensure!(
                details.equivalent_with_defaults(&metadata.configuration, configuration, &exposed)?,
                "configuration() must equal the constructed configuration: {configuration} vs {exposed}"
            );
            let again = self.create(exposed.clone())?.configuration()?;
            ensure!(
                exposed == again,
                "configuration() must round-trip through create"
            );
        }
        Ok(())
    }

    /// Checks the factory's assessment for each proposed change. `None`
    /// expects the change to be unsupported or rejected.
    pub fn assess_matrix(
        &self,
        current: &Value,
        cases: &[(Value, Option<UpdateBoundary>)],
    ) -> Result<()> {
        ensure!(
            self.factory.supports_update_planning(),
            "factory does not plan updates"
        );
        for (proposed, expected) in cases {
            let request = update_request(self.factory, &self.id, current.clone(), proposed.clone());
            let actual = match self.factory.assess(&request) {
                Ok(Assessment::Supported { boundary, .. }) => Some(boundary),
                Ok(Assessment::Unsupported { .. }) | Err(_) => None,
            };
            ensure!(
                actual == *expected,
                "assessment of {proposed} was {actual:?}, expected {expected:?}"
            );
        }
        Ok(())
    }

    /// Applies a retained-object update at its assessed boundary on a running
    /// source, then checks the exposed configuration and that the source
    /// still accepts events.
    pub async fn apply(
        &self,
        current: Value,
        proposed: Value,
        probe: &dyn SourceProbe,
    ) -> Result<()> {
        let mut source = self.create(current.clone())?;
        source.initialize(&contexts::initialize()).await?;
        step("start", source.start(&mut contexts::start())).await??;
        let request = update_request(self.factory, &self.id, current, proposed.clone());
        let assessment = source
            .management()
            .context("source must export management")?
            .assess(&request)?;
        let plan = assessment.plan().context("update must be supported")?;
        let boundary = UpdateBoundary::from(plan.mode);
        let update = ApplyUpdateRequest {
            operation_id: "conformance-apply".into(),
            request,
            plan,
            fingerprint: assessment.fingerprint().map(str::to_owned),
        };
        match boundary {
            UpdateBoundary::Pause => {
                step("pause", source.pause(&contexts::pause())).await??;
                apply(source.as_mut(), boundary, &update).await?;
                step("resume", source.resume(&contexts::resume())).await??;
            }
            UpdateBoundary::Stop => {
                step("stop", source.stop(&contexts::stop(StopMode::Drain))).await??;
                apply(source.as_mut(), boundary, &update).await?;
                step("restart", source.start(&mut contexts::start())).await??;
            }
            UpdateBoundary::Replace => anyhow::bail!("Replace is not a retained-object update"),
        }
        let metadata = self.factory.metadata();
        let details = self
            .factory
            .configuration_details()
            .context("configuration details")?;
        let exposed = source.configuration()?;
        ensure!(
            details.equivalent_with_defaults(&metadata.configuration, &proposed, &exposed)?,
            "configuration() must expose the applied configuration: {proposed} vs {exposed}"
        );
        self.offer_and_receive(source.as_mut(), probe).await?;
        source.stop(&contexts::stop(StopMode::Drain)).await?;
        Ok(())
    }
}

async fn apply(
    source: &mut dyn NativeComponent,
    boundary: UpdateBoundary,
    update: &ApplyUpdateRequest,
) -> Result<()> {
    let management = source
        .management()
        .context("source must export management")?;
    step(
        "apply",
        management.apply(&contexts::apply(boundary), update),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("apply failed: {error}"))
}

/// Constructs a sink bound to `inputs` through the factory's standalone path.
pub fn create_sink(
    factory: &dyn Factory,
    id: &ComponentId,
    configuration: Value,
    inputs: &[InputBinding],
) -> Result<Box<dyn NativeSink>> {
    let mut request = create_request(factory, id, configuration);
    request.inputs = inputs.to_vec();
    match factory
        .create(&request, ControlSender::default())?
        .component
    {
        Component::Sink(sink) => Ok(sink),
        _ => anyhow::bail!("factory did not create an ordinary sink"),
    }
}

/// Plugin-specific access to a sink's inputs and destination.
#[async_trait]
pub trait SinkProbe: Send + Sync {
    /// An input envelope for the sink's input port carrying one change with
    /// the given producer sequence.
    fn input(&self, sequence: u64) -> Result<InputEnvelope>;
    /// Waits until the destination has observed at least `count` deliveries
    /// in total, failing after a bounded wait. Sinks whose destination is a
    /// subscriber (SSE) observe through their own client.
    async fn expect_delivered(&self, configuration: &Value, count: usize) -> Result<()>;
    /// Makes the destination fail (`false`) or succeed (`true`). Returns
    /// `false` when the probe cannot control its destination.
    async fn set_available(&self, _available: bool) -> Result<bool> {
        Ok(false)
    }
}

/// Runs the standard native-sink scenarios against one factory.
pub struct SinkConformance<'a> {
    pub factory: &'a dyn Factory,
    pub id: ComponentId,
    pub inputs: Vec<InputBinding>,
}

impl<'a> SinkConformance<'a> {
    pub fn new(factory: &'a dyn Factory, id: &str, inputs: Vec<InputBinding>) -> Result<Self> {
        Ok(Self {
            factory,
            id: ComponentId::try_new(id)?,
            inputs,
        })
    }

    fn create(&self, configuration: Value) -> Result<Box<dyn NativeSink>> {
        create_sink(self.factory, &self.id, configuration, &self.inputs)
    }

    async fn deliver(
        &self,
        sink: &mut dyn NativeSink,
        probe: &dyn SinkProbe,
        sequence: u64,
        delivered: usize,
    ) -> Result<()> {
        step("handle", sink.handle(probe.input(sequence)?))
            .await?
            .with_context(|| format!("handle of sequence {sequence} failed"))?;
        let configuration = sink.configuration()?;
        step(
            "expect delivered",
            probe.expect_delivered(&configuration, delivered),
        )
        .await??;
        Ok(())
    }

    /// Initialize, stop before start, start, double start refused, delivery,
    /// Pause/Resume (or `unsupported`), Drain, refusal while stopped, restart,
    /// Abort and a preserving dispose.
    pub async fn lifecycle(&self, configuration: Value, probe: &dyn SinkProbe) -> Result<()> {
        let mut sink = self.create(configuration)?;
        sink.initialize(&contexts::initialize()).await?;
        sink.stop(&contexts::stop(StopMode::Drain))
            .await
            .context("stop before start must succeed")?;
        step("start", sink.start(&mut contexts::start())).await??;
        ensure!(
            sink.start(&mut contexts::start()).await.is_err(),
            "a second start must be refused while running"
        );
        let mut delivered = 1;
        self.deliver(sink.as_mut(), probe, 1, delivered).await?;
        match step("pause", sink.pause(&contexts::pause())).await? {
            Ok(()) => {
                step("resume", sink.resume(&contexts::resume())).await??;
                delivered += 1;
                self.deliver(sink.as_mut(), probe, 2, delivered).await?;
            }
            Err(error) => ensure!(
                error.kind() == LifecycleErrorKind::Unsupported,
                "pause must succeed or return unsupported: {error:#}"
            ),
        }
        step("drain stop", sink.stop(&contexts::stop(StopMode::Drain))).await??;
        ensure!(
            step("handle while stopped", sink.handle(probe.input(3)?))
                .await?
                .is_err(),
            "a stopped sink must refuse input"
        );
        step("restart", sink.start(&mut contexts::start())).await??;
        delivered += 1;
        self.deliver(sink.as_mut(), probe, 4, delivered).await?;
        step("abort stop", sink.stop(&contexts::stop(StopMode::Abort))).await??;
        step(
            "dispose",
            sink.dispose(&contexts::dispose(
                DisposeReason::Shutdown,
                StateIntent::Preserve,
                1,
            )?),
        )
        .await??;
        Ok(())
    }

    /// A failed input is retried successfully on the same running object,
    /// without Stop/Start. Skipped when the probe cannot control the
    /// destination.
    pub async fn retry_after_failure(
        &self,
        configuration: Value,
        probe: &dyn SinkProbe,
    ) -> Result<()> {
        if !probe.set_available(false).await? {
            return Ok(());
        }
        let mut sink = self.create(configuration)?;
        sink.initialize(&contexts::initialize()).await?;
        step("start", sink.start(&mut contexts::start())).await??;
        let input = probe.input(1)?;
        ensure!(
            step("failed handle", sink.handle(input.clone()))
                .await?
                .is_err(),
            "delivery to an unavailable destination must fail under the strict policy"
        );
        probe.set_available(true).await?;
        step("retried handle", sink.handle(input))
            .await?
            .context("a failed input must be retryable without a restart")?;
        let configuration = sink.configuration()?;
        step(
            "expect delivered",
            probe.expect_delivered(&configuration, 1),
        )
        .await??;
        sink.stop(&contexts::stop(StopMode::Drain)).await?;
        Ok(())
    }

    /// A removal that erases owned state succeeds and is idempotent on retry.
    pub async fn erase(&self, configuration: Value) -> Result<()> {
        ensure!(
            self.factory.metadata().capabilities.erases_owned_state,
            "factory does not declare erases_owned_state"
        );
        let mut sink = self.create(configuration)?;
        sink.initialize(&contexts::initialize()).await?;
        step("start", sink.start(&mut contexts::start())).await??;
        step("stop", sink.stop(&contexts::stop(StopMode::Drain))).await??;
        for attempt in [1, 2] {
            step(
                "erase",
                sink.dispose(&contexts::dispose(
                    DisposeReason::Removal,
                    StateIntent::Erase,
                    attempt,
                )?),
            )
            .await?
            .with_context(|| format!("erase attempt {attempt} failed"))?;
        }
        Ok(())
    }

    /// Construction from `configuration()` reproduces the same configuration,
    /// and published defaults agree with the constructed values.
    pub fn configuration_round_trip(&self, configurations: &[Value]) -> Result<()> {
        let metadata = self.factory.metadata();
        let details = self
            .factory
            .configuration_details()
            .context("production sinks publish configuration details")?;
        details.validate_schema(&metadata.configuration)?;
        for configuration in configurations {
            let exposed = self.create(configuration.clone())?.configuration()?;
            ensure!(
                details.equivalent_with_defaults(&metadata.configuration, configuration, &exposed)?,
                "configuration() must equal the constructed configuration: {configuration} vs {exposed}"
            );
            let again = self.create(exposed.clone())?.configuration()?;
            ensure!(
                exposed == again,
                "configuration() must round-trip through create"
            );
        }
        Ok(())
    }

    /// Checks the factory's assessment for each proposed change. `None`
    /// expects the change to be unsupported or rejected.
    pub fn assess_matrix(
        &self,
        current: &Value,
        cases: &[(Value, Option<UpdateBoundary>)],
    ) -> Result<()> {
        ensure!(
            self.factory.supports_update_planning(),
            "factory does not plan updates"
        );
        for (proposed, expected) in cases {
            let mut request =
                update_request(self.factory, &self.id, current.clone(), proposed.clone());
            request.current.inputs = self.inputs.clone();
            request.proposed.inputs = self.inputs.clone();
            let actual = match self.factory.assess(&request) {
                Ok(Assessment::Supported { boundary, .. }) => Some(boundary),
                Ok(Assessment::Unsupported { .. }) | Err(_) => None,
            };
            ensure!(
                actual == *expected,
                "assessment of {proposed} was {actual:?}, expected {expected:?}"
            );
        }
        Ok(())
    }

    /// Applies a retained-object update at its assessed boundary on a running
    /// sink, then checks the exposed configuration and that delivery works.
    pub async fn apply(
        &self,
        current: Value,
        proposed: Value,
        probe: &dyn SinkProbe,
    ) -> Result<()> {
        let mut sink = self.create(current.clone())?;
        sink.initialize(&contexts::initialize()).await?;
        step("start", sink.start(&mut contexts::start())).await??;
        self.deliver(sink.as_mut(), probe, 1, 1).await?;
        let mut request = update_request(self.factory, &self.id, current, proposed.clone());
        request.current.inputs = self.inputs.clone();
        request.proposed.inputs = self.inputs.clone();
        let assessment = sink
            .management()
            .context("sink must export management")?
            .assess(&request)?;
        let plan = assessment.plan().context("update must be supported")?;
        let boundary = UpdateBoundary::from(plan.mode);
        let update = ApplyUpdateRequest {
            operation_id: "conformance-apply".into(),
            request,
            plan,
            fingerprint: assessment.fingerprint().map(str::to_owned),
        };
        match boundary {
            UpdateBoundary::Pause => {
                step("pause", sink.pause(&contexts::pause())).await??;
                apply(sink.as_mut(), boundary, &update).await?;
                step("resume", sink.resume(&contexts::resume())).await??;
            }
            UpdateBoundary::Stop => {
                step("stop", sink.stop(&contexts::stop(StopMode::Drain))).await??;
                apply(sink.as_mut(), boundary, &update).await?;
                step("restart", sink.start(&mut contexts::start())).await??;
            }
            UpdateBoundary::Replace => anyhow::bail!("Replace is not a retained-object update"),
        }
        let metadata = self.factory.metadata();
        let details = self
            .factory
            .configuration_details()
            .context("configuration details")?;
        let exposed = sink.configuration()?;
        ensure!(
            details.equivalent_with_defaults(&metadata.configuration, &proposed, &exposed)?,
            "configuration() must expose the applied configuration: {proposed} vs {exposed}"
        );
        self.deliver(sink.as_mut(), probe, 2, 2).await?;
        sink.stop(&contexts::stop(StopMode::Drain)).await?;
        Ok(())
    }
}
