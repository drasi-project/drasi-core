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
    pub query_ids: Vec<String>,
    pub metrics: BTreeMap<String, Arc<ReactionMetrics>>,
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
        let query_ids = reaction.query_ids();
        let metrics: BTreeMap<_, _> = query_ids
            .iter()
            .cloned()
            .map(|id| (id, Arc::new(ReactionMetrics::new())))
            .collect();
        Arc::new(Self {
            reaction,
            query_ids,
            metrics,
            host: StdMutex::new(None),
            initializing: Mutex::new(()),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            owner: Arc::downgrade(owner),
            inputs: Mutex::new(Vec::new()),
            changed: owner.changed.clone(),
        })
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
    pub(super) async fn deprovision(&self) -> anyhow::Result<()> {
        RuntimeComponent::shutdown(self).await?;
        self.reaction.deprovision().await
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
        owner.reaction_queries(&self.query_ids).await?;
        if let Some(previous) = self.current_host()? {
            previous.shutdown().await?;
        }
        let host = ReactionPluginHost::for_runtime(
            self.reaction.clone(),
            owner.services.clone(),
            owner.catalog.clone(),
            ReactionPluginOptions::default(),
            RuntimeReactionMetrics {
                queries: self.metrics.clone(),
                lifecycle: owner.lifecycle_metrics.clone(),
            },
        )?;
        host.set_resource_observer(Arc::new(GraphResourceObserver::new(
            owner.control()?,
            ComponentId::try_new(self.reaction.id())?,
            generation,
        )))?;
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
        host.validate_startup_configuration().await?;
        let mut subscriptions = Vec::new();
        let mut heads = BTreeMap::new();
        for id in &self.query_ids {
            let query = owner.query(id).await?;
            host.wait_query_ready(id).await?;
            let subscription = query.subscribe_results()?;
            heads.insert(query.config.id.clone(), subscription.head()?);
            subscriptions.push((query.config.id.clone(), subscription));
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
                anyhow::Result<ChangeEnvelope>,
            ) {
                let result = receiver.receive().await;
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
                        Ok(envelope) => host.handle_envelope(&envelope).await?,
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
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("reaction stop also failed: {cleanup:#}")))
            }
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
        }
        self.notify();
        Ok(())
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.deprovision().await
    }
    async fn status(&self) -> ComponentStatus {
        self.reaction.status().await
    }
}
