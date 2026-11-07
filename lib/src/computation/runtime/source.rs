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
use crate::{
    computation::v1::{
        plugin_services::PluginObservations, ComponentGeneration, ComponentId, GraphControl,
        GraphResourceObserver, LegacyPluginServices, SourcePluginHost, SourceSubscriptionObserver,
    },
    ComponentStatus, Source, SourceRuntimeContext,
};
use async_trait::async_trait;
use futures::FutureExt;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Weak,
};
use tokio::sync::{watch, Mutex};

#[derive(Default)]
struct Life {
    initialized: bool,
    initialized_complete: bool,
    needs_stop: bool,
    stop_completed: bool,
    closed: bool,
    disposed: bool,
}

struct SubscriptionObserver {
    owner: Weak<super::Runtime>,
    revision: AtomicU64,
}

#[async_trait]
impl SourceSubscriptionObserver for SubscriptionObserver {
    fn begin_subscriptions(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
    }
    async fn subscriptions_changed(&self) -> anyhow::Result<()> {
        self.owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("source subscription owner was retired"))?
            .subscriptions_complete()
            .await
    }
}

pub(super) struct SourceInstance {
    pub source: Arc<dyn Source>,
    pub(super) pending: Option<Arc<super::source_creation::DeferredSource>>,
    pub borrowed: Arc<SourcePluginHost>,
    services: LegacyPluginServices,
    parent_control: GraphControl,
    observations: PluginObservations,
    life: Mutex<Life>,
    changed: watch::Sender<u64>,
    subscriptions: Arc<SubscriptionObserver>,
    subscriptions_notified: Mutex<Option<u64>>,
}
impl SourceInstance {
    pub(super) fn new(
        source: Box<dyn Source>,
        services: LegacyPluginServices,
        changed: watch::Sender<u64>,
        parent_control: GraphControl,
        owner: Weak<super::Runtime>,
    ) -> Arc<Self> {
        let source: Arc<dyn Source> = source.into();
        Self::new_inner(source, None, services, changed, parent_control, owner)
    }
    pub(super) fn deferred(
        source: Arc<super::source_creation::DeferredSource>,
        services: LegacyPluginServices,
        changed: watch::Sender<u64>,
        parent_control: GraphControl,
        owner: Weak<super::Runtime>,
    ) -> Arc<Self> {
        Self::new_inner(
            source.clone(),
            Some(source),
            services,
            changed,
            parent_control,
            owner,
        )
    }
    fn new_inner(
        source: Arc<dyn Source>,
        pending: Option<Arc<super::source_creation::DeferredSource>>,
        services: LegacyPluginServices,
        changed: watch::Sender<u64>,
        parent_control: GraphControl,
        owner: Weak<super::Runtime>,
    ) -> Arc<Self> {
        let subscriptions = Arc::new(SubscriptionObserver {
            owner,
            revision: AtomicU64::new(0),
        });
        Arc::new(Self {
            observations: PluginObservations::new(source.id()),
            borrowed: SourcePluginHost::borrowed_with_subscription_observer(
                source.clone(),
                subscriptions.clone(),
                pending
                    .clone()
                    .map(|source| source as Arc<dyn crate::computation::v1::SourcePreparation>),
            ),
            source,
            pending,
            services,
            parent_control,
            life: Mutex::new(Life::default()),
            changed,
            subscriptions,
            subscriptions_notified: Mutex::new(None),
        })
    }
    pub(super) fn notify(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub(super) async fn notify_subscriptions_complete(&self) {
        let mut notified = self.subscriptions_notified.lock().await;
        let revision = self.subscriptions.revision.load(Ordering::Acquire);
        if *notified != Some(revision) {
            self.source.on_subscriptions_complete().await;
            *notified = Some(revision);
        }
    }
}
#[async_trait]
impl RuntimeComponent for SourceInstance {
    fn id(&self) -> &str {
        self.source.id()
    }
    fn kind(&self) -> &'static str {
        "source"
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
            pending.construct().await?;
            super::construction::publish_metadata(
                pending.metadata(),
                &self.parent_control,
                &ComponentId::try_new(self.source.id())?,
                generation,
            )?;
        }
        Ok(())
    }
    async fn initialize(&self, generation: ComponentGeneration) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.closed {
            anyhow::bail!("source is permanently shut down");
        }
        if life.initialized_complete {
            return Ok(());
        }
        if life.initialized {
            anyhow::bail!("failed source initialization requires a newly constructed object");
        }
        self.observations.reset().await?;
        life.initialized = true;
        let mut context = SourceRuntimeContext::new(
            self.services.scope.as_ref(),
            self.source.id(),
            self.services.state_store.clone(),
            self.observations.channel().await,
            self.services.identity.clone(),
        );
        context.wal_provider = self.services.wal.clone();
        context.resource_observer = Some(Arc::new(GraphResourceObserver::new(
            self.parent_control.clone(),
            ComponentId::try_new(self.source.id())?,
            generation,
        )));
        self.observations
            .drive(
                async {
                    self.source.bind_runtime_context(context).await;
                    self.source.initialize().await
                },
                false,
            )
            .await?;
        life.initialized_complete = true;
        self.notify();
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.closed || !life.initialized_complete {
            anyhow::bail!("source is not initialized for activation");
        }
        life.needs_stop = true;
        life.stop_completed = false;
        self.subscriptions.begin_subscriptions();
        self.observations.reset().await?;
        let result = self.observations.drive(self.source.start(), false).await;
        self.notify();
        result?;
        self.subscriptions.subscriptions_changed().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.needs_stop {
            self.observations.drive(self.source.stop(), true).await?;
            let status = self.source.status().await;
            anyhow::ensure!(
                status == ComponentStatus::Stopped,
                "Source '{}' returned from stop with status {status:?}, expected Stopped",
                self.source.id()
            );
            life.needs_stop = false;
            life.stop_completed = true;
        }
        self.notify();
        Ok(())
    }
    async fn run(&self) -> anyhow::Result<()> {
        loop {
            let update = self.observations.read().await;
            self.notify();
            update?;
            if self.source.status().await == ComponentStatus::Stopped {
                self.observations.drain().await?;
                return Ok(());
            }
        }
    }
    async fn wait_ready(&self) -> anyhow::Result<()> {
        let mut changes = self.changed.subscribe();
        loop {
            changes.borrow_and_update();
            match self.source.status().await {
                ComponentStatus::Running => return Ok(()),
                ComponentStatus::Error => {
                    let failure = self
                        .observations
                        .failure()
                        .now_or_never()
                        .and_then(|result| result.err())
                        .unwrap_or_else(|| {
                            anyhow::anyhow!(
                                "Source '{}' failed before reaching Running",
                                self.source.id()
                            )
                        });
                    return Err(failure);
                }
                _ => {}
            }
            tokio::select! {
                changed = changes.changed() => changed?,
                running = self.observations.wait_running() => return running,
            }
        }
    }
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop().await?;
        let mut life = self.life.lock().await;
        life.closed = true;
        if !life.disposed {
            self.observations.drive(self.source.dispose(), true).await?;
            life.disposed = true;
        }
        drop(life);
        self.observations.close().await;
        Ok(())
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.source.delete_owned_resources().await
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.source.pause_processing().await
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.source.resume_processing().await
    }
    async fn apply_update(
        &self,
        context: crate::management::OrdinaryComponentUpdateContext,
    ) -> anyhow::Result<()> {
        self.source.apply_update(context).await
    }
}
