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

//! Dashboard persistence backed by DrasiLib StateStoreProvider.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use drasi_lib::StateStoreProvider;
use log::error;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const DASHBOARD_KEY_PREFIX: &str = "dashboard:";

/// Grid layout options for a dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GridOptions {
    pub columns: u32,
    pub row_height: u32,
    pub margin: u32,
}

impl Default for GridOptions {
    fn default() -> Self {
        Self {
            columns: 12,
            row_height: 60,
            margin: 10,
        }
    }
}

/// Grid position and size for a widget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WidgetGrid {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Default for WidgetGrid {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            w: 4,
            h: 3,
        }
    }
}

/// A single dashboard widget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DashboardWidget {
    pub id: String,
    #[serde(rename = "type")]
    pub widget_type: String,
    pub title: String,
    #[serde(default)]
    pub grid: WidgetGrid,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Full dashboard configuration persisted in state store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DashboardConfig {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub grid_options: GridOptions,
    #[serde(default)]
    pub widgets: Vec<DashboardWidget>,
}

impl DashboardConfig {
    /// Create a new dashboard configuration with generated ID and timestamps.
    pub fn new(name: String, grid_options: GridOptions, widgets: Vec<DashboardWidget>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            created_at: now,
            updated_at: now,
            grid_options,
            widgets,
        }
    }

    /// Create a new dashboard configuration with a specific stable ID.
    /// Use this for predefined dashboards that should not be duplicated on restart.
    pub fn with_id(
        id: impl Into<String>,
        name: String,
        grid_options: GridOptions,
        widgets: Vec<DashboardWidget>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: id.into(),
            name,
            created_at: now,
            updated_at: now,
            grid_options,
            widgets,
        }
    }

    /// Update the dashboard's `updated_at` timestamp.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// State-store backed CRUD operations for dashboard configs.
#[derive(Clone)]
pub struct DashboardStorage {
    reaction_id: String,
    state_store: Arc<dyn StateStoreProvider>,
}

impl DashboardStorage {
    pub fn new(reaction_id: impl Into<String>, state_store: Arc<dyn StateStoreProvider>) -> Self {
        Self {
            reaction_id: reaction_id.into(),
            state_store,
        }
    }

    fn key_for(dashboard_id: &str) -> String {
        format!("{DASHBOARD_KEY_PREFIX}{dashboard_id}")
    }

    fn is_dashboard_key(key: &str) -> bool {
        key.starts_with(DASHBOARD_KEY_PREFIX)
    }

    /// List all dashboards for the current reaction, newest updated first.
    pub async fn list_dashboards(&self) -> Result<Vec<DashboardConfig>> {
        let keys = self
            .state_store
            .list_keys(&self.reaction_id)
            .await
            .with_context(|| {
                format!(
                    "failed listing dashboard keys in store '{}'",
                    self.reaction_id
                )
            })?;

        let mut dashboards = Vec::new();
        for key in keys.into_iter().filter(|key| Self::is_dashboard_key(key)) {
            let Some(bytes) = self
                .state_store
                .get(&self.reaction_id, &key)
                .await
                .with_context(|| format!("failed reading dashboard key '{key}'"))?
            else {
                continue;
            };

            match serde_json::from_slice::<DashboardConfig>(&bytes) {
                Ok(dashboard) => dashboards.push(dashboard),
                Err(err) => {
                    error!("skipping corrupted dashboard at key '{key}': {err}");
                }
            }
        }

        dashboards.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(dashboards)
    }

    /// Get a dashboard by ID.
    pub async fn get_dashboard(&self, dashboard_id: &str) -> Result<Option<DashboardConfig>> {
        let key = Self::key_for(dashboard_id);
        let maybe_bytes = self
            .state_store
            .get(&self.reaction_id, &key)
            .await
            .with_context(|| format!("failed reading dashboard '{dashboard_id}'"))?;

        let Some(bytes) = maybe_bytes else {
            return Ok(None);
        };

        let dashboard = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed deserializing dashboard '{dashboard_id}'"))?;
        Ok(Some(dashboard))
    }

