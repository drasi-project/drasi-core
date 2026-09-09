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

use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{
    channels::{ChangeReceiver, ComponentStatus, DispatchMode, SourceEventWrapper},
    component_graph::ComponentUpdate,
    config::SourceSubscriptionSettings,
    context::SourceRuntimeContext,
    schema::SourceSchema,
    Source,
};

use super::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentId, ComponentRole,
    ComponentSpecification, ComputationComponent, ConfigurationField, ConfigurationSchema,
    ConfigurationType, ConfigurationValue, ConstructedComponent, ConstructionContext,
    EnvelopeSource, FactoryDescriptor, GraphChangeCodec, ImplementationIdentity, OutputEnvelope,
    PipeRequirements, PortDescriptor, PortDirection, PortId, ResourceCleanup, ResourceHandle,
    ResourceId, ResourceOwnership, ResourceRequirement, ResourceRole, ResourceSpecification,
    StreamId,
};

/// Explicit external source ownership. Borrowing never initializes, starts,
/// stops, rewinds, or deprovisions the existing Source or its other subscribers.
pub struct LegacySourceResource {
    source: Arc<dyn Source>,
    owned: bool,
    initialized: AtomicBool,
    needs_stop: AtomicBool,
}

impl LegacySourceResource {
    pub fn borrowed(source: Arc<dyn Source>) -> Self {
        Self {
            source,
            owned: false,
            initialized: AtomicBool::new(false),
            needs_stop: AtomicBool::new(false),
        }
    }

    /// Transfer a fresh, uninitialized, exclusive Source instance to the graph.
    /// This basic adapter supplies no implicit identity/state/WAL/bootstrap service.
    pub fn owned(source: Box<dyn Source>) -> Self {
        Self {
            source: source.into(),
            owned: true,
            initialized: AtomicBool::new(false),
            needs_stop: AtomicBool::new(false),
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if self.owned && self.needs_stop.load(Ordering::Acquire) {
            self.source.stop().await?;
            self.needs_stop.store(false, Ordering::Release);
        }
        Ok(())
    }
}

#[async_trait]
impl ResourceCleanup for LegacySourceResource {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop().await
    }
}

fn descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("out").expect("constant port"),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("one valid graph output")
}

fn settings(source: &dyn Source) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source.id().to_owned(),
        query_id: format!("computation:{}", uuid::Uuid::new_v4()),
        enable_bootstrap: false,
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from: None,
        resume_sequence: None,
        request_position_handle: false,
    }
}

async fn subscribe(
    source: &dyn Source,
) -> anyhow::Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
    let response = source.subscribe(settings(source)).await?;
    if response.bootstrap_receiver.is_some()
        || response.bootstrap_result_receiver.is_some()
        || response.position_handle.is_some()
    {
        anyhow::bail!("live legacy adapter requires an isolated subscription without bootstrap or position ownership");
    }
    Ok(response.receiver)
}

fn observe(source_id: &str, update: ComponentUpdate) -> anyhow::Result<()> {
    match update {
        ComponentUpdate::Status {
            component_id,
            status,
            message,
        } => {
            if component_id != source_id {
                anyhow::bail!("legacy source reported another component's status");
            }
            if status == ComponentStatus::Error {
                anyhow::bail!(
                    "legacy source is unavailable: {}",
                    message.as_deref().unwrap_or("unspecified source failure")
                );
            }
            log::trace!("Legacy computation source observation: {source_id} {status:?}");
        }
    }
    Ok(())
}

async fn with_observations<T>(
    source_id: &str,
    updates: &mut Option<mpsc::Receiver<ComponentUpdate>>,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    drive_observations(source_id, updates, future, false).await
}

async fn drive_observations<T>(
    source_id: &str,
    updates: &mut Option<mpsc::Receiver<ComponentUpdate>>,
    future: impl Future<Output = anyhow::Result<T>>,
    cleanup: bool,
) -> anyhow::Result<T> {
    tokio::pin!(future);
    let mut observation_failure = None;
    loop {
        if let Some(receiver) = updates {
            tokio::select! {
                result = &mut future => return observed_result(result, observation_failure),
                update = receiver.recv() => match update {
                    Some(update) => {
                        if let Err(error) = observe(source_id, update) {
                            if !cleanup { return Err(error); }
                            observation_failure = Some(error);
                        }
                    },
                    None => {
                        log::trace!("Legacy source {source_id} does not retain the optional status channel");
                        *updates = None;
                    },
                }
            }
        } else {
            return observed_result(future.await, observation_failure);
        }
    }
}

fn observed_result<T>(
    result: anyhow::Result<T>,
    observation: Option<anyhow::Error>,
) -> anyhow::Result<T> {
    match (result, observation) {
        (Ok(_), Some(error)) => Err(error),
        (Err(error), Some(observation)) => {
            Err(error.context(format!("legacy status also reported: {observation:#}")))
        }
        (result, None) => result,
    }
}

