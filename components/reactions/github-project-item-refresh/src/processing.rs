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
use chrono::{DateTime, Utc};
use log::warn;
use reqwest::Url;
use serde_json::Value;
use std::time::Duration;

use crate::config::{
    validate_project_item_node_id, validate_project_node_id, GitHubProjectItemRefreshConfig,
};
use crate::destination::{DestinationPublishError, DestinationSourceClient};
use crate::graphql::{GitHubGraphqlClient, GraphqlFetchError};
use crate::models::{
    DeliveryKey, DeliveryReservation, FetchedProjectItemState, InvalidationInput,
    ItemVersionRecord, ProjectItemStatusNode, PublicationRecord, PublicationState,
};
use crate::state_store::RefreshStateStore;

const MAX_FETCH_ATTEMPTS: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 100;
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddRowOutcome {
    Published,
    Duplicate,
    Stale,
    Rejected,
}

#[derive(Clone)]
pub struct RefreshProcessor {
    config: GitHubProjectItemRefreshConfig,
    state_store: RefreshStateStore,
    graphql_client: GitHubGraphqlClient,
    destination_client: DestinationSourceClient,
}

impl RefreshProcessor {
    pub fn new(
        config: GitHubProjectItemRefreshConfig,
        state_store: RefreshStateStore,
        graphql_client: GitHubGraphqlClient,
        destination_client: DestinationSourceClient,
    ) -> Self {
        Self {
            config,
            state_store,
            graphql_client,
            destination_client,
        }
    }

    pub async fn process_add_row(&self, row_data: &Value) -> anyhow::Result<AddRowOutcome> {
        let input = parse_invalidation_input(row_data).context("parsing invalidation row")?;
        self.process_invalidation(input).await
    }

