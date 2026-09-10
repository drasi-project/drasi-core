// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::plugin_services::PluginObservations;
use super::*;
use crate::{
    bootstrap::BootstrapResult,
    channels::{
        BootstrapEventReceiver, ChangeReceiver, ComponentStatus, SourceEvent, SourceEventWrapper,
    },
    config::SourceSubscriptionSettings,
    context::SourceRuntimeContext,
    Source,
};
use async_trait::async_trait;
use std::{
    collections::{BTreeSet, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::watch;

#[derive(Default)]
struct SourceLife {
    initialized: bool,
    initialization_complete: bool,
    running: bool,
    activation_incomplete: bool,
    needs_stop: bool,
    users: BTreeSet<String>,
    subscribed: BTreeSet<String>,
    closed: bool,
    replace_on_start: bool,
}

#[async_trait]
pub trait SourcePluginConstructor: Send + Sync {
    async fn create(&self) -> anyhow::Result<Box<dyn Source>>;
}

struct SharedBootstrap(Arc<dyn crate::bootstrap::BootstrapProvider>);
#[async_trait]
impl crate::bootstrap::BootstrapProvider for SharedBootstrap {
    async fn bootstrap(
        &self,
        request: crate::bootstrap::BootstrapRequest,
        context: &crate::bootstrap::BootstrapContext,
        sender: crate::channels::BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        self.0.bootstrap(request, context, sender, settings).await
    }
}

/// One lifecycle owner for a legacy source, shared by native subscription nodes.
/// Borrowed hosts never initialize, start, stop, or deprovision the legacy source.
pub struct SourcePluginHost {
    id: String,
    source: Mutex<Arc<dyn Source>>,
    constructor: Option<Arc<dyn SourcePluginConstructor>>,
    bootstrap: Mutex<Option<Arc<dyn crate::bootstrap::BootstrapProvider>>>,
    services: LegacyPluginServices,
    owned: bool,
    life: tokio::sync::Mutex<SourceLife>,
    observations: PluginObservations,
    expected_subscriptions: AtomicUsize,
}

impl SourcePluginHost {
    pub fn owned(source: Box<dyn Source>, services: LegacyPluginServices) -> Arc<Self> {
        Self::new(source.into(), services, true)
    }
    pub fn borrowed(source: Arc<dyn Source>) -> Arc<Self> {
        Self::new(source, LegacyPluginServices::empty("borrowed"), false)
    }
    pub async fn recreatable(
        constructor: Arc<dyn SourcePluginConstructor>,
        services: LegacyPluginServices,
    ) -> anyhow::Result<Arc<Self>> {
        let source = constructor.create().await?;
        Ok(Self::new_with_constructor(
            source.into(),
            services,
            true,
            Some(constructor),
        ))
    }
    fn new(source: Arc<dyn Source>, services: LegacyPluginServices, owned: bool) -> Arc<Self> {
        Self::new_with_constructor(source, services, owned, None)
    }
    fn new_with_constructor(
        source: Arc<dyn Source>,
        services: LegacyPluginServices,
        owned: bool,
        constructor: Option<Arc<dyn SourcePluginConstructor>>,
    ) -> Arc<Self> {
        let observations = PluginObservations::new(source.id());
        Arc::new(Self {
            id: source.id().to_owned(),
            source: Mutex::new(source),
            constructor,
            bootstrap: Mutex::new(None),
            services,
            owned,
            life: tokio::sync::Mutex::new(SourceLife::default()),
            observations,
            expected_subscriptions: AtomicUsize::new(1),
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn is_owned(&self) -> bool {
        self.owned
    }
    pub fn source(&self) -> anyhow::Result<Arc<dyn Source>> {
        Ok(self
            .source
            .lock()
            .map_err(|_| anyhow::anyhow!("source instance ownership poisoned"))?
            .clone())
    }
    pub fn services(&self) -> &LegacyPluginServices {
        &self.services
    }
    pub fn bootstrap_resource(&self) -> anyhow::Result<Option<ResourceHandle>> {
        Ok(self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap ownership poisoned"))?
            .as_ref()
            .map(|provider| {
                ResourceHandle::new(
                    ResourceRole::Bootstrap,
                    Arc::new(LegacyBootstrapResource(provider.clone())),
                )
            }))
    }
    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        let handle = ResourceHandle::new(ResourceRole::LegacySource, self.clone());
        if self.owned {
            handle.with_cleanup(self.clone())
        } else {
            handle
        }
    }
    pub fn expected_subscriptions(&self, count: usize) {
        self.expected_subscriptions.store(count, Ordering::Release);
    }
    pub async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn crate::bootstrap::BootstrapProvider>,
    ) -> anyhow::Result<()> {
        if !self.owned {
            anyhow::bail!("cannot mutate a borrowed source's bootstrap provider");
        }
        let life = self.life.lock().await;
        if life.initialized {
            anyhow::bail!("bootstrap provider must be supplied before initialization");
        }
        let provider: Arc<dyn crate::bootstrap::BootstrapProvider> = provider.into();
        *self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap ownership poisoned"))? = Some(provider.clone());
        self.source()?
            .set_bootstrap_provider(Box::new(SharedBootstrap(provider)))
            .await;
        Ok(())
    }
    async fn read_status(&self) -> anyhow::Result<()> {
        self.observations.read().await
    }
    async fn with_observations<T>(
        &self,
        future: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        self.observations.drive(future, false).await
    }
    async fn drive_observations<T>(
        &self,
        future: impl std::future::Future<Output = anyhow::Result<T>>,
        cleanup: bool,
    ) -> anyhow::Result<T> {
        self.observations.drive(future, cleanup).await
    }
    async fn acquire(&self, id: &str) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.closed {
            anyhow::bail!("source host is closed");
        }
        if life.users.contains(id) {
            return Ok(());
        }
        if self.owned {
            if life.replace_on_start {
                let constructor = self
                    .constructor
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("source has no reconstruction factory"))?;
                let source = constructor.create().await?;
                if source.id() != self.id() {
                    anyhow::bail!("source reconstruction changed its identity");
                }
                let bootstrap = self
                    .bootstrap
                    .lock()
                    .map_err(|_| anyhow::anyhow!("bootstrap ownership poisoned"))?
                    .clone();
                if let Some(bootstrap) = bootstrap {
                    source
                        .set_bootstrap_provider(Box::new(SharedBootstrap(bootstrap)))
                        .await;
                }
                *self
                    .source
                    .lock()
                    .map_err(|_| anyhow::anyhow!("source instance ownership poisoned"))? =
                    source.into();
                life.initialized = false;
                life.initialization_complete = false;
                life.replace_on_start = false;
            }
            if life.initialized && !life.initialization_complete {
                anyhow::bail!("source initialization was interrupted; replace the source host");
            }
            if !life.initialized {
                life.initialized = true;
                life.needs_stop = true;
                let sender = self.observations.channel().await;
                let mut context = SourceRuntimeContext::new(
                    self.services.scope.as_ref(),
                    self.id(),
                    self.services.state_store.clone(),
                    sender,
                    self.services.identity.clone(),
                );
                context.wal_provider = self.services.wal.clone();
                let source = self.source()?;
                self.with_observations(async {
                    source.initialize(context).await;
                    Ok(())
                })
                .await?;
                life.initialization_complete = true;
            }
            if !life.running {
                if life.activation_incomplete {
                    anyhow::bail!("source has an incomplete prior activation");
                }
                life.needs_stop = true;
                life.activation_incomplete = true;
                self.observations.reset().await;
                self.with_observations(self.source()?.start()).await?;
                life.running = true;
                life.activation_incomplete = false;
            }
        }
        life.users.insert(id.to_owned());
        Ok(())
    }
    async fn subscribed(&self, id: &str) -> anyhow::Result<()> {
        let notify = {
            let mut life = self.life.lock().await;
            life.subscribed.insert(id.to_owned());
            self.owned
                && life.subscribed.len() == self.expected_subscriptions.load(Ordering::Acquire)
        };
        if notify {
            self.source()?.on_subscriptions_complete().await;
        }
        Ok(())
    }
    async fn release(&self, id: &str) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        life.users.remove(id);
        life.subscribed.remove(id);
        if self.owned && life.users.is_empty() && life.needs_stop {
            self.drive_observations(self.source()?.stop(), true).await?;
            life.running = false;
            life.needs_stop = false;
            life.activation_incomplete = false;
            life.replace_on_start = self.constructor.is_some();
        }
        Ok(())
    }
    async fn unavailable(&self) -> anyhow::Result<()> {
        if self.owned {
            return self.observations.failure().await;
        }
        loop {
            if self.source()?.status().await == ComponentStatus::Error {
                anyhow::bail!("borrowed source {} is unavailable", self.id());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    pub async fn deprovision_owned(&self) -> anyhow::Result<()> {
        if !self.owned {
            anyhow::bail!("cannot deprovision a borrowed source");
        }
        self.shutdown().await?;
        self.source()?.deprovision().await
    }
}
#[async_trait]
impl ResourceCleanup for SourcePluginHost {
    async fn shutdown(&self) -> anyhow::Result<()> {
        if !self.owned {
            return Ok(());
        }
        let mut life = self.life.lock().await;
        life.closed = true;
        if !life.users.is_empty() {
            anyhow::bail!("source still has active graph subscriptions");
        }
        if life.needs_stop {
            self.drive_observations(self.source()?.stop(), true).await?;
            life.needs_stop = false;
            life.running = false;
        }
        self.observations.close().await;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceSubscriptionOptions {
    #[serde(default)]
    pub enable_bootstrap: bool,
    #[serde(default)]
    pub nodes: HashSet<String>,
    #[serde(default)]
    pub relations: HashSet<String>,
    /// Explicit permission to use a borrowed plugin's bootstrap/replay contract.
    /// Keep false unless that plugin supports isolated additional subscriptions.
    #[serde(default)]
    pub borrowed_recovery: bool,
    /// Broadcast delivery is explicitly lossy before this adapter's output pipe.
    #[serde(default)]
    pub allow_broadcast_loss: bool,
    #[serde(default = "bootstrap_timeout")]
    pub bootstrap_timeout_secs: u64,
}
fn bootstrap_timeout() -> u64 {
    300
}
impl Default for SourceSubscriptionOptions {
    fn default() -> Self {
        Self {
            enable_bootstrap: false,
            nodes: HashSet::new(),
            relations: HashSet::new(),
            borrowed_recovery: false,
            allow_broadcast_loss: false,
            bootstrap_timeout_secs: bootstrap_timeout(),
        }
    }
}

#[derive(Clone)]
enum SubscriptionPhase {
    New,
    Waiting,
    Reset { generation: u64, volatile: bool },
    Ready,
    Failed(Arc<str>),
    Closed,
}
struct SubscriptionState {
    bootstrap: Option<BootstrapEventReceiver>,
    result: Option<tokio::sync::oneshot::Receiver<anyhow::Result<BootstrapResult>>>,
    position: Option<Arc<AtomicU64>>,
    requested_position: bool,
    generation: u64,
}

pub struct LegacySourceSubscription {
    host: Arc<SourcePluginHost>,
    options: SourceSubscriptionOptions,
    stream: StreamId,
    id: String,
    progress: Option<Arc<QuerySourceProgress>>,
    phase: watch::Sender<SubscriptionPhase>,
    state: Mutex<SubscriptionState>,
}

impl LegacySourceSubscription {
    pub fn new(
        host: Arc<SourcePluginHost>,
        options: SourceSubscriptionOptions,
        stream: StreamId,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<Arc<Self>> {
        if options.bootstrap_timeout_secs == 0
            || tokio::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(
                    options.bootstrap_timeout_secs,
                ))
                .is_none()
        {
            anyhow::bail!("bootstrap timeout must be nonzero and representable");
        }
        if host.source()?.dispatch_mode() == crate::DispatchMode::Broadcast
            && !options.allow_broadcast_loss
        {
            anyhow::bail!("broadcast source requires explicit source-side loss permission");
        }
        if !host.owned && options.enable_bootstrap && !options.borrowed_recovery {
            anyhow::bail!(
                "borrowed source bootstrap requires explicit isolated-subscription permission"
            );
        }
        Ok(Arc::new(Self {
            host,
            options,
            stream,
            id: format!("computation:{}", uuid::Uuid::new_v4()),
            progress,
            phase: watch::channel(SubscriptionPhase::New).0,
            state: Mutex::new(SubscriptionState {
                bootstrap: None,
                result: None,
                position: None,
                requested_position: false,
                generation: 0,
            }),
        }))
    }
    pub fn host(&self) -> &Arc<SourcePluginHost> {
        &self.host
    }
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }
    fn checkpoint(
        &self,
        view: &SourceProgressSnapshot,
    ) -> Option<drasi_core::interface::SourceCheckpoint> {
        [
            SourceProgressKey::Source(self.host.id().to_owned()),
            SourceProgressKey::Stream(self.stream.clone()),
        ]
        .iter()
        .filter_map(|key| view.checkpoints.get(key))
        .max_by_key(|checkpoint| checkpoint.sequence)
        .cloned()
    }
    fn confirm_position(&self) -> anyhow::Result<()> {
        if let Some(progress) = &self.progress {
            let view = progress.snapshot();
            if view.persistent {
                if let Some(checkpoint) = self.checkpoint(&view) {
                    if let Some(handle) = &self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                        .position
                    {
                        handle.store(checkpoint.sequence, Ordering::Release);
                    }
                }
            }
        }
        Ok(())
    }
    async fn reset(&self, view: &SourceProgressSnapshot, volatile: bool) -> anyhow::Result<()> {
        if view.ready {
            return Err(QueryRecoveryError::SourceResetRequired.into());
        }
        let progress = self
            .progress
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("source reset requires query progress coordination"))?;
        self.phase.send_replace(SubscriptionPhase::Reset {
            generation: view.reset_generation,
            volatile,
        });
        let mut changes = progress.subscribe();
        let next = changes
            .wait_for(|next| {
                next.failure.is_some()
                    || next.recovered && next.reset_generation > view.reset_generation
            })
            .await?;
        if let Some(error) = &next.failure {
            anyhow::bail!("{error}");
        }
        self.phase.send_replace(SubscriptionPhase::Waiting);
        Ok(())
    }
    async fn subscribe(&self) -> anyhow::Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        self.phase.send_replace(SubscriptionPhase::Waiting);
        let result = async {
            loop {
                let view = if let Some(progress) = &self.progress {
                    progress.wait_recovered().await?
                } else {
                    Arc::new(SourceProgressSnapshot::default())
                };
                let saved = self.checkpoint(&view);
                if !view.ready
                    && view.bootstrap_complete
                    && self.options.enable_bootstrap
                    && !view.persistent
                {
                    self.reset(&view, true).await?;
                    continue;
                }
                let source = self.host.source()?;
                if source.dispatch_mode() == crate::DispatchMode::Broadcast
                    && !self.options.allow_broadcast_loss
                {
                    anyhow::bail!(
                        "source reconstruction changed to an undeclared lossy dispatch mode"
                    );
                }
                let recoverable = source.supports_replay()
                    && source.dispatch_mode() == crate::DispatchMode::Channel
                    && (self.host.owned || self.options.borrowed_recovery);
                if !view.ready
                    && view.persistent
                    && view.bootstrap_complete
                    && (!recoverable
                        || saved
                            .as_ref()
                            .and_then(|saved| saved.source_position.as_ref())
                            .is_none())
                {
                    self.reset(&view, false).await?;
                    continue;
                }
                let settings = SourceSubscriptionSettings {
                    source_id: self.host.id().to_owned(),
                    query_id: self.id.clone(),
                    nodes: self.options.nodes.clone(),
                    relations: self.options.relations.clone(),
                    enable_bootstrap: self.options.enable_bootstrap && !view.bootstrap_complete,
                    resume_from: if view.persistent && recoverable {
                        saved
                            .as_ref()
                            .and_then(|saved| saved.source_position.clone())
                    } else {
                        None
                    },
                    // A sequence floor is not positional replay. Recreated
                    // volatile sources must not reuse committed raw sequences.
                    resume_sequence: if self.host.owned || self.options.borrowed_recovery {
                        saved.as_ref().map(|saved| saved.sequence)
                    } else {
                        None
                    },
                    request_position_handle: view.persistent && recoverable,
                };
                self.state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                    .requested_position = settings.request_position_handle;
                let response = match source.subscribe(settings).await {
                    Ok(response) => response,
                    Err(error)
                        if error
                            .downcast_ref::<crate::sources::SourceError>()
                            .is_some_and(|error| {
                                matches!(
                                    error,
                                    crate::sources::SourceError::PositionUnavailable { .. }
                                )
                            }) =>
                    {
                        self.reset(&view, false).await?;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if response.source_id != self.host.id() || response.query_id != self.id {
                    anyhow::bail!("source returned a subscription for another source/query");
                }
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?;
                    if response.position_handle.is_some() && !state.requested_position {
                        anyhow::bail!("source supplied unrequested position ownership");
                    }
                    state.bootstrap = response.bootstrap_receiver;
                    state.result = response.bootstrap_result_receiver;
                    state.position = response.position_handle;
                    state.generation = view.reset_generation;
                }
                self.confirm_position()?;
                self.phase.send_replace(SubscriptionPhase::Ready);
                self.host.subscribed(&self.id).await?;
                return Ok(response.receiver);
            }
        }
        .await;
        if let Err(error) = &result {
            self.phase
                .send_replace(SubscriptionPhase::Failed(Arc::from(format!("{error:#}"))));
        }
        result
    }
    async fn close(&self) -> anyhow::Result<()> {
        self.phase.send_replace(SubscriptionPhase::Closed);
        self.clear_subscription().await?;
        self.host.release(&self.id).await
    }
    async fn clear_subscription(&self) -> anyhow::Result<()> {
        let requested = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?;
            state.bootstrap = None;
            state.result = None;
            state.position = None;
            std::mem::take(&mut state.requested_position)
        };
        if requested {
            self.host.source()?.remove_position_handle(&self.id).await;
        }
        Ok(())
    }
    async fn preparation(&self) -> anyhow::Result<BootstrapPreparation> {
        let mut phase = self.phase.subscribe();
        tokio::time::timeout(
            std::time::Duration::from_secs(self.options.bootstrap_timeout_secs),
            async {
                loop {
                    let current = phase.borrow_and_update().clone();
                    let generation = self
                        .progress
                        .as_ref()
                        .map(|progress| progress.snapshot().reset_generation);
                    let binding_generation = self
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                        .generation;
                    match current {
                        SubscriptionPhase::Ready
                            if generation
                                .map_or(true, |generation| binding_generation == generation) =>
                        {
                            return Ok(BootstrapPreparation::Ready)
                        }
                        SubscriptionPhase::Reset {
                            generation: reset,
                            volatile,
                        } if generation == Some(reset) => {
                            return Ok(if volatile {
                                BootstrapPreparation::RefreshVolatile
                            } else {
                                BootstrapPreparation::ResetRequired
                            });
                        }
                        SubscriptionPhase::Failed(error) => anyhow::bail!("{error}"),
                        _ => {}
                    }
                    phase.changed().await?;
                }
            },
        )
        .await?
    }
}

pub struct SourcePluginAdapter {
    descriptor: ComponentDescriptor,
    subscription: Arc<LegacySourceSubscription>,
    live: Option<Box<dyn ChangeReceiver<SourceEventWrapper>>>,
    sequence: u64,
}
impl SourcePluginAdapter {
    pub fn new(id: ComponentId, subscription: Arc<LegacySourceSubscription>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                id,
                vec![PortDescriptor::new(
                    PortId::try_new("out").expect("port"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source descriptor"),
            subscription,
            live: None,
            sequence: 0,
        }
    }
}
#[async_trait]
impl ComputationComponent for SourcePluginAdapter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.subscription
            .phase
            .send_replace(SubscriptionPhase::Waiting);
        let result = self.subscription.host.acquire(&self.subscription.id).await;
        if let Err(error) = &result {
            self.subscription
                .phase
                .send_replace(SubscriptionPhase::Failed(Arc::from(format!("{error:#}"))));
        }
        result
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.live = None;
        self.subscription.close().await
    }
}
#[async_trait]
impl EnvelopeSource for SourcePluginAdapter {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let mut progress = self
            .subscription
            .progress
            .as_ref()
            .map(|progress| progress.subscribe());
        loop {
            if self.live.is_none() {
                self.live = Some(self.subscription.subscribe().await?);
            }
            if let Some(progress) = &mut progress {
                let view = progress.borrow_and_update().clone();
                if let Some(error) = &view.failure {
                    anyhow::bail!("{error}");
                }
                if self
                    .subscription
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                    .generation
                    != view.reset_generation
                {
                    self.live = None;
                    self.subscription.clear_subscription().await?;
                    continue;
                }
                if !view.ready {
                    tokio::select! {
                        error = self.subscription.host.unavailable() => { error?; unreachable!() },
                        update = self.subscription.host.read_status() => update?,
                        result = progress.changed() => result?,
                    }
                    continue;
                }
            }
            self.subscription.confirm_position()?;
            let receiver = self.live.as_mut().expect("subscribed");
            let event = tokio::select! {
                biased;
                error = self.subscription.host.unavailable() => { error?; unreachable!() },
                update = self.subscription.host.read_status() => { update?; continue; },
                _ = async {
                    if let Some(progress) = &mut progress { let _ = progress.changed().await; }
                    else { std::future::pending::<()>().await; }
                } => continue,
                event = receiver.recv() => event?,
            };
            if matches!(
                &event.event,
                SourceEvent::Control(crate::channels::SourceControl::Subscription { .. })
            ) {
                log::trace!("Ignoring legacy subscription control on native data input");
                continue;
            }
            if self
                .subscription
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                .position
                .is_some()
                && event.sequence.is_none()
            {
                anyhow::bail!("position-tracked source omitted its authoritative sequence");
            }
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("source adapter sequence exhausted"))?;
            let envelope = GraphChangeCodec::encode_source_event(
                event,
                self.descriptor.id(),
                self.subscription.stream.clone(),
                sequence,
                self.subscription.host.source()?.describe_schema(),
            )?;
            self.sequence = sequence;
            return Ok(Some(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope,
            }));
        }
    }
}