/// An explicit graph-only adapter for a compatible live Channel subscription.
/// SourceBase may retain a closed dispatcher entry after receiver drop; the
/// adapter does not claim to remove that source-owned registration.
pub struct LegacySourceAdapter {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    resource: Arc<LegacySourceResource>,
    receiver: Option<Box<dyn ChangeReceiver<SourceEventWrapper>>>,
    updates: Option<mpsc::Receiver<ComponentUpdate>>,
    sequence: u64,
    schema: Option<SourceSchema>,
}

impl LegacySourceAdapter {
    pub async fn bind(
        graph_id: &str,
        id: ComponentId,
        stream: StreamId,
        resource: Arc<LegacySourceResource>,
    ) -> anyhow::Result<Self> {
        if resource.source.dispatch_mode() != DispatchMode::Channel {
            anyhow::bail!("legacy adapter requires an isolated Channel subscription; Broadcast would hide source-side loss");
        }
        let mut updates = None;
        if resource.owned {
            if resource.initialized.swap(true, Ordering::AcqRel) {
                anyhow::bail!("owned legacy source was already initialized; reconstruct it instead of reinitializing another pipeline");
            }
            resource.needs_stop.store(true, Ordering::Release);
            let (sender, receiver) = mpsc::channel(64);
            updates = Some(receiver);
            let context = SourceRuntimeContext::new(
                format!("computation:{graph_id}"),
                resource.source.id(),
                None,
                sender,
                None,
            );
            with_observations(resource.source.id(), &mut updates, async {
                resource.source.initialize(context).await;
                Ok(())
            })
            .await?;
        }
        let receiver = with_observations(
            resource.source.id(),
            &mut updates,
            subscribe(resource.source.as_ref()),
        )
        .await?;
        Ok(Self {
            descriptor: descriptor(id),
            stream,
            schema: resource.source.describe_schema(),
            resource,
            receiver: Some(receiver),
            updates,
            sequence: 0,
        })
    }
}

#[async_trait]
impl ComputationComponent for LegacySourceAdapter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        if let Some(updates) = &mut self.updates {
            while let Ok(update) = updates.try_recv() {
                log::trace!(
                    "Discarding prior-operation legacy source status observation: {update:?}"
                );
            }
        }
        if self.receiver.is_none() {
            self.receiver = Some(
                with_observations(
                    self.resource.source.id(),
                    &mut self.updates,
                    subscribe(self.resource.source.as_ref()),
                )
                .await?,
            );
        }
        if self.resource.owned {
            self.resource.needs_stop.store(true, Ordering::Release);
            with_observations(
                self.resource.source.id(),
                &mut self.updates,
                self.resource.source.start(),
            )
            .await?;
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.receiver = None;
        drive_observations(
            self.resource.source.id(),
            &mut self.updates,
            self.resource.stop(),
            true,
        )
        .await
    }
}

#[async_trait]
impl EnvelopeSource for LegacySourceAdapter {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("legacy source adapter is not subscribed"))?;
        let event = with_observations(
            self.resource.source.id(),
            &mut self.updates,
            receiver.recv(),
        )
        .await?;
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("adapter producer sequence exhausted"))?;
        let envelope = GraphChangeCodec::encode_source_event(
            event,
            self.descriptor.id(),
            self.stream.clone(),
            sequence,
            self.schema.clone(),
        )?;
        self.sequence = sequence;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}

pub struct LegacySourceFactory {
    descriptor: FactoryDescriptor,
}

impl Default for LegacySourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/legacy-source-adapter", "1")
                    .expect("constant implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("stream"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("source"),
                    ResourceRequirement::exactly_one::<LegacySourceResource>(
                        ResourceRole::LegacySource,
                    ),
                )]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for LegacySourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        if spec.descriptor != descriptor(spec.descriptor.id().clone()) {
            anyhow::bail!(
                "legacy source adapter requires its typed graph-change output descriptor"
            );
        }
        if let Some(ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid stream"))?,
            )?;
        }
        Ok(())
    }

    fn validate_resources(
        &self,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        for id in spec.dependencies.get("source").into_iter().flatten() {
            if let Some(handle) = resources.get(id) {
                let resource = handle.get::<LegacySourceResource>()?;
                if resource.owned != (declarations[id].ownership == ResourceOwnership::Graph) {
                    anyhow::bail!("legacy source lifecycle ownership must match its declared resource ownership");
                }
                if resource.owned && !handle.has_cleanup() {
                    anyhow::bail!(
                        "owned legacy source requires its explicit resource cleanup owner"
                    );
                }
                if resource.source.dispatch_mode() != DispatchMode::Channel {
                    anyhow::bail!("legacy source adapter requires isolated Channel subscriptions");
                }
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let source = context
            .resources::<LegacySourceResource>("source")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing source")))?;
        let stream = context
            .configuration()
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing stream")))?;
        let stream = StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?;
        let adapter =
            LegacySourceAdapter::bind(&context.graph_id, context.component_id, stream, source)
                .await
                .map_err(ComponentCreationError::retryable)?;
        Ok(ConstructedComponent::source(Box::new(adapter)))
    }
}