    /// Save (create/update) a dashboard.
    /// If the ID is empty or whitespace-only, a new UUID is generated automatically.
    pub async fn save_dashboard(&self, mut dashboard: DashboardConfig) -> Result<DashboardConfig> {
        if dashboard.id.trim().is_empty() {
            dashboard.id = Uuid::new_v4().to_string();
            log::warn!(
                "dashboard saved without an ID — generated '{}'",
                dashboard.id
            );
        }

        dashboard.touch();
        let key = Self::key_for(&dashboard.id);
        let bytes = serde_json::to_vec(&dashboard)
            .with_context(|| format!("failed serializing dashboard '{}'", dashboard.id))?;

        self.state_store
            .set(&self.reaction_id, &key, bytes)
            .await
            .with_context(|| format!("failed writing dashboard '{}'", dashboard.id))?;

        Ok(dashboard)
    }

    /// Delete a dashboard by ID.
    pub async fn delete_dashboard(&self, dashboard_id: &str) -> Result<bool> {
        let key = Self::key_for(dashboard_id);
        self.state_store
            .delete(&self.reaction_id, &key)
            .await
            .with_context(|| format!("failed deleting dashboard '{dashboard_id}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::{MemoryStateStoreProvider, StateStoreProvider};

    fn build_widget(id: &str) -> DashboardWidget {
        DashboardWidget {
            id: id.to_string(),
            widget_type: "table".to_string(),
            title: "Widget".to_string(),
            grid: WidgetGrid::default(),
            config: serde_json::json!({"queryId": "test-query", "columns": ["name"]}),
        }
    }

    #[tokio::test]
    async fn test_dashboard_storage_crud() {
        let state_store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let storage = DashboardStorage::new("reaction-1", state_store);

        let dashboard = DashboardConfig::new(
            "Sensors".to_string(),
            GridOptions::default(),
            vec![build_widget("widget-1")],
        );

        let saved = storage
            .save_dashboard(dashboard.clone())
            .await
            .expect("save must succeed");
        assert_eq!(saved.name, "Sensors");

        let loaded = storage
            .get_dashboard(&saved.id)
            .await
            .expect("read must succeed")
            .expect("dashboard should exist");
        assert_eq!(loaded.id, saved.id);
        assert_eq!(loaded.widgets.len(), 1);

        let listed = storage.list_dashboards().await.expect("list must succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, saved.id);

        let mut updated = loaded.clone();
        updated.name = "Updated Sensors".to_string();
        let updated_saved = storage
            .save_dashboard(updated)
            .await
            .expect("update must succeed");
        assert_eq!(updated_saved.name, "Updated Sensors");
        assert!(updated_saved.updated_at >= loaded.updated_at);

        let deleted = storage
            .delete_dashboard(&saved.id)
            .await
            .expect("delete must succeed");
        assert!(deleted);

        let missing = storage
            .get_dashboard(&saved.id)
            .await
            .expect("read after delete must succeed");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_list_filters_non_dashboard_keys() {
        let state_store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let storage = DashboardStorage::new("reaction-2", Arc::clone(&state_store));

        state_store
            .set("reaction-2", "not-a-dashboard", vec![1, 2, 3])
            .await
            .expect("seed should succeed");

        let dashboard = DashboardConfig::new(
            "Only Dashboard".to_string(),
            GridOptions::default(),
            vec![build_widget("widget-2")],
        );
        storage
            .save_dashboard(dashboard)
            .await
            .expect("save must succeed");

        let listed = storage.list_dashboards().await.expect("list must succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Only Dashboard");
    }

    #[tokio::test]
    async fn test_list_skips_corrupted_dashboard_entries() {
        let state_store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let storage = DashboardStorage::new("reaction-3", Arc::clone(&state_store));

        state_store
            .set("reaction-3", "dashboard:corrupted", b"not-json".to_vec())
            .await
            .expect("seed corrupted entry should succeed");

        let dashboard = DashboardConfig::new(
            "Healthy Dashboard".to_string(),
            GridOptions::default(),
            vec![build_widget("widget-3")],
        );
        storage
            .save_dashboard(dashboard)
            .await
            .expect("save must succeed");

        let listed = storage
            .list_dashboards()
            .await
            .expect("list should succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Healthy Dashboard");
    }
}
