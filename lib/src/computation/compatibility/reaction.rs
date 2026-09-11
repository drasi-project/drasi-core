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
use futures::{stream::FuturesUnordered, StreamExt};
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use tokio::sync::{watch, Mutex};

pub(super) struct ReactionInstance {
    pub reaction: Arc<dyn Reaction>,
    pub metrics: BTreeMap<String, Arc<ReactionMetrics>>,
    pub host: Arc<ReactionPluginHost>,
    owner: Weak<super::Runtime>,
    inputs: Mutex<Vec<(String, CatalogSubscription)>>,
    changed: watch::Sender<u64>,
}
impl ReactionInstance {
    pub(super) fn new(
        reaction: Box<dyn Reaction>,
        owner: &Arc<super::Runtime>,
    ) -> anyhow::Result<Arc<Self>> {
        let reaction: Arc<dyn Reaction> = reaction.into();
        let metrics: BTreeMap<_, _> = reaction
            .query_ids()
            .into_iter()
            .map(|id| (id, Arc::new(ReactionMetrics::new())))
            .collect();
        let host = ReactionPluginHost::for_runtime(
            reaction.clone(),
            owner.services.clone(),
            owner.catalog.clone(),
            ReactionPluginOptions::default(),
            RuntimeReactionMetrics {
                queries: metrics.clone(),
                lifecycle: owner.lifecycle_metrics.clone(),
            },
        )?;
        Ok(Arc::new(Self {
            reaction,
            metrics,
            host,
            owner: Arc::downgrade(owner),
            inputs: Mutex::new(Vec::new()),
            changed: owner.changed.clone(),
        }))
    }
    fn notify(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub(super) async fn deprovision(&self) -> anyhow::Result<()> {
        self.host.deprovision_owned().await
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
    async fn initialize(&self, _: ComponentGeneration) -> anyhow::Result<()> {
        self.host.initialize_component().await?;
        self.notify();
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime was dropped"))?;
        let mut inputs = self.inputs.lock().await;
        inputs.clear();
        for id in self.reaction.query_ids() {
            let query = owner.query(&id).await?;
            inputs.push((
                query.config.id.clone(),
                query.catalog.subscribe_query(&query.config.id)?,
            ));
        }
        let result = self.host.start_component().await;
        if result.is_err() {
            inputs.clear();
        }
        self.notify();
        result
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.inputs.lock().await.clear();
        let result = self.host.stop_component().await;
        self.notify();
        result
    }
    async fn run(&self) -> anyhow::Result<()> {
        let mut inputs = self.inputs.lock().await;
        if inputs.is_empty() {
            return self.host.run_observations().await;
        }
        // A receive and its enqueue share one scoped worker. An extra prefetch
        // queue would change the legacy dispatch capacity and gap boundary.
        let result = {
            let mut workers: FuturesUnordered<_> = inputs
                .iter_mut()
                .map(|(query, receiver)| async {
                    loop {
                        match receiver.receive().await {
                            Ok(envelope) => self.host.handle_envelope(&envelope).await?,
                            Err(error)
                                if matches!(
                                    error
                                        .downcast_ref::<tokio::sync::broadcast::error::RecvError>(),
                                    Some(tokio::sync::broadcast::error::RecvError::Lagged(_))
                                ) =>
                            {
                                self.host.recover_gap(query).await?;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                })
                .collect();
            tokio::select! {
                result = workers.next() => result.expect("nonempty subscription workers"),
                result = self.host.run_observations() => result,
            }
        };
        inputs.clear();
        let cleanup = self.host.stop_component().await;
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
        self.inputs.lock().await.clear();
        self.host.shutdown().await?;
        self.notify();
        Ok(())
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.host.deprovision_owned().await
    }
    async fn status(&self) -> ComponentStatus {
        self.reaction.status().await
    }
}
