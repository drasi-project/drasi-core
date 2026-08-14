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

//! Bootstrap provider for initial GitHub snapshot loading.

use crate::config::GitHubSourceConfig;
use crate::graphql::GitHubGraphQLClient;
use crate::mapping::{map_reconcile_snapshot, repositories_from_project_items};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use std::collections::{HashMap, HashSet};

pub struct GitHubBootstrapProvider {
    config: GitHubSourceConfig,
}

impl GitHubBootstrapProvider {
    pub fn new(config: GitHubSourceConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl BootstrapProvider for GitHubBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let client =
            GitHubGraphQLClient::new(self.config.graphql_url.clone(), self.config.token.clone())
                .context("Failed to create GitHub GraphQL client for bootstrap")?;

        let mut effective_repos = self.config.static_repository_set()?;
        for project in &self.config.projects {
            let project_items = client.fetch_project_items(project).await.with_context(|| {
                format!(
                    "Failed to fetch project items for {}#{}",
                    project.owner, project.number
                )
            })?;
            effective_repos.extend(repositories_from_project_items(&project_items));
        }

        let repos_vec = effective_repos.into_iter().collect::<Vec<_>>();
        let snapshot = client
            .fetch_reconcile_snapshot(&repos_vec, &self.config.projects)
            .await
            .context("Failed to fetch bootstrap snapshot")?;

        let (changes, _) = map_reconcile_snapshot(
            &context.source_id,
            &snapshot,
            &HashMap::new(),
            chrono::Utc::now().timestamp_millis().max(0) as u64,
        );

        let node_filter: HashSet<String> = request.node_labels.into_iter().collect();
        let rel_filter: HashSet<String> = request.relation_labels.into_iter().collect();

        let mut sent = 0usize;
        for change in changes {
            if !label_matches(&change, &node_filter, &rel_filter) {
                continue;
            }

            let event = BootstrapEvent {
                source_id: context.source_id.clone(),
                change,
                timestamp: chrono::Utc::now(),
                sequence: context.next_sequence(),
            };
            if event_tx.send(event).await.is_err() {
                break;
            }
            sent += 1;
        }

        Ok(BootstrapResult {
            event_count: sent,
            source_position: None,
        })
    }
}

fn label_matches(
    change: &SourceChange,
    node_filter: &HashSet<String>,
    rel_filter: &HashSet<String>,
) -> bool {
    if node_filter.is_empty() && rel_filter.is_empty() {
        return true;
    }

    let labels = match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_metadata().labels.clone()
        }
        SourceChange::Delete { metadata } => metadata.labels.clone(),
        SourceChange::Future { .. } => return false,
    };

    for label in labels.iter() {
        let label = label.as_ref();
        if node_filter.contains(label) || rel_filter.contains(label) {
            return true;
        }
    }
    false
}
