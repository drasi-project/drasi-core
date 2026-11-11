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

use super::component::RuntimeComponent;
use crate::{computation::v1::*, metrics::ReactionMetrics, ComponentStatus, Reaction};
use async_trait::async_trait;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, Weak,
    },
};
use tokio::sync::{watch, Mutex};

pub(super) struct ReactionInstance {
    pub reaction: Arc<dyn Reaction>,
    pending: Option<Arc<super::reaction_creation::DeferredReaction>>,
    metrics: StdMutex<BTreeMap<String, Arc<ReactionMetrics>>>,
    host: StdMutex<Option<Arc<ReactionPluginHost>>>,
    initializing: Mutex<()>,
    initialized: AtomicBool,
    closed: AtomicBool,
    owner: Weak<super::Runtime>,
    inputs: Mutex<Vec<(String, CatalogSubscription)>>,
    changed: watch::Sender<u64>,
}
impl ReactionInstance {
    pub(super) fn new(reaction: Box<dyn Reaction>, owner: &Arc<super::Runtime>) -> Arc<Self> {
        let reaction: Arc<dyn Reaction> = reaction.into();
        Self::new_inner(reaction, None, owner)
    }

    pub(super) fn deferred(
        pending: Arc<super::reaction_creation::DeferredReaction>,
        owner: &Arc<super::Runtime>,
    ) -> Arc<Self> {
        Self::new_inner(pending.clone(), Some(pending), owner)
    }

    fn new_inner(
        reaction: Arc<dyn Reaction>,
        pending: Option<Arc<super::reaction_creation::DeferredReaction>>,
        owner: &Arc<super::Runtime>,
    ) -> Arc<Self> {
        let query_ids = reaction.query_ids();
        let metrics: BTreeMap<_, _> = query_ids
            .iter()
            .cloned()
            .map(|id| (id, Arc::new(ReactionMetrics::new())))
            .collect();
        Arc::new(Self {
            reaction,
            pending,
            metrics: StdMutex::new(metrics),
            host: StdMutex::new(None),
            initializing: Mutex::new(()),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            owner: Arc::downgrade(owner),
            inputs: Mutex::new(Vec::new()),
            changed: owner.changed.clone(),
        })
    }
    pub(super) fn query_ids(&self) -> Vec<String> {
        self.reaction.query_ids()
    }

    pub(super) fn references(&self) -> anyhow::Result<Vec<crate::config::ComponentReference>> {
        if let Some(pending) = &self.pending {
            return Ok(pending.references());
        }
        self.query_ids()
            .into_iter()
            .map(|id| {
                Ok(crate::config::ComponentReference::by_id(
                    ComponentId::try_new(id)?,
                ))
            })
            .collect()
    }

    pub(super) fn bind_committed(
        &self,
        inputs: &[crate::config::ComponentReference],
    ) -> anyhow::Result<()> {
        if let Some(pending) = &self.pending {
            pending.bind_committed(inputs)?;
        } else {
            anyhow::ensure!(
                self.references()? == inputs,
                "reaction object inputs differ from its declaration"
            );
        }
        Ok(())
    }