/// One query's subscription-backed bootstrap provider. No legacy QueryManager is used.
pub struct LegacySourceBootstrap {
    subscriptions: Vec<Arc<LegacySourceSubscription>>,
}
impl LegacySourceBootstrap {
    pub fn new(subscriptions: Vec<Arc<LegacySourceSubscription>>) -> Arc<Self> {
        Arc::new(Self { subscriptions })
    }
}
#[async_trait]
impl ComputationBootstrapProvider for LegacySourceBootstrap {
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        let mut result = BootstrapPreparation::Ready;
        for subscription in &self.subscriptions {
            match subscription.preparation().await? {
                BootstrapPreparation::ResetRequired => {
                    return Ok(BootstrapPreparation::ResetRequired)
                }
                BootstrapPreparation::RefreshVolatile => {
                    result = BootstrapPreparation::RefreshVolatile
                }
                BootstrapPreparation::Ready => {}
            }
        }
        Ok(result)
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let mut streams = Vec::new();
        for subscription in &self.subscriptions {
            let receiver = subscription
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                .bootstrap
                .take();
            if let Some(receiver) = receiver {
                let source = subscription.host.id().to_owned();
                let stream = StreamId::try_new(format!("{}/bootstrap", subscription.stream))?;
                let deadline = tokio::time::Instant::now()
                    + std::time::Duration::from_secs(subscription.options.bootstrap_timeout_secs);
                streams.push(Box::pin(futures::stream::try_unfold(
                    (receiver, source, stream, deadline),
                    |(mut receiver, source, stream, deadline)| async move {
                        let Some(event) =
                            tokio::time::timeout_at(deadline, receiver.recv()).await?
                        else {
                            return Ok(None);
                        };
                        if event.source_id != source {
                            anyhow::bail!("bootstrap returned another source's data");
                        }
                        let sequence = event
                            .sequence
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("bootstrap ordinal exhausted"))?;
                        let envelope = GraphChangeCodec::encode_change(
                            event.change,
                            stream.clone(),
                            sequence,
                            Some(event.timestamp),
                        )?;
                        Ok(Some((envelope, (receiver, source, stream, deadline))))
                    },
                ))
                    as std::pin::Pin<
                        Box<dyn futures::Stream<Item = anyhow::Result<ChangeEnvelope>> + Send>,
                    >);
            }
        }
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::select_all(streams)),
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        let mut watermarks = Vec::new();
        for subscription in &self.subscriptions {
            let receiver = subscription
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("subscription ownership poisoned"))?
                .result
                .take();
            if let Some(receiver) = receiver {
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(subscription.options.bootstrap_timeout_secs),
                    receiver,
                )
                .await???;
                watermarks.push(BootstrapWatermark {
                    stream: subscription.stream.clone(),
                    source_id: Some(subscription.host.id().to_owned()),
                    sequence: 0,
                    position: result.source_position,
                });
            }
        }
        Ok(watermarks)
    }
}

