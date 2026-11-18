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
//! Both suites run against a [`ConformanceTarget`]. [`SourceConformance::new`]
//! and [`SinkConformance::new`] wrap a linked [`Factory`]; the Host SDK's
//! loaded-library driver (`drasi_host_sdk::computation::conformance`, feature
//! `test-support`) supplies a target that drives a built dynamic library
//! through the exported C ABI. The same scenarios and probes then cover both
//! the linked and the loaded path. Live update application (`apply`) needs
//! the linked factory.

use crate::{
    lifecycle::{
        ApplyContext, Assessment, CancellationToken, DisposeContext, DisposeReason,
        InitializeContext, LifecycleErrorKind, LifecycleResult, OperationScope, PauseContext,
        ResumeContext, StartContext, StateIntent, StopContext, StopMode, UpdateBoundary,
    },
    ApplyUpdateRequest, Component, ConfigDetails, ControlSender, CreateRequest, Factory,
    FactoryMetadata, NativeComponent, NativeSink, NativeSource, UpdateRequest,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentId, InputBinding, InputEnvelope};
use serde_json::Value;
use std::time::Duration;

/// Bound for each awaited step, so a hung component fails instead of hanging.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Every dispose reason, each with the state intent it permits.
pub const DISPOSE_MATRIX: [(DisposeReason, StateIntent); 5] = [
    (DisposeReason::Shutdown, StateIntent::Preserve),
    (DisposeReason::Removal, StateIntent::Preserve),
    (DisposeReason::Removal, StateIntent::Erase),
    (DisposeReason::Replacement, StateIntent::Preserve),
    (DisposeReason::InitializationFailure, StateIntent::Preserve),
];

/// One constructed component under test, linked or loaded. Each hook takes
/// the operation scope; the target builds its own context from it.
#[async_trait]
pub trait LifecycleSubject: Send {
    fn configuration(&self) -> Result<Value>;
    async fn initialize(&mut self, scope: OperationScope) -> LifecycleResult;
    async fn start(&mut self, scope: OperationScope) -> LifecycleResult;
    async fn pause(&mut self, scope: OperationScope) -> LifecycleResult;
    async fn resume(&mut self, scope: OperationScope) -> LifecycleResult;
    async fn stop(&mut self, scope: OperationScope, mode: StopMode) -> LifecycleResult;
    async fn dispose(
        &mut self,
        scope: OperationScope,
        reason: DisposeReason,
        intent: StateIntent,
        attempt: u32,
    ) -> LifecycleResult;
}

/// A source under test.
#[async_trait]
pub trait SourceSubject: LifecycleSubject {
    /// Waits for the next output. `false` means the source is exhausted.
    async fn next(&mut self) -> Result<bool>;
}

/// A sink under test.
#[async_trait]
pub trait SinkSubject: LifecycleSubject {
    async fn handle(&mut self, input: InputEnvelope) -> Result<()>;
}

/// Where conformance subjects come from: a linked factory or a loaded library.
pub trait ConformanceTarget: Send + Sync {
    fn metadata(&self) -> FactoryMetadata;
    fn configuration_details(&self) -> Option<ConfigDetails>;
    /// Whether the factory plans updates without an instance.
    fn supports_update_planning(&self) -> bool;
    /// The factory's boundary for `request`, or `None` when the change is
    /// unsupported or rejected.
    fn assess(&self, request: &UpdateRequest) -> Option<UpdateBoundary>;
    fn create_source(
        &self,
        id: &ComponentId,
        configuration: Value,
    ) -> Result<Box<dyn SourceSubject>>;
    fn create_sink(
        &self,
        id: &ComponentId,
        configuration: Value,
        inputs: &[InputBinding],
    ) -> Result<Box<dyn SinkSubject>>;
    /// The linked factory, when there is one. Live update application needs it.
    fn linked_factory(&self) -> Option<&dyn Factory> {
        None
    }
    /// Whether a repeated, already completed transition is acknowledged
    /// rather than refused. The native ABI acknowledges a retried transition
    /// without rerunning the hook, so a host retry after a lost reply is
    /// safe; a linked component refuses it. Either way the component must
    /// keep working afterwards.
    fn acknowledges_repeated_transitions(&self) -> bool {
        false
    }
}

