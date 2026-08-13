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

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io;

/// Parsed invalidation payload from a query `ADD` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidationInput {
    pub invalidation_node_id: String,
    pub delivery_id: String,
    pub project_item_node_id: String,
    pub project_node_id: Option<String>,
    pub status_field_node_id: Option<String>,
    pub state_source_url: Option<String>,
    pub webhook_action: Option<String>,
    pub webhook_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReservation {
    pub delivery_id: String,
    pub project_item_node_id: String,
    pub invalidation_node_id: String,
    pub project_node_id: Option<String>,
    pub status_field_node_id: Option<String>,
    pub state_source_url: Option<String>,
    pub webhook_action: Option<String>,
    pub webhook_updated_at: Option<DateTime<Utc>>,
    pub reserved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    Reserved,
    Fetched,
    Published,
    Stale,
    Rejected,
    Failed,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedProjectItemState {
    pub project_item_node_id: String,
    pub project_node_id: String,
    pub content_node_id: Option<String>,
    pub content_type: Option<String>,
    pub status_field_node_id: String,
    pub status_option_id: String,
    pub status_name: String,
    pub updated_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
    pub triggering_delivery_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationRecord {
    pub state: PublicationState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub fetched_state: Option<FetchedProjectItemState>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl PublicationRecord {
    pub fn reserved() -> Self {
        Self {
            state: PublicationState::Reserved,
            attempts: 0,
            last_error: None,
            fetched_state: None,
            completed_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemVersionRecord {
    pub project_item_node_id: String,
    pub project_node_id: String,
    pub status_field_node_id: String,
    pub status_option_id: String,
    pub status_name: String,
    pub updated_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
    pub triggering_delivery_id: String,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryKey {
    pub delivery_id: String,
    pub project_item_node_id: String,
}

impl DeliveryKey {
    pub fn new(delivery_id: impl Into<String>, project_item_node_id: impl Into<String>) -> Self {
        Self {
            delivery_id: delivery_id.into(),
            project_item_node_id: project_item_node_id.into(),
        }
    }

    pub fn as_storage_key(&self) -> String {
        format!("{}::{}", self.delivery_id, self.project_item_node_id)
    }
}

/// Outbound deterministic node representation posted to a standard-mode HTTP source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectItemStatusNode {
    pub id: String,
    pub project_item_node_id: String,
    pub project_node_id: String,
    pub status_field_node_id: String,
    pub status_option_id: String,
    pub status_name: String,
    pub updated_at: DateTime<Utc>,
    pub refreshed_at: DateTime<Utc>,
    pub triggering_delivery_id: String,
}

impl ProjectItemStatusNode {
    pub fn deterministic_node_id(project_item_node_id: &str) -> String {
        format!("project-item-status:{project_item_node_id}")
    }

    pub fn from_fetched(fetched: &FetchedProjectItemState) -> Self {
        Self {
            id: Self::deterministic_node_id(&fetched.project_item_node_id),
            project_item_node_id: fetched.project_item_node_id.clone(),
            project_node_id: fetched.project_node_id.clone(),
            status_field_node_id: fetched.status_field_node_id.clone(),
            status_option_id: fetched.status_option_id.clone(),
            status_name: fetched.status_name.clone(),
            updated_at: fetched.updated_at,
            refreshed_at: fetched.refreshed_at,
            triggering_delivery_id: fetched.triggering_delivery_id.clone(),
        }
    }

    pub fn updated_at_rfc3339(&self) -> String {
        self.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    pub fn refreshed_at_rfc3339(&self) -> String {
        self.refreshed_at
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
pub enum HttpSourceChange {
    Update {
        element: HttpElement,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HttpElement {
    Node {
        id: String,
        labels: Vec<String>,
        #[serde(default)]
        properties: Map<String, Value>,
    },
}

impl HttpSourceChange {
    pub fn update_project_item_status(node: &ProjectItemStatusNode) -> Result<Self, io::Error> {
        let mut properties = Map::new();
        properties.insert(
            "projectItemNodeId".to_string(),
            Value::String(node.project_item_node_id.clone()),
        );
        properties.insert(
            "projectNodeId".to_string(),
            Value::String(node.project_node_id.clone()),
        );
        properties.insert(
            "statusFieldNodeId".to_string(),
            Value::String(node.status_field_node_id.clone()),
        );
        properties.insert(
            "statusOptionId".to_string(),
            Value::String(node.status_option_id.clone()),
        );
        properties.insert(
            "statusName".to_string(),
            Value::String(node.status_name.clone()),
        );
        properties.insert(
            "updatedAt".to_string(),
            Value::String(node.updated_at_rfc3339()),
        );
        properties.insert(
            "refreshedAt".to_string(),
            Value::String(node.refreshed_at_rfc3339()),
        );
        properties.insert(
            "triggeringDeliveryId".to_string(),
            Value::String(node.triggering_delivery_id.clone()),
        );

        let timestamp = node
            .updated_at
            .timestamp_nanos_opt()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "updatedAt cannot be represented as a non-negative nanosecond timestamp",
                )
            })?;

        Ok(Self::Update {
            element: HttpElement::Node {
                id: node.id.clone(),
                labels: vec!["ProjectItemStatus".to_string()],
                properties,
            },
            timestamp: Some(timestamp),
        })
    }
}