    async fn process_invalidation(
        &self,
        input: InvalidationInput,
    ) -> anyhow::Result<AddRowOutcome> {
        if let Err(err) = self
            .state_store
            .prune_terminal_records_older_than(self.config.delivery_record_ttl_secs, Utc::now())
            .await
        {
            warn!("failed to prune terminal delivery records: {err:#}");
        }

        validate_project_item_node_id(&input.project_item_node_id)
            .context("validating project item node id")?;
        if input.delivery_id.trim().is_empty() {
            anyhow::bail!("delivery id must not be empty");
        }

        if let Some(project_node_id) = &input.project_node_id {
            validate_project_node_id(project_node_id).context("validating project node id")?;
            if !self.config.is_project_allowed(project_node_id) {
                return self
                    .mark_rejected(
                        &input,
                        format!("project '{project_node_id}' is not allowlisted"),
                    )
                    .await;
            }
        }

        if let Some(state_source_url) = &input.state_source_url {
            if !urls_match(state_source_url, &self.config.destination_event_url) {
                return self
                        .mark_rejected(
                            &input,
                            format!(
                                "input StateSourceUrl '{state_source_url}' does not match configured destinationEventUrl '{}'",
                                self.config.destination_event_url
                            ),
                        )
                        .await;
            }
        }

        let key = DeliveryKey::new(&input.delivery_id, &input.project_item_node_id);
        let reservation = DeliveryReservation {
            delivery_id: input.delivery_id.clone(),
            project_item_node_id: input.project_item_node_id.clone(),
            invalidation_node_id: input.invalidation_node_id.clone(),
            project_node_id: input.project_node_id.clone(),
            status_field_node_id: input.status_field_node_id.clone(),
            state_source_url: input.state_source_url.clone(),
            webhook_action: input.webhook_action.clone(),
            webhook_updated_at: input.webhook_updated_at,
            reserved_at: Utc::now(),
        };

        let reservation = match self.state_store.get_reservation(&key).await? {
            Some(existing) => existing,
            None => {
                self.state_store
                    .set_reservation(&key, &reservation)
                    .await
                    .context("persisting delivery reservation")?;
                reservation
            }
        };

        let mut publication = self
            .state_store
            .get_publication(&key)
            .await?
            .unwrap_or_else(PublicationRecord::reserved);

        if matches!(
            publication.state,
            PublicationState::Published | PublicationState::Stale | PublicationState::Rejected
        ) {
            return Ok(AddRowOutcome::Duplicate);
        }

        let recoverable_fetched_state = match publication.state {
            PublicationState::Fetched | PublicationState::Ambiguous => {
                publication.fetched_state.clone()
            }
            _ => None,
        };
        publication.attempts = publication.attempts.saturating_add(1);
        publication.state = PublicationState::Reserved;
        publication.last_error = None;
        self.state_store
            .set_publication(&key, &publication)
            .await
            .context("writing reservation publication state")?;

        if let Some(status_field_node_id) = &reservation.status_field_node_id {
            if status_field_node_id != &self.config.expected_status_field_node_id {
                let message = format!(
                    "input StatusFieldNodeId '{status_field_node_id}' does not match configured expectedStatusFieldNodeId '{}'",
                    self.config.expected_status_field_node_id
                );
                self.state_store
                    .mark_failed(&key, publication, PublicationState::Failed, message.clone())
                    .await
                    .context("recording configured status field mismatch")?;
                anyhow::bail!(message);
            }
        }

        let fetched = match recoverable_fetched_state {
            Some(fetched) => fetched,
            None => match self.fetch_with_retry(&input).await {
                Ok(fetched) => fetched,
                Err(err) => {
                    self.state_store
                        .mark_failed(&key, publication, PublicationState::Failed, err.to_string())
                        .await
                        .context("recording fetch failure")?;
                    return Err(err);
                }
            },
        };

        if !self.config.is_project_allowed(&fetched.project_node_id) {
            return self
                .mark_rejected(
                    &input,
                    format!("project '{}' is not allowlisted", fetched.project_node_id),
                )
                .await;
        }

        if fetched.status_field_node_id != self.config.expected_status_field_node_id {
            let message = format!(
                "fetched status field node id '{}' does not match configured expectedStatusFieldNodeId '{}'",
                fetched.status_field_node_id, self.config.expected_status_field_node_id
            );
            self.state_store
                .mark_failed(&key, publication, PublicationState::Failed, message.clone())
                .await
                .context("recording fetched configured status field mismatch")?;
            anyhow::bail!(message);
        }

        if let Some(project_node_id) = &reservation.project_node_id {
            if project_node_id != &fetched.project_node_id {
                let message = format!(
                    "input project node id '{project_node_id}' does not match fetched project '{}'",
                    fetched.project_node_id
                );
                self.state_store
                    .mark_failed(&key, publication, PublicationState::Failed, message.clone())
                    .await
                    .context("recording project mismatch")?;
                anyhow::bail!(message);
            }
        }

        if let Some(status_field_node_id) = &reservation.status_field_node_id {
            if status_field_node_id != &fetched.status_field_node_id {
                let message = format!(
                    "input StatusFieldNodeId '{status_field_node_id}' does not match fetched status field '{}'",
                    fetched.status_field_node_id
                );
                self.state_store
                    .mark_failed(&key, publication, PublicationState::Failed, message.clone())
                    .await
                    .context("recording status field mismatch")?;
                anyhow::bail!(message);
            }
        }

        if let Some(version) = self
            .state_store
            .get_item_version(&fetched.project_item_node_id)
            .await?
        {
            if fetched.updated_at <= version.updated_at {
                let stale_record = PublicationRecord {
                    state: PublicationState::Stale,
                    attempts: publication.attempts,
                    last_error: None,
                    fetched_state: Some(fetched.clone()),
                    completed_at: Some(Utc::now()),
                };
                self.state_store
                    .set_publication(&key, &stale_record)
                    .await
                    .context("recording stale publication state")?;
                return Ok(AddRowOutcome::Stale);
            }
        }

        let fetched_record = PublicationRecord {
            state: PublicationState::Fetched,
            attempts: publication.attempts,
            last_error: None,
            fetched_state: Some(fetched.clone()),
            completed_at: None,
        };
        self.state_store
            .set_publication(&key, &fetched_record)
            .await
            .context("recording fetched publication state")?;

        let node = ProjectItemStatusNode::from_fetched(&fetched);
        if let Err(err) = self
            .destination_client
            .publish_project_item_status(&node)
            .await
        {
            let failed_state = if err.is_ambiguous() {
                PublicationState::Ambiguous
            } else {
                PublicationState::Failed
            };
            self.state_store
                .mark_failed(&key, fetched_record, failed_state, err.to_string())
                .await
                .context("recording publication failure")?;
            return Err(anyhow::anyhow!(err));
        }

        let published_at = Utc::now();
        let version = ItemVersionRecord {
            project_item_node_id: fetched.project_item_node_id.clone(),
            project_node_id: fetched.project_node_id.clone(),
            status_field_node_id: fetched.status_field_node_id.clone(),
            status_option_id: fetched.status_option_id.clone(),
            status_name: fetched.status_name.clone(),
            updated_at: fetched.updated_at,
            refreshed_at: fetched.refreshed_at,
            triggering_delivery_id: fetched.triggering_delivery_id.clone(),
            published_at,
        };
        self.state_store
            .set_item_version(&fetched.project_item_node_id, &version)
            .await
            .context("writing item version state")?;

        let published_record = PublicationRecord {
            state: PublicationState::Published,
            attempts: publication.attempts,
            last_error: None,
            fetched_state: Some(fetched),
            completed_at: Some(published_at),
        };
        self.state_store
            .set_publication(&key, &published_record)
            .await
            .context("recording published state")?;

        Ok(AddRowOutcome::Published)
    }

    async fn mark_rejected(
        &self,
        input: &InvalidationInput,
        message: String,
    ) -> anyhow::Result<AddRowOutcome> {
        let key = DeliveryKey::new(&input.delivery_id, &input.project_item_node_id);
        let mut publication = self
            .state_store
            .get_publication(&key)
            .await?
            .unwrap_or_else(PublicationRecord::reserved);
        publication.attempts = publication.attempts.saturating_add(1);
        publication.state = PublicationState::Rejected;
        publication.last_error = Some(message);
        publication.completed_at = Some(Utc::now());
        self.state_store
            .set_publication(&key, &publication)
            .await
            .context("recording rejected publication state")?;
        Ok(AddRowOutcome::Rejected)
    }