#[async_trait]
impl ResourceCleanup for LegacySourceSubscription {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.close().await
    }
}

pub struct SourcePluginAdapterFactory {
    descriptor: FactoryDescriptor,
}
impl Default for SourcePluginAdapterFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    "drasi/source-plugin-subscription",
                    "1",
                )
                .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: std::collections::BTreeMap::from([(
                        Arc::from("stream"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: [
                    (
                        Arc::from("source"),
                        ResourceRequirement::exactly_one::<SourcePluginHost>(
                            ResourceRole::LegacySource,
                        ),
                    ),
                    (
                        Arc::from("subscription"),
                        ResourceRequirement::exactly_one::<LegacySourceSubscription>(
                            ResourceRole::SourceSubscription,
                        ),
                    ),
                    (
                        Arc::from("progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                    (
                        Arc::from("bootstrap"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<LegacyBootstrapResource>(
                                ResourceRole::Bootstrap,
                            )
                        },
                    ),
                ]
                .into_iter()
                .chain(LegacyPluginServices::requirements())
                .collect(),
            },
        }
    }
}
#[async_trait]
impl ComponentFactory for SourcePluginAdapterFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = ComponentDescriptor::try_new(
            spec.descriptor.id().clone(),
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        if spec.descriptor != expected {
            anyhow::bail!("source adapter requires typed graph output");
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid source stream"))?,
            )?;
        }
        Ok(())
    }
    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &std::collections::BTreeMap<ResourceId, ResourceSpecification>,
        resources: &std::collections::BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        if let Some(id) = spec.dependencies.get("source").and_then(|ids| ids.first()) {
            if let Some(resource) = resources.get(id) {
                let host = resource.get::<SourcePluginHost>()?;
                if host.owned != (declarations[id].ownership == ResourceOwnership::Graph) {
                    anyhow::bail!("source host ownership differs from its declaration");
                }
                if host.owned && !resource.has_cleanup() {
                    anyhow::bail!("owned source host requires cleanup ownership");
                }
                host.services.validate_bindings(spec, resources)?;
                let bootstrap = host
                    .bootstrap
                    .lock()
                    .map_err(|_| anyhow::anyhow!("bootstrap ownership poisoned"))?;
                super::plugin_services::validate_service(
                    spec,
                    resources,
                    "bootstrap",
                    bootstrap.as_ref(),
                    |value: &LegacyBootstrapResource| &value.0,
                )?;
                if let Some(subscription) = spec
                    .dependencies
                    .get("subscription")
                    .and_then(|ids| ids.first())
                    .and_then(|id| resources.get(id))
                {
                    let subscription = subscription.get::<LegacySourceSubscription>()?;
                    if !Arc::ptr_eq(&host, &subscription.host) {
                        anyhow::bail!("source subscription belongs to another host");
                    }
                    if let Some(ConfigurationValue::Literal(value)) =
                        spec.configuration.get("stream")
                    {
                        if value.as_str() != Some(subscription.stream.as_str()) {
                            anyhow::bail!(
                                "source subscription stream differs from its specification"
                            );
                        }
                    }
                    super::plugin_services::validate_service(
                        spec,
                        resources,
                        "progress",
                        subscription.progress.as_ref(),
                        |value: &QuerySourceProgressResource| &value.0,
                    )?;
                }
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let source = context
            .resources::<SourcePluginHost>("source")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing source host"))
            })?;
        let subscription = context
            .resources::<LegacySourceSubscription>("subscription")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing source subscription"))
            })?;
        if !Arc::ptr_eq(&source, &subscription.host)
            || context
                .configuration()
                .get("stream")
                .and_then(serde_json::Value::as_str)
                != Some(subscription.stream.as_str())
        {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "source subscription binding mismatch"
            )));
        }
        Ok(ConstructedComponent::source(Box::new(
            SourcePluginAdapter::new(context.component_id, subscription),
        )))
    }
}
