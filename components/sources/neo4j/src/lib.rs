#![allow(unexpected_cfgs)]
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

//! Neo4j CDC source plugin for Drasi.
//!
//! This source reads Neo4j CDC events using `db.cdc.query` and maps node and
//! relationship events to `SourceChange` records.

pub mod cdc;
pub mod config;
pub mod descriptor;
pub mod mapping;

pub use config::{CdcMode, Neo4jSourceConfig, StartCursor};

use anyhow::Result;
use async_trait::async_trait;
use cdc::{build_selectors, determine_start_cursor, now_ms, parse_source_change, persist_cursor};
use drasi_lib::channels::*;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Source;
use log::{error, info, warn};
use neo4rs::{query, BoltType, ConfigBuilder, Graph};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;

pub struct Neo4jSource {
    base: SourceBase,
    config: Neo4jSourceConfig,
    // Optional direct store injection for standalone scenarios.
    explicit_state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl Neo4jSource {
    pub fn builder(id: impl Into<String>) -> Neo4jSourceBuilder {
        Neo4jSourceBuilder::new(id)
    }

    fn state_store_override_or_context(&self) -> Option<Arc<dyn StateStoreProvider>> {
        self.explicit_state_store.clone()
    }
}

#[async_trait]
impl Source for Neo4jSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "neo4j"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "uri".to_string(),
            serde_json::Value::String(self.config.uri.clone()),
        );
        props.insert(
            "user".to_string(),
            serde_json::Value::String(self.config.user.clone()),
        );
        // Don't expose password in properties
        props.insert(
            "database".to_string(),
            serde_json::Value::String(self.config.database.clone()),
        );
        props.insert(
            "poll_interval_ms".to_string(),
            serde_json::Value::Number(self.config.poll_interval_ms.into()),
        );
        props.insert(
            "labels".to_string(),
            serde_json::Value::Array(
                self.config
                    .labels
                    .iter()
                    .map(|l| serde_json::Value::String(l.clone()))
                    .collect(),
            ),
        );
        props.insert(
            "rel_types".to_string(),
            serde_json::Value::Array(
                self.config
                    .rel_types
                    .iter()
                    .map(|r| serde_json::Value::String(r.clone()))
                    .collect(),
            ),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.config.validate()?;
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting Neo4j source".into()),
            )
            .await?;

        let graph = connect_graph(&self.config).await?;
        let selectors = build_selectors(&self.config.labels, &self.config.rel_types);

        let state_store = match self.state_store_override_or_context() {
            Some(store) => Some(store),
            None => self.base.state_store().await,
        };

        let start_cursor =
            determine_start_cursor(&graph, &self.base.id, &self.config, state_store.clone())
                .await?;

        info!(
            "Neo4j source '{}' starting from CDC cursor '{}'",
            self.base.id, start_cursor
        );

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let source_id = self.base.id.clone();
        let database = self.config.database.clone();
        let poll_interval = Duration::from_millis(self.config.poll_interval_ms);
        let start_cursor_filter = self.config.start_cursor.clone();
        let status = self.base.status.clone();
        let base = self.base.clone_shared();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "neo4j_cdc_task",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );

        let task = tokio::spawn(
            async move {
                let mut cursor = start_cursor;
                let mut ticker = tokio::time::interval(poll_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            info!("Neo4j source '{source_id}' received shutdown");
                            break;
                        }
                        _ = ticker.tick() => {
                            match poll_once(&base, &graph, &source_id, &selectors, &cursor, &start_cursor_filter).await {
                                Ok(Some(latest_cursor)) => {
                                    cursor = latest_cursor;
                                    persist_cursor(&source_id, &database, state_store.clone(), &cursor).await;
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    error!("Neo4j CDC poll failed for source '{source_id}': {e}");
                                    *status.write().await = ComponentStatus::Error;
                                }
                            }
                        }
                    }
                }
            }
            .instrument(span),
        );

        self.base.set_task_handle(task).await;
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Neo4j CDC polling started".to_string()),
            )
            .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await != ComponentStatus::Running
            && self.base.get_status().await != ComponentStatus::Error
        {
            return Ok(());
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping Neo4j source".into()),
            )
            .await?;
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "Neo4j").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