    async fn fetch_with_retry(
        &self,
        input: &InvalidationInput,
    ) -> anyhow::Result<FetchedProjectItemState> {
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        for attempt in 1..=MAX_FETCH_ATTEMPTS {
            let refreshed_at = Utc::now();
            match self
                .graphql_client
                .fetch_project_item_status(
                    &input.project_item_node_id,
                    &input.delivery_id,
                    refreshed_at,
                )
                .await
            {
                Ok(fetched) => return Ok(fetched),
                Err(err) => {
                    if attempt < MAX_FETCH_ATTEMPTS && err.is_retryable() {
                        let wait = match &err {
                            GraphqlFetchError::RateLimited { retry_after, .. } => {
                                (*retry_after).min(MAX_RATE_LIMIT_WAIT)
                            }
                            _ => {
                                let delay = Duration::from_millis(backoff_ms);
                                backoff_ms = backoff_ms.saturating_mul(2);
                                delay
                            }
                        };
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(map_graphql_error(err));
                }
            }
        }
        unreachable!("fetch retry loop should always return");
    }
}

fn map_graphql_error(err: GraphqlFetchError) -> anyhow::Error {
    anyhow::anyhow!(err)
}

pub fn parse_invalidation_input(row_data: &Value) -> anyhow::Result<InvalidationInput> {
    let object = row_data
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("row data must be a JSON object"))?;

    let invalidation_node_id = required_string(
        object,
        &[
            "InvalidationNodeId",
            "invalidationNodeId",
            "invalidation_node_id",
            "id",
        ],
        "invalidation node id",
    )?;
    let delivery_id = required_string(
        object,
        &[
            "DeliveryId",
            "deliveryId",
            "xGitHubDelivery",
            "xGithubDelivery",
            "githubDeliveryId",
        ],
        "delivery id",
    )?;
    let project_item_node_id = required_string(
        object,
        &[
            "ProjectItemNodeId",
            "projectItemNodeId",
            "project_item_node_id",
        ],
        "project item node id",
    )?;
    let project_node_id = optional_string(
        object,
        &["ProjectNodeId", "projectNodeId", "project_node_id"],
    );
    let status_field_node_id = optional_string(object, &["StatusFieldNodeId", "statusFieldNodeId"]);
    let state_source_url = optional_string(object, &["StateSourceUrl", "stateSourceUrl"]);
    let webhook_action = optional_string(object, &["webhookAction", "action"]);
    let webhook_updated_at = optional_timestamp(
        object,
        &[
            "InvalidatedAt",
            "invalidatedAt",
            "webhookUpdatedAt",
            "webhookUpdateTime",
            "updatedAt",
            "webhook_updated_at",
        ],
    )?;

    Ok(InvalidationInput {
        invalidation_node_id,
        delivery_id,
        project_item_node_id,
        project_node_id,
        status_field_node_id,
        state_source_url,
        webhook_action,
        webhook_updated_at,
    })
}

fn urls_match(lhs: &str, rhs: &str) -> bool {
    match (Url::parse(lhs), Url::parse(rhs)) {
        (Ok(lhs), Ok(rhs)) => {
            lhs.scheme() == rhs.scheme()
                && lhs.host_str() == rhs.host_str()
                && lhs.port_or_known_default() == rhs.port_or_known_default()
                && normalize_path(lhs.path()) == normalize_path(rhs.path())
                && lhs.query() == rhs.query()
        }
        _ => false,
    }
}

fn normalize_path(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/"
    } else {
        trimmed
    }
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    names: &[&str],
    display_name: &str,
) -> anyhow::Result<String> {
    optional_string(object, names).ok_or_else(|| anyhow::anyhow!("missing {display_name} field"))
}

fn optional_string(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| match value {
            Value::String(s) if !s.trim().is_empty() => Some(s.to_string()),
            _ => None,
        })
    })
}

fn optional_timestamp(
    object: &serde_json::Map<String, Value>,
    names: &[&str],
) -> anyhow::Result<Option<DateTime<Utc>>> {
    for name in names {
        let Some(value) = object.get(*name) else {
            continue;
        };
        match value {
            Value::Null => return Ok(None),
            Value::String(text) => {
                let parsed = DateTime::parse_from_rfc3339(text)
                    .with_context(|| format!("invalid RFC3339 timestamp for '{name}'"))?
                    .with_timezone(&Utc);
                return Ok(Some(parsed));
            }
            Value::Number(number) => {
                if let Some(unix_secs) = number.as_i64() {
                    let parsed = DateTime::<Utc>::from_timestamp(unix_secs, 0)
                        .ok_or_else(|| anyhow::anyhow!("invalid unix seconds for '{name}'"))?;
                    return Ok(Some(parsed));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}
