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

use std::collections::{HashMap, HashSet};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::Client;
use uuid::Uuid;

use crate::config::GitHubWorkGraphReactionConfig;
use crate::model::{
    lease_comment, CapacityRow, DispatchableTask, IssueComment, PendingLease, PendingScope,
};

pub struct GitHubWorkGraphReaction {
    pub(crate) base: ReactionBase,
    config: GitHubWorkGraphReactionConfig,
}

impl GitHubWorkGraphReaction {
    pub fn new(
        id: impl Into<String>,
        query_ids: Vec<String>,
        config: GitHubWorkGraphReactionConfig,
        auto_start: bool,
    ) -> Result<Self> {
        config.validate(&query_ids)?;
        let params = ReactionBaseParams::new(id.into(), query_ids).with_auto_start(auto_start);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    pub(crate) fn build_client(&self) -> Result<Client> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.config.token))
                .context("invalid GitHub token header value")?,
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("drasi-reaction-github-workgraph"),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static("2022-11-28"),
        );
        Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build GitHub HTTP client")
    }
}

pub(crate) struct DispatcherEngine {
    client: Client,
    api_base_url: String,
    pending: HashMap<String, PendingLease>,
}

impl DispatcherEngine {
    pub(crate) fn new(client: Client, api_base_url: impl Into<String>) -> Self {
        Self {
            client,
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            pending: HashMap::new(),
        }
    }

    pub(crate) async fn process_query_result(&mut self, result: &QueryResult) -> Result<()> {
        for diff in &result.results {
            let value = match diff {
                ResultDiff::Add { data, .. } => data,
                ResultDiff::Update { after, .. } | ResultDiff::Aggregation { after, .. } => after,
                ResultDiff::Delete { .. } | ResultDiff::Noop => continue,
            };
            let row: CapacityRow = serde_json::from_value(value.clone()).with_context(|| {
                format!("malformed capacity row from query '{}'", result.query_id)
            })?;
            self.process_row(row).await?;
        }
        Ok(())
    }

    async fn process_row(&mut self, row: CapacityRow) -> Result<()> {
        ensure!(
            row.lease_duration_seconds > 0,
            "leaseDurationSeconds must be positive"
        );
        let scope = PendingScope::from(&row);
        let active: HashSet<&str> = row.active_lease_ids.iter().map(String::as_str).collect();
        self.pending.retain(|lease_id, pending| {
            pending.scope != scope || !active.contains(lease_id.as_str())
        });

        let mut slots: Vec<(u64, String)> = row
            .free_slot_ids
            .iter()
            .filter(|slot_id| {
                !self
                    .pending
                    .values()
                    .any(|pending| pending.slot_id == slot_id.as_str())
            })
            .map(|slot_id| {
                let (worker_id, suffix) = slot_id
                    .rsplit_once('/')
                    .context("free slot ID must end with a numeric slot suffix")?;
                ensure!(
                    !worker_id.is_empty(),
                    "free slot ID must include a worker ID"
                );
                ensure!(
                    worker_id == row.worker_id,
                    "free slot ID worker prefix does not match capacity row workerId"
                );
                let slot_number = suffix
                    .parse::<u64>()
                    .context("free slot ID must end with a numeric slot suffix")?;
                ensure!(slot_number > 0, "free slot ID suffix must be positive");
                Ok((slot_number, slot_id.clone()))
            })
            .collect::<Result<_>>()?;
        slots.sort_by(|(left_number, left_id), (right_number, right_id)| {
            left_number
                .cmp(right_number)
                .then_with(|| left_id.cmp(right_id))
        });
        let mut tasks: Vec<DispatchableTask> = row
            .dispatchable_tasks
            .iter()
            .filter(|task| {
                !self
                    .pending
                    .values()
                    .any(|pending| pending.task_node_id == task.task_node_id)
            })
            .cloned()
            .collect();
        tasks.sort_by(|left, right| {
            left.queue_priority
                .cmp(&right.queue_priority)
                .then_with(|| left.assignment_created_at.cmp(&right.assignment_created_at))
                .then_with(|| left.task_node_id.cmp(&right.task_node_id))
        });

        for ((_, slot_id), task) in slots.iter().zip(tasks.iter()) {
            ensure!(
                task.worker_id == row.worker_id,
                "dispatchable task workerId does not match capacity row workerId"
            );
            ensure!(
                task.repository_owner == row.repository_owner
                    && task.repository_name == row.repository_name,
                "dispatchable task repository does not match capacity row repository"
            );
            let lease_id = Uuid::now_v7().to_string();
            self.post_lease(
                &lease_id,
                slot_id,
                task,
                Utc::now(),
                row.lease_duration_seconds,
            )
            .await?;
            self.pending.insert(
                lease_id,
                PendingLease {
                    scope: scope.clone(),
                    slot_id: slot_id.clone(),
                    task_node_id: task.task_node_id.clone(),
                },
            );
        }
        Ok(())
    }

    pub(crate) async fn post_lease(
        &self,
        lease_id: &str,
        slot_id: &str,
        task: &DispatchableTask,
        acquired_at: DateTime<Utc>,
        lease_duration_seconds: i64,
    ) -> Result<()> {
        let body = lease_comment(lease_id, slot_id, task, acquired_at, lease_duration_seconds)?;
        let url = format!(
            "{}/repos/{}/{}/issues/{}/comments",
            self.api_base_url, task.repository_owner, task.repository_name, task.task_number
        );
        self.client
            .post(url)
            .json(&IssueComment { body })
            .send()
            .await
            .context("failed to create GitHub issue comment")?
            .error_for_status()
            .context("GitHub issue comment was rejected")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> &HashMap<String, PendingLease> {
        &self.pending
    }
}

#[async_trait]
impl Reaction for GitHubWorkGraphReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "github-workgraph-dispatcher"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
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

    async fn start(&self) -> Result<()> {
        self.config.validate(&self.base.queries)?;
        log_component_start("GitHub WorkGraph Reaction", &self.base.id);
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting GitHub WorkGraph reaction".to_string()),
            )
            .await;

        let client = self.build_client()?;
        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let priority_queue = self.base.priority_queue.clone();
        let status = self.base.status_handle();
        let reaction_id = self.base.id.clone();
        let mut engine = DispatcherEngine::new(client, self.config.api_base_url.clone());

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("GitHub WorkGraph reaction started".to_string()),
            )
            .await;

        let processing_task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = priority_queue.dequeue() => result,
                };
                if let Err(error) = engine.process_query_result(result.as_ref()).await {
                    log::error!("[{reaction_id}] GitHub WorkGraph dispatch failed: {error:#}");
                    status
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("GitHub WorkGraph dispatch failed: {error:#}")),
                        )
                        .await;
                    break;
                }
            }
        });
        self.base.set_processing_task(processing_task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
}
