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
        plugin_services::PluginObservations, ComponentGeneration, LegacyPluginServices,
        SourcePluginHost,
    },
    ComponentStatus, Source, SourceRuntimeContext,
};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};

#[derive(Default)]
struct Life {
    initialized: bool,
    initialized_complete: bool,
    needs_stop: bool,
    closed: bool,
}

pub(super) struct SourceInstance {
    pub source: Arc<dyn Source>,
    pub borrowed: Arc<SourcePluginHost>,
    services: LegacyPluginServices,
    observations: PluginObservations,
    life: Mutex<Life>,
    changed: watch::Sender<u64>,
}
impl SourceInstance {
    pub(super) fn new(
        source: Box<dyn Source>,
        services: LegacyPluginServices,
        changed: watch::Sender<u64>,
    ) -> Arc<Self> {
        let source: Arc<dyn Source> = source.into();
        Arc::new(Self {
            observations: PluginObservations::new(source.id()),
            borrowed: SourcePluginHost::borrowed(source.clone()),
            source,
            services,
            life: Mutex::new(Life::default()),
            changed,
        })
    }
    pub(super) async fn deprovision(&self) -> anyhow::Result<()> {
        self.source.deprovision().await
    }
    pub(super) fn notify(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
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
    async fn initialize(&self, _: ComponentGeneration) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.initialized {
            if !life.initialized_complete {
                anyhow::bail!("source initialization was interrupted");
            }
            return Ok(());
        }
        life.initialized = true;
        let mut context = SourceRuntimeContext::new(
            self.services.scope.as_ref(),
            self.source.id(),
            self.services.state_store.clone(),
            self.observations.channel().await,
            self.services.identity.clone(),
        );
        context.wal_provider = self.services.wal.clone();
        self.observations
            .drive(
                async {
                    self.source.initialize(context).await;
                    Ok(())
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
        self.observations.reset().await;
        let result = self.observations.drive(self.source.start(), false).await;
        self.notify();
        result
    }
    async fn stop(&self) -> anyhow::Result<()> {
        let mut life = self.life.lock().await;
        if life.needs_stop || life.initialized && !life.initialized_complete {
            self.observations.drive(self.source.stop(), true).await?;
            life.needs_stop = false;
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
                return Ok(());
            }
        }
    }
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop().await?;
        self.life.lock().await.closed = true;
        self.observations.close().await;
        Ok(())
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.source.deprovision().await
    }
    async fn status(&self) -> ComponentStatus {
        self.source.status().await
    }
}