/// Checks a second Start while running: refused, or acknowledged without
/// effect when the target says so.
async fn repeated_start(
    subject: &mut (dyn LifecycleSubject + '_),
    acknowledged: bool,
) -> Result<()> {
    let repeated = subject.start(contexts::scope("start")).await;
    if acknowledged {
        if let Err(error) = repeated {
            anyhow::bail!("a repeated start must be acknowledged: {error:#}");
        }
    } else {
        ensure!(
            repeated.is_err(),
            "a second start must be refused while running"
        );
    }
    Ok(())
}

/// A [`ConformanceTarget`] over a linked factory.
pub struct LinkedTarget<'a>(pub &'a dyn Factory);

impl ConformanceTarget for LinkedTarget<'_> {
    fn linked_factory(&self) -> Option<&dyn Factory> {
        Some(self.0)
    }
    fn metadata(&self) -> FactoryMetadata {
        self.0.metadata()
    }
    fn configuration_details(&self) -> Option<ConfigDetails> {
        self.0.configuration_details()
    }
    fn supports_update_planning(&self) -> bool {
        self.0.supports_update_planning()
    }
    fn assess(&self, request: &UpdateRequest) -> Option<UpdateBoundary> {
        match self.0.assess(request) {
            Ok(Assessment::Supported { boundary, .. }) => Some(boundary),
            Ok(Assessment::Unsupported { .. }) | Err(_) => None,
        }
    }
    fn create_source(
        &self,
        id: &ComponentId,
        configuration: Value,
    ) -> Result<Box<dyn SourceSubject>> {
        Ok(Box::new(Linked(create_source(self.0, id, configuration)?)))
    }
    fn create_sink(
        &self,
        id: &ComponentId,
        configuration: Value,
        inputs: &[InputBinding],
    ) -> Result<Box<dyn SinkSubject>> {
        Ok(Box::new(Linked(create_sink(
            self.0,
            id,
            configuration,
            inputs,
        )?)))
    }
}

struct Linked<T: ?Sized>(Box<T>);

macro_rules! linked_lifecycle {
    ($component:ty) => {
        #[async_trait]
        impl LifecycleSubject for Linked<$component> {
            fn configuration(&self) -> Result<Value> {
                self.0.configuration()
            }
            async fn initialize(&mut self, scope: OperationScope) -> LifecycleResult {
                self.0.initialize(&InitializeContext::new(scope)).await
            }
            async fn start(&mut self, scope: OperationScope) -> LifecycleResult {
                self.0.start(&mut StartContext::new(scope, None)).await
            }
            async fn pause(&mut self, scope: OperationScope) -> LifecycleResult {
                self.0.pause(&PauseContext::new(scope)).await
            }
            async fn resume(&mut self, scope: OperationScope) -> LifecycleResult {
                self.0.resume(&ResumeContext::new(scope)).await
            }
            async fn stop(&mut self, scope: OperationScope, mode: StopMode) -> LifecycleResult {
                self.0.stop(&StopContext::new(scope, mode)).await
            }
            async fn dispose(
                &mut self,
                scope: OperationScope,
                reason: DisposeReason,
                intent: StateIntent,
                attempt: u32,
            ) -> LifecycleResult {
                self.0
                    .dispose(&DisposeContext::new(scope, reason, intent, attempt)?)
                    .await
            }
        }
    };
}
linked_lifecycle!(dyn NativeSource);
linked_lifecycle!(dyn NativeSink);

#[async_trait]
impl SourceSubject for Linked<dyn NativeSource> {
    async fn next(&mut self) -> Result<bool> {
        Ok(self.0.next().await?.is_some())
    }
}