    pub(super) fn metrics(&self) -> anyhow::Result<BTreeMap<String, Arc<ReactionMetrics>>> {
        Ok(self
            .metrics
            .lock()
            .map_err(|_| anyhow::anyhow!("reaction metrics ownership poisoned"))?
            .clone())
    }
    fn current_host(&self) -> anyhow::Result<Option<Arc<ReactionPluginHost>>> {
        Ok(self
            .host
            .lock()
            .map_err(|_| anyhow::anyhow!("reaction host ownership poisoned"))?
            .clone())
    }
    fn host(&self) -> anyhow::Result<Arc<ReactionPluginHost>> {
        self.current_host()?.ok_or_else(|| {
            anyhow::anyhow!("Reaction '{}' has not been created", self.reaction.id())
        })
    }
    fn notify(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub(super) async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        RuntimeComponent::stop(self).await?;
        self.reaction.delete_owned_resources().await
    }
}
#[async_trait]
impl RuntimeComponent for ReactionInstance {
    fn id(&self) -> &str {
        self.reaction.id()
    }
    fn kind(&self) -> &'static str {
        "reaction"
    }
    fn metadata(&self) -> Option<std::collections::HashMap<String, String>> {
        self.pending
            .as_ref()
            .and_then(|pending| pending.metadata().cloned())
    }
    fn configuration_available(&self) -> bool {
        self.pending
            .as_ref()
            .is_none_or(|pending| pending.configuration_available())
    }
    async fn construct(&self, generation: ComponentGeneration) -> anyhow::Result<()> {
        if let Some(pending) = &self.pending {
            let owner = self
                .owner
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("native runtime was dropped"))?;
            let control = owner.control()?;
            pending.construct(&control).await?;
            super::construction::publish_metadata(
                pending.metadata(),
                &control,
                &ComponentId::try_new(self.reaction.id())?,
                generation,
            )?;
        }
        Ok(())
    }
    fn dependency_waits(
        &self,
        desired: &GraphSnapshot,
        observed: &ObservedGraph,
    ) -> Vec<DependencyWait> {
        self.pending
            .as_ref()
            .map_or_else(Vec::new, |pending| pending.waiting(desired, observed))
    }
    async fn wait_ready(&self) -> anyhow::Result<()> {
        let host = self.host()?;
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            match self.reaction.status().await {
                ComponentStatus::Running => return Ok(()),
                ComponentStatus::Error => {
                    let failure = host
                        .wait_running()
                        .now_or_never()
                        .and_then(|result| result.err())
                        .unwrap_or_else(|| {
                            anyhow::anyhow!(
                                "Reaction '{}' failed before reaching Running",
                                self.reaction.id()
                            )
                        });
                    return Err(failure);
                }
                _ => {}
            }
            tokio::select! {
                changed = changes.changed() => changed?,
                running = host.wait_running() => return running,
            }
        }
    }
    async fn initialize(&self, generation: ComponentGeneration) -> anyhow::Result<()> {
        let _initializing = self.initializing.lock().await;
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("reaction is permanently shut down");
        }
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime was dropped"))?;
        let query_ids = self.query_ids();
        {
            let mut metrics = self
                .metrics
                .lock()
                .map_err(|_| anyhow::anyhow!("reaction metrics ownership poisoned"))?;
            for id in &query_ids {
                metrics
                    .entry(id.clone())
                    .or_insert_with(|| Arc::new(ReactionMetrics::new()));
            }
        }
        owner.reaction_queries(&query_ids).await?;
        for id in &query_ids {
            owner.link_query_output(id).await?;
        }
        anyhow::ensure!(
            self.current_host()?.is_none(),
            "failed reaction initialization requires a newly constructed object"
        );
        let host = ReactionPluginHost::for_runtime(
            self.reaction.clone(),
            owner.services.clone(),
            owner.catalog.clone(),
            ReactionPluginOptions::default(),
            RuntimeReactionMetrics {
                queries: self.metrics()?,
                lifecycle: owner.lifecycle_metrics.clone(),
            },
        )?;
        host.set_resource_observer(Arc::new(GraphResourceObserver::new(
            owner.control()?,
            ComponentId::try_new(self.reaction.id())?,
            generation,
        )))?;
        host.set_log_binding(crate::observability::ComponentLogBinding {
            registry: owner.logs.clone(),
            key: crate::observability::ComponentLogKey::new(
                &owner.config.id,
                crate::observability::ComponentLogType::Reaction,
                self.reaction.id(),
            ),
        })?;
        // Retain the owner before initialization can be interrupted.
        *self
            .host
            .lock()
            .map_err(|_| anyhow::anyhow!("reaction host ownership poisoned"))? = Some(host.clone());
        host.initialize_component().await?;
        self.initialized.store(true, Ordering::Release);
        self.notify();
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        let host = self.host()?;
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime was dropped"))?;
        let mut inputs = self.inputs.lock().await;
        inputs.clear();
        let query_ids = self.query_ids();
        for id in &query_ids {
            owner.link_query_output(id).await?;
        }
        host.validate_startup_configuration().await?;
        let mut subscriptions = Vec::new();
        let mut heads = BTreeMap::new();
        for id in &query_ids {
            host.wait_query_ready(id).await?;
            let subscription = owner.query_subscription(id).await?;
            heads.insert(id.clone(), subscription.head()?);
            subscriptions.push((id.clone(), subscription));
        }
        *inputs = subscriptions;
        let result = host.start_component_with_heads(heads).await;
        if result.is_err() {
            inputs.clear();
        }
        self.notify();
        result
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.inputs.lock().await.clear();
        let result = match self.current_host()? {
            Some(host) => host.stop_component().await,
            None => Ok(()),
        };
        self.notify();
        result
    }
    async fn run(&self) -> anyhow::Result<()> {
        let host = self.host()?;
        let mut inputs = self.inputs.lock().await;
        if inputs.is_empty() {
            return host.run_observations().await;
        }
        let result = {
            async fn receive<'a>(
                query: &'a str,
                receiver: &'a mut CatalogSubscription,
            ) -> (
                &'a str,
                &'a mut CatalogSubscription,
                anyhow::Result<crate::computation::v1::EnvelopeMessage>,
            ) {
                let result = receiver.receive_message().await;
                (query, receiver, result)
            }
            let mut receivers: FuturesUnordered<_> = inputs
                .iter_mut()
                .map(|(query, receiver)| receive(query, receiver))
                .collect();
            async {
                loop {
                    let (query, receiver, result) = tokio::select! {
                        result = receivers.next() => result.expect("nonempty subscriptions"),
                        result = host.run_observations() => return result,
                    };
                    // Drop the idle status reader before the enqueue/recovery
                    // operation drives and drains that same observation channel.
                    match result {
                        Ok(message) => {
                            crate::observability::ComponentObservations::current_or_disabled()
                                .begin(
                                    crate::observability::ObservedOperation::Handle,
                                    &message.execution_context,
                                    crate::observability::OperationFields {
                                        input_count: Some(1),
                                        change_count: Some(
                                            message.envelope.changes().operations().len(),
                                        ),
                                        completion: Some("accepted"),
                                        ..Default::default()
                                    },
                                )?
                                .run(host.handle_envelope(&message.envelope))
                                .await?;
                        }
                        Err(error)
                            if matches!(
                                error.downcast_ref::<tokio::sync::broadcast::error::RecvError>(),
                                Some(tokio::sync::broadcast::error::RecvError::Lagged(_))
                            ) =>
                        {
                            host.recover_gap(query).await?
                        }
                        Err(error) => return Err(error),
                    }
                    receivers.push(receive(query, receiver));
                }
            }
            .await
        };
        inputs.clear();
        let cleanup = host.stop_component().await;
        self.notify();
        match (result, cleanup) {
            (Err(error), Err(cleanup)) => Err(error.context(crate::error::OperationFailures::new(
                "reaction stop also failed",
                vec![cleanup],
            ))),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), cleanup) => cleanup,
        }
    }
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        let _initializing = self.initializing.lock().await;
        self.inputs.lock().await.clear();
        if let Some(host) = self.current_host()? {
            host.shutdown().await?;
        } else {
            self.reaction.dispose().await?;
        }
        self.notify();
        Ok(())
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.delete_owned_resources().await
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.host()?.pause_processing().await
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.host()?.resume_processing().await
    }
    async fn apply_update(
        &self,
        context: crate::management::OrdinaryComponentUpdateContext,
    ) -> anyhow::Result<()> {
        self.reaction.apply_update(context).await
    }
}
