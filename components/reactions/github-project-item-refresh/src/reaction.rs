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

use anyhow::Context;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::{CheckpointState, FailureAction};
use drasi_lib::reactions::ManagerCheckpointOwnership;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

use crate::config::GitHubProjectItemRefreshConfig;
use crate::destination::DestinationSourceClient;
use crate::graphql::GitHubGraphqlClient;
use crate::processing::RefreshProcessor;
use crate::state_store::RefreshStateStore;
use crate::GitHubProjectItemRefreshBuilder;

pub(crate) const HTTP_USER_AGENT: &str = concat!(
    "drasi-github-project-item-refresh/",
    env!("CARGO_PKG_VERSION")
);

pub struct GitHubProjectItemRefreshReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: GitHubProjectItemRefreshConfig,
}

impl GitHubProjectItemRefreshReaction {
    pub fn builder(id: impl Into<String>) -> GitHubProjectItemRefreshBuilder {
        GitHubProjectItemRefreshBuilder::new(id)
    }

    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: GitHubProjectItemRefreshConfig,
    ) -> Self {
        let params = ReactionBaseParams::new(id.into(), queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: GitHubProjectItemRefreshConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
        recovery_policy: Option<ReactionRecoveryPolicy>,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        if let Some(policy) = recovery_policy {
            params = params.with_recovery_policy(policy);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    fn build_http_client(&self) -> anyhow::Result<Client> {
        Client::builder()
            .user_agent(HTTP_USER_AGENT)
            .timeout(Duration::from_millis(self.config.request_timeout_ms))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build()
            .context("building shared HTTP client")
    }
}

#[async_trait]
impl Reaction for GitHubProjectItemRefreshReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "github-project-item-refresh"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::GitHubProjectItemRefreshConfigDto::from(&self.config);
        self.base.properties_or_serialize(&dto)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        log_component_start("GitHub Project Item Refresh Reaction", &self.base.id);
        info!(
            "[{}] GitHub project item refresh reaction starting",
            self.base.id
        );

        if let Err(err) = self.config.validate(&self.base.queries, None) {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Invalid reaction config: {err:#}")),
                )
                .await;
            return Err(err);
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting GitHub project item refresh reaction".to_string()),
            )
            .await;

        let state_store = self.base.state_store().await.ok_or_else(|| {
            anyhow::anyhow!(
                "No state store configured. github-project-item-refresh requires durable state"
            )
        })?;
        if !state_store.is_durable() {
            anyhow::bail!(
                "github-project-item-refresh requires a durable state store (state store is non-durable)"
            );
        }

        let http_client = self.build_http_client()?;
        let processor = RefreshProcessor::new(
            self.config.clone(),
            RefreshStateStore::new(state_store.clone(), self.base.id.clone()),
            GitHubGraphqlClient::new(
                http_client.clone(),
                self.config.graphql_url.clone(),
                self.config.github_token.clone(),
                self.config.graphql_headers.clone(),
                self.config.status_field_name.clone(),
            ),
            DestinationSourceClient::new(
                http_client.clone(),
                self.config.destination_event_url.clone(),
                self.config.destination_bearer_secret.clone(),
            ),
        );

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let reaction_name = self.base.id.clone();
        let policy = self
            .base
            .recovery_policy
            .unwrap_or_else(|| self.default_recovery_policy());
        let status_handle = self.base.status_handle();
        let mut checkpoints = CheckpointState::load(&self.base).await;

        let processing_task = tokio::spawn(async move {
            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal");
                        break;
                    }
                    result = base.priority_queue.dequeue() => result,
                };
                let query_result = query_result_arc.as_ref();
                if query_result.results.is_empty() {
                    continue;
                }

                let query_name = &query_result.query_id;
                let seq = query_result.sequence;
                debug!(
                    "[{reaction_name}] Processing query result seq={seq} query='{query_name}' rows={}",
                    query_result.results.len()
                );

                let mut delivery_failed = false;
                for diff in &query_result.results {
                    match diff {
                        ResultDiff::Add { data, .. } => {
                            if let Err(err) = processor.process_add_row(data).await {
                                error!(
                                    "[{reaction_name}] Failed to process invalidation row (query='{query_name}', seq={seq}): {err:#}"
                                );
                                delivery_failed = true;
                                if FailureAction::from_policy(policy) == FailureAction::Stop {
                                    break;
                                }
                            }
                        }
                        ResultDiff::Update { .. } | ResultDiff::Delete { .. } => {
                            debug!(
                                "[{reaction_name}] Ignoring non-ADD diff for query '{query_name}' seq={seq}"
                            );
                        }
                        ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
                    }
                }

                if delivery_failed {
                    match FailureAction::from_policy(policy) {
                        FailureAction::Stop => {
                            status_handle
                                .set_status(
                                    ComponentStatus::Error,
                                    Some(format!(
                                        "Delivery failed for query '{query_name}' (seq {seq}); stopped per strict recovery policy"
                                    )),
                                )
                                .await;
                            return;
                        }
                        FailureAction::SkipAndContinue => {
                            warn!(
                                "[{reaction_name}] Delivery failure for query '{query_name}' (seq {seq}); skipping per AutoSkipGap policy"
                            );
                            if let Err(err) = checkpoints.advance(&base, query_name, seq).await {
                                error!(
                                    "[{reaction_name}] Failed to write checkpoint while skipping failed event: {err:#}"
                                );
                            }
                            continue;
                        }
                    }
                }

                if let Err(err) = checkpoints.advance(&base, query_name, seq).await {
                    error!(
                        "[{reaction_name}] Failed to write checkpoint for query '{query_name}' (seq {seq}): {err:#}"
                    );
                    if FailureAction::from_policy(policy) == FailureAction::Stop {
                        status_handle
                            .set_status(
                                ComponentStatus::Error,
                                Some(format!(
                                    "Checkpoint write failed for query '{query_name}' (seq {seq}); stopped per recovery policy"
                                )),
                            )
                            .await;
                        return;
                    }
                }
            }

            status_handle
                .set_status(
                    ComponentStatus::Stopped,
                    Some("GitHub project item refresh reaction stopped".to_string()),
                )
                .await;
        });

        self.base.set_processing_task(processing_task).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("GitHub project item refresh reaction running".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
    }
}