async fn poll_once(
    base: &SourceBase,
    graph: &Graph,
    source_id: &str,
    selectors: &[HashMap<String, BoltType>],
    from_cursor: &str,
    start_cursor_mode: &StartCursor,
) -> Result<Option<String>> {
    let mut stream = graph
        .execute(
            query("CALL db.cdc.query($from, $selectors) YIELD id, metadata, event RETURN id, metadata, event")
                .param("from", from_cursor)
                .param("selectors", selectors.to_vec()),
        )
        .await?;

    let mut latest_cursor: Option<String> = None;
    loop {
        match stream.next().await {
            Ok(Some(row)) => {
                let change_id: String = row.get("id")?;
                let metadata = row.get::<BoltType>("metadata").ok();
                let event = row.get::<BoltType>("event")?;

                let event_map = match event {
                    BoltType::Map(map) => map,
                    _ => {
                        latest_cursor = Some(change_id);
                        continue;
                    }
                };

                let event_time_ms = extract_event_time_ms(metadata.as_ref()).unwrap_or_else(now_ms);
                if let StartCursor::Timestamp(ts) = start_cursor_mode {
                    if *ts >= 0 && event_time_ms < *ts as u64 {
                        latest_cursor = Some(change_id);
                        continue;
                    }
                }

                match parse_source_change(source_id, &event_map, event_time_ms) {
                    Ok(Some(change)) => {
                        base.dispatch_source_change(change).await?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Skipping malformed Neo4j CDC event '{change_id}': {e}");
                    }
                }
                latest_cursor = Some(change_id);
            }
            Ok(None) => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(latest_cursor)
}

fn extract_event_time_ms(metadata: Option<&BoltType>) -> Option<u64> {
    let BoltType::Map(metadata_map) = metadata? else {
        return None;
    };
    match metadata_map.value.get("txCommitTime") {
        Some(BoltType::Integer(v)) if v.value >= 0 => Some(v.value as u64),
        _ => None,
    }
}

fn normalize_uri(uri: &str) -> String {
    uri.trim()
        .trim_start_matches("bolt+ssc://")
        .trim_start_matches("bolt+s://")
        .trim_start_matches("neo4j+ssc://")
        .trim_start_matches("neo4j+s://")
        .trim_start_matches("bolt://")
        .trim_start_matches("neo4j://")
        .to_string()
}

async fn connect_graph(config: &Neo4jSourceConfig) -> Result<Graph> {
    let neo4j_config = ConfigBuilder::default()
        .uri(normalize_uri(&config.uri))
        .user(config.user.as_str())
        .password(config.password.as_str())
        .db(config.database.as_str())
        .build()?;

    Ok(Graph::connect(neo4j_config).await?)
}

pub struct Neo4jSourceBuilder {
    id: String,
    uri: String,
    user: String,
    password: String,
    database: String,
    labels: Vec<String>,
    rel_types: Vec<String>,
    poll_interval_ms: u64,
    cdc_mode: CdcMode,
    start_cursor: StartCursor,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl Neo4jSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            uri: "bolt://localhost:7687".to_string(),
            user: "neo4j".to_string(),
            password: String::new(),
            database: "neo4j".to_string(),
            labels: Vec::new(),
            rel_types: Vec::new(),
            poll_interval_ms: 500,
            cdc_mode: CdcMode::Full,
            start_cursor: StartCursor::Now,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
            state_store: None,
        }
    }

    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = uri.into();
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn with_rel_types(mut self, rel_types: Vec<String>) -> Self {
        self.rel_types = rel_types;
        self
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval_ms = poll_interval.as_millis() as u64;
        self
    }

    pub fn with_cdc_mode(mut self, cdc_mode: CdcMode) -> Self {
        self.cdc_mode = cdc_mode;
        self
    }

    pub fn start_from_beginning(mut self) -> Self {
        self.start_cursor = StartCursor::Beginning;
        self
    }

    pub fn start_from_now(mut self) -> Self {
        self.start_cursor = StartCursor::Now;
        self
    }

    pub fn start_from_timestamp(mut self, timestamp_ms: i64) -> Self {
        self.start_cursor = StartCursor::Timestamp(timestamp_ms);
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn with_config(mut self, config: Neo4jSourceConfig) -> Self {
        self.uri = config.uri;
        self.user = config.user;
        self.password = config.password;
        self.database = config.database;
        self.labels = config.labels;
        self.rel_types = config.rel_types;
        self.poll_interval_ms = config.poll_interval_ms;
        self.cdc_mode = config.cdc_mode;
        self.start_cursor = config.start_cursor;
        self
    }

    pub fn build(self) -> Result<Neo4jSource> {
        let config = Neo4jSourceConfig {
            uri: self.uri,
            user: self.user,
            password: self.password,
            database: self.database,
            labels: self.labels,
            rel_types: self.rel_types,
            poll_interval_ms: self.poll_interval_ms,
            cdc_mode: self.cdc_mode,
            start_cursor: self.start_cursor,
        };
        config.validate()?;

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        Ok(Neo4jSource {
            base: SourceBase::new(params)?,
            config,
            explicit_state_store: self.state_store,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let source = Neo4jSource::builder("neo4j-source")
            .with_uri("bolt://localhost:7687")
            .with_user("neo4j")
            .with_password("password")
            .with_database("neo4j")
            .with_labels(vec!["Person".to_string()])
            .with_rel_types(vec!["ACTED_IN".to_string()])
            .with_poll_interval(Duration::from_millis(1000))
            .start_from_now()
            .build();

        assert!(source.is_ok());
        assert_eq!(source.unwrap().id(), "neo4j-source");
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "neo4j-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::Neo4jSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