#[async_trait]
impl SinkSubject for Linked<dyn NativeSink> {
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.0.handle(input).await
    }
}

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
    /// A scope whose cancellation was already requested.
    pub fn cancelled_scope(operation: &str) -> OperationScope {
        let scope = scope(operation);
        scope.cancellation().cancel();
        scope
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

/// Runs the standard native-source scenarios against one target.
pub struct SourceConformance<'a, T = LinkedTarget<'a>> {
    target: T,
    _linked: std::marker::PhantomData<&'a ()>,
    id: ComponentId,
}

async fn step<T>(name: &str, future: impl std::future::Future<Output = T>) -> Result<T> {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .with_context(|| format!("conformance step `{name}` timed out"))
}

impl<'a> SourceConformance<'a> {
    /// Runs against a linked factory, including live update application.
    pub fn new(factory: &'a dyn Factory, id: &str) -> Result<Self> {
        Ok(Self {
            target: LinkedTarget(factory),
            _linked: std::marker::PhantomData,
            id: ComponentId::try_new(id)?,
        })
    }
}

impl<'a, T: ConformanceTarget> SourceConformance<'a, T> {
    /// Runs against any target, such as a loaded dynamic library.
    /// [`Self::apply`] needs a linked target.
    pub fn with_target(target: T, id: &str) -> Result<Self> {
        Ok(Self {
            target,
            _linked: std::marker::PhantomData,
            id: ComponentId::try_new(id)?,
        })
    }

    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    fn create(&self, configuration: Value) -> Result<Box<dyn SourceSubject>> {
        self.target.create_source(&self.id, configuration)
    }

    fn linked(&self) -> Result<&dyn Factory> {
        self.target
            .linked_factory()
            .context("live update application needs the linked factory")
    }

    fn request(&self, configuration: Value) -> CreateRequest {
        let metadata = self.target.metadata();
        CreateRequest {
            id: self.id.clone(),
            implementation: metadata.implementation,
            configuration_version: metadata.configuration_version,
            configuration,
            scope: None,
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
        }
    }

    fn update_request(&self, current: Value, proposed: Value) -> UpdateRequest {
        UpdateRequest {
            current: self.request(current),
            proposed: self.request(proposed),
        }
    }

    /// Every dispose reason with each intent it permits ends a fresh,
    /// initialized object. Erase runs only when the factory declares it.
    pub async fn dispose_matrix(&self, configuration: Value) -> Result<()> {
        let erases = self.target.metadata().capabilities.erases_owned_state;
        for (reason, intent) in DISPOSE_MATRIX {
            if intent == StateIntent::Erase && !erases {
                continue;
            }
            let mut subject = self.create(configuration.clone())?;
            subject.initialize(contexts::scope("initialize")).await?;
            if reason != DisposeReason::InitializationFailure {
                step("start", subject.start(contexts::scope("start"))).await??;
                step(
                    "stop",
                    subject.stop(contexts::scope("stop"), StopMode::Drain),
                )
                .await??;
            }
            step(
                "dispose",
                subject.dispose(contexts::scope("dispose"), reason, intent, 1),
            )
            .await?
            .with_context(|| format!("dispose for {reason:?}/{intent:?} failed"))?;
        }
        Ok(())
    }

    /// `configuration` constructs but cannot initialize (for example an
    /// unreachable dependency). Initialize must fail with a classified error
    /// and leave the object disposable for `InitializationFailure`.
    pub async fn dispose_after_failed_initialize(&self, configuration: Value) -> Result<()> {
        let mut subject = self.create(configuration)?;
        let error = step(
            "initialize",
            subject.initialize(contexts::scope("initialize")),
        )
        .await?
        .err()
        .context("initialize was expected to fail for this configuration")?;
        ensure!(
            error.kind() != LifecycleErrorKind::Unsupported,
            "a failed initialize is retryable or terminal, not unsupported: {error:#}"
        );
        step(
            "dispose",
            subject.dispose(
                contexts::scope("dispose"),
                DisposeReason::InitializationFailure,
                StateIntent::Preserve,
                1,
            ),
        )
        .await?
        .context("an object whose initialize failed must dispose")?;
        Ok(())
    }

    async fn offer_and_receive(
        &self,
        source: &mut dyn SourceSubject,
        probe: &dyn SourceProbe,
    ) -> Result<()> {
        let configuration = source.configuration()?;
        ensure!(
            step("offer", probe.offer(&configuration)).await?? == Offer::Accepted,
            "a running source must accept an offered event"
        );
        ensure!(
            step("next", source.next()).await??,
            "an accepted event must reach next"
        );
        Ok(())
    }

    /// Initialize, start, offer, restart, Drain and Abort stops, and a
    /// preserving dispose. Pause must gate admission or report `unsupported`.
    pub async fn lifecycle(&self, configuration: Value, probe: &dyn SourceProbe) -> Result<()> {
        let mut source = self.create(configuration)?;
        source.initialize(contexts::scope("initialize")).await?;
        source
            .stop(contexts::scope("stop"), StopMode::Drain)
            .await
            .context("stop before start must succeed")?;
        step("start", source.start(contexts::scope("start"))).await??;
        repeated_start(
            source.as_mut(),
            self.target.acknowledges_repeated_transitions(),
        )
        .await?;
        self.offer_and_receive(source.as_mut(), probe).await?;

        match step("pause", source.pause(contexts::scope("pause"))).await? {
            Ok(()) => {
                let configuration = source.configuration()?;
                ensure!(
                    step("offer while paused", probe.offer(&configuration)).await?? == Offer::Refused,
                    "a paused source must refuse new events; return `unsupported` from pause if it cannot"
                );
                step("resume", source.resume(contexts::scope("resume"))).await??;
                self.offer_and_receive(source.as_mut(), probe).await?;
            }
            Err(error) => ensure!(
                error.kind() == LifecycleErrorKind::Unsupported,
                "pause must succeed with real gating or return unsupported: {error:#}"
            ),
        }

        step(
            "drain stop",
            source.stop(contexts::scope("stop"), StopMode::Drain),
        )
        .await??;
        let configuration = source.configuration()?;
        ensure!(
            step("offer while stopped", probe.offer(&configuration)).await?? == Offer::Refused,
            "a stopped source must not accept events"
        );
        step("restart", source.start(contexts::scope("start"))).await??;
        self.offer_and_receive(source.as_mut(), probe).await?;
        step(
            "abort stop",
            source.stop(contexts::scope("stop"), StopMode::Abort),
        )
        .await??;
        step(
            "dispose",
            source.dispose(
                contexts::scope("dispose"),
                DisposeReason::Shutdown,
                StateIntent::Preserve,
                1,
            ),
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
        source.initialize(contexts::scope("initialize")).await?;
        let error = step(
            "cancelled start",
            source.start(contexts::cancelled_scope("start")),
        )
        .await?
        .err()
        .context("a pre-cancelled start must not succeed")?;
        ensure!(
            error.kind() == LifecycleErrorKind::Cancelled,
            "a pre-cancelled start must report cancellation: {error:#}"
        );
        source
            .stop(contexts::scope("stop"), StopMode::Abort)
            .await?;
        step(
            "start after cancellation",
            source.start(contexts::scope("start")),
        )
        .await??;
        self.offer_and_receive(source.as_mut(), probe).await?;
        source
            .stop(contexts::scope("stop"), StopMode::Drain)
            .await?;
        Ok(())
    }

    /// A removal that erases owned state succeeds and is idempotent on retry.
    pub async fn erase(&self, configuration: Value) -> Result<()> {
        ensure!(
            self.target.metadata().capabilities.erases_owned_state,
            "factory does not declare erases_owned_state"
        );
        let mut source = self.create(configuration)?;
        source.initialize(contexts::scope("initialize")).await?;
        step("start", source.start(contexts::scope("start"))).await??;
        step(
            "stop",
            source.stop(contexts::scope("stop"), StopMode::Drain),
        )
        .await??;
        for attempt in [1, 2] {
            step(
                "erase",
                source.dispose(
                    contexts::scope("dispose"),
                    DisposeReason::Removal,
                    StateIntent::Erase,
                    attempt,
                ),
            )
            .await?
            .with_context(|| format!("erase attempt {attempt} failed"))?;
        }
        Ok(())
    }

    /// Construction from `configuration()` reproduces the same configuration,
    /// and published defaults agree with the constructed values.
    pub fn configuration_round_trip(&self, configurations: &[Value]) -> Result<()> {
        let metadata = self.target.metadata();
        let details = self
            .target
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
            self.target.supports_update_planning(),
            "factory does not plan updates"
        );
        for (proposed, expected) in cases {
            let request = self.update_request(current.clone(), proposed.clone());
            let actual = self.target.assess(&request);
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
        let factory = self.linked()?;
        let mut source = Linked(create_source(factory, &self.id, current.clone())?);
        source.initialize(contexts::scope("initialize")).await?;
        step("start", source.start(contexts::scope("start"))).await??;
        let request = self.update_request(current, proposed.clone());
        let assessment = source
            .0
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
                step("pause", source.pause(contexts::scope("pause"))).await??;
                apply(source.0.as_mut(), boundary, &update).await?;
                step("resume", source.resume(contexts::scope("resume"))).await??;
            }
            UpdateBoundary::Stop => {
                step(
                    "stop",
                    source.stop(contexts::scope("stop"), StopMode::Drain),
                )
                .await??;
                apply(source.0.as_mut(), boundary, &update).await?;
                step("restart", source.start(contexts::scope("start"))).await??;
            }
            UpdateBoundary::Replace => anyhow::bail!("Replace is not a retained-object update"),
        }
        let metadata = factory.metadata();
        let details = factory
            .configuration_details()
            .context("configuration details")?;
        let exposed = source.configuration()?;
        ensure!(
            details.equivalent_with_defaults(&metadata.configuration, &proposed, &exposed)?,
            "configuration() must expose the applied configuration: {proposed} vs {exposed}"
        );
        self.offer_and_receive(&mut source, probe).await?;
        source
            .stop(contexts::scope("stop"), StopMode::Drain)
            .await?;
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

/// Runs the standard native-sink scenarios against one target.
pub struct SinkConformance<'a, T = LinkedTarget<'a>> {
    target: T,
    _linked: std::marker::PhantomData<&'a ()>,
    id: ComponentId,
    inputs: Vec<InputBinding>,
}

impl<'a> SinkConformance<'a> {
    /// Runs against a linked factory, including live update application.
    pub fn new(factory: &'a dyn Factory, id: &str, inputs: Vec<InputBinding>) -> Result<Self> {
        Ok(Self {
            target: LinkedTarget(factory),
            _linked: std::marker::PhantomData,
            id: ComponentId::try_new(id)?,
            inputs,
        })
    }
}

impl<'a, T: ConformanceTarget> SinkConformance<'a, T> {
    /// Runs against any target, such as a loaded dynamic library.
    /// [`Self::apply`] needs a linked target.
    pub fn with_target(target: T, id: &str, inputs: Vec<InputBinding>) -> Result<Self> {
        Ok(Self {
            target,
            _linked: std::marker::PhantomData,
            id: ComponentId::try_new(id)?,
            inputs,
        })
    }

    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    fn create(&self, configuration: Value) -> Result<Box<dyn SinkSubject>> {
        self.target
            .create_sink(&self.id, configuration, &self.inputs)
    }

    fn linked(&self) -> Result<&dyn Factory> {
        self.target
            .linked_factory()
            .context("live update application needs the linked factory")
    }

    fn request(&self, configuration: Value) -> CreateRequest {
        let metadata = self.target.metadata();
        CreateRequest {
            id: self.id.clone(),
            implementation: metadata.implementation,
            configuration_version: metadata.configuration_version,
            configuration,
            scope: None,
            inputs: self.inputs.clone(),
            activation_credentials_bound: false,
            update_directive: None,
        }
    }

    fn update_request(&self, current: Value, proposed: Value) -> UpdateRequest {
        UpdateRequest {
            current: self.request(current),
            proposed: self.request(proposed),
        }
    }

    /// Every dispose reason with each intent it permits ends a fresh,
    /// initialized object. Erase runs only when the factory declares it.
    pub async fn dispose_matrix(&self, configuration: Value) -> Result<()> {
        let erases = self.target.metadata().capabilities.erases_owned_state;
        for (reason, intent) in DISPOSE_MATRIX {
            if intent == StateIntent::Erase && !erases {
                continue;
            }
            let mut subject = self.create(configuration.clone())?;
            subject.initialize(contexts::scope("initialize")).await?;
            if reason != DisposeReason::InitializationFailure {
                step("start", subject.start(contexts::scope("start"))).await??;
                step(
                    "stop",
                    subject.stop(contexts::scope("stop"), StopMode::Drain),
                )
                .await??;
            }
            step(
                "dispose",
                subject.dispose(contexts::scope("dispose"), reason, intent, 1),
            )
            .await?
            .with_context(|| format!("dispose for {reason:?}/{intent:?} failed"))?;
        }
        Ok(())
    }

    /// `configuration` constructs but cannot initialize (for example an
    /// unreachable dependency). Initialize must fail with a classified error
    /// and leave the object disposable for `InitializationFailure`.
    pub async fn dispose_after_failed_initialize(&self, configuration: Value) -> Result<()> {
        let mut subject = self.create(configuration)?;
        let error = step(
            "initialize",
            subject.initialize(contexts::scope("initialize")),
        )
        .await?
        .err()
        .context("initialize was expected to fail for this configuration")?;
        ensure!(
            error.kind() != LifecycleErrorKind::Unsupported,
            "a failed initialize is retryable or terminal, not unsupported: {error:#}"
        );
        step(
            "dispose",
            subject.dispose(
                contexts::scope("dispose"),
                DisposeReason::InitializationFailure,
                StateIntent::Preserve,
                1,
            ),
        )
        .await?
        .context("an object whose initialize failed must dispose")?;
        Ok(())
    }

    async fn deliver(
        &self,
        sink: &mut dyn SinkSubject,
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
        sink.initialize(contexts::scope("initialize")).await?;
        sink.stop(contexts::scope("stop"), StopMode::Drain)
            .await
            .context("stop before start must succeed")?;
        step("start", sink.start(contexts::scope("start"))).await??;
        repeated_start(
            sink.as_mut(),
            self.target.acknowledges_repeated_transitions(),
        )
        .await?;
        let mut delivered = 1;
        self.deliver(sink.as_mut(), probe, 1, delivered).await?;
        match step("pause", sink.pause(contexts::scope("pause"))).await? {
            Ok(()) => {
                step("resume", sink.resume(contexts::scope("resume"))).await??;
                delivered += 1;
                self.deliver(sink.as_mut(), probe, 2, delivered).await?;
            }
            Err(error) => ensure!(
                error.kind() == LifecycleErrorKind::Unsupported,
                "pause must succeed or return unsupported: {error:#}"
            ),
        }
        step(
            "drain stop",
            sink.stop(contexts::scope("stop"), StopMode::Drain),
        )
        .await??;
        ensure!(
            step("handle while stopped", sink.handle(probe.input(3)?))
                .await?
                .is_err(),
            "a stopped sink must refuse input"
        );
        step("restart", sink.start(contexts::scope("start"))).await??;
        delivered += 1;
        self.deliver(sink.as_mut(), probe, 4, delivered).await?;
        step(
            "abort stop",
            sink.stop(contexts::scope("stop"), StopMode::Abort),
        )
        .await??;
        step(
            "dispose",
            sink.dispose(
                contexts::scope("dispose"),
                DisposeReason::Shutdown,
                StateIntent::Preserve,
                1,
            ),
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
        sink.initialize(contexts::scope("initialize")).await?;
        step("start", sink.start(contexts::scope("start"))).await??;
        let input = probe.input(1)?;
        let failure = step("failed handle", sink.handle(input.clone()))
            .await?
            .err()
            .context("delivery to an unavailable destination must fail")?;
        ensure!(
            !drasi_computation_contracts::delivery_failure_class(&failure)
                .is_some_and(|class| class.is_permanent()),
            "an unavailable destination is a transient failure the graph retries, not a \
             permanent one: {failure:#}"
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
        sink.stop(contexts::scope("stop"), StopMode::Drain).await?;
        Ok(())
    }

    /// A removal that erases owned state succeeds and is idempotent on retry.
    pub async fn erase(&self, configuration: Value) -> Result<()> {
        ensure!(
            self.target.metadata().capabilities.erases_owned_state,
            "factory does not declare erases_owned_state"
        );
        let mut sink = self.create(configuration)?;
        sink.initialize(contexts::scope("initialize")).await?;
        step("start", sink.start(contexts::scope("start"))).await??;
        step("stop", sink.stop(contexts::scope("stop"), StopMode::Drain)).await??;
        for attempt in [1, 2] {
            step(
                "erase",
                sink.dispose(
                    contexts::scope("dispose"),
                    DisposeReason::Removal,
                    StateIntent::Erase,
                    attempt,
                ),
            )
            .await?
            .with_context(|| format!("erase attempt {attempt} failed"))?;
        }
        Ok(())
    }

    /// Construction from `configuration()` reproduces the same configuration,
    /// and published defaults agree with the constructed values.
    pub fn configuration_round_trip(&self, configurations: &[Value]) -> Result<()> {
        let metadata = self.target.metadata();
        let details = self
            .target
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
            self.target.supports_update_planning(),
            "factory does not plan updates"
        );
        for (proposed, expected) in cases {
            let request = self.update_request(current.clone(), proposed.clone());
            let actual = self.target.assess(&request);
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
        let factory = self.linked()?;
        let mut sink = Linked(create_sink(
            factory,
            &self.id,
            current.clone(),
            &self.inputs,
        )?);
        sink.initialize(contexts::scope("initialize")).await?;
        step("start", sink.start(contexts::scope("start"))).await??;
        self.deliver(&mut sink, probe, 1, 1).await?;
        let request = self.update_request(current, proposed.clone());
        let assessment = sink
            .0
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
                step("pause", sink.pause(contexts::scope("pause"))).await??;
                apply(sink.0.as_mut(), boundary, &update).await?;
                step("resume", sink.resume(contexts::scope("resume"))).await??;
            }
            UpdateBoundary::Stop => {
                step("stop", sink.stop(contexts::scope("stop"), StopMode::Drain)).await??;
                apply(sink.0.as_mut(), boundary, &update).await?;
                step("restart", sink.start(contexts::scope("start"))).await??;
            }
            UpdateBoundary::Replace => anyhow::bail!("Replace is not a retained-object update"),
        }
        let metadata = factory.metadata();
        let details = factory
            .configuration_details()
            .context("configuration details")?;
        let exposed = sink.configuration()?;
        ensure!(
            details.equivalent_with_defaults(&metadata.configuration, &proposed, &exposed)?,
            "configuration() must expose the applied configuration: {proposed} vs {exposed}"
        );
        self.deliver(&mut sink, probe, 2, 2).await?;
        sink.stop(contexts::scope("stop"), StopMode::Drain).await?;
        Ok(())
    }
}
