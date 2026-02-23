// Copyright 2025 The Drasi Authors.
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

//! Introspection Source
//!
//! A built-in source that models Drasi's own component state as a graph.
//! Queries can subscribe to this source to reactively observe component
//! topology and lifecycle changes — no polling needed.
//!
//! # Data Model
//!
//! **Nodes:** `DrasiInstance`, `Source`, `Query`, `Reaction`
//! **Relations:** `HAS_SOURCE`, `HAS_QUERY`, `HAS_REACTION`, `SUBSCRIBES_TO`, `LISTENS_TO`
//!
//! # Element IDs
//!
//! - Nodes: `instance:{id}`, `source:{id}`, `query:{id}`, `reaction:{id}`
//! - Relations: `rel:has_source:{instance}:{source}`, `rel:subscribes:{query}:{source}`, etc.

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use crate::channels::*;
use crate::config::SourceSubscriptionSettings;
use crate::context::SourceRuntimeContext;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::base::{SourceBase, SourceBaseParams};
use crate::sources::manager::SourceManager;
use crate::sources::Source;

/// Well-known source ID for the introspection source
pub const INTROSPECTION_SOURCE_ID: &str = "__introspection__";

/// Create a node Element with the given label, element_id, and properties
fn make_node(element_id: &str, labels: &[&str], props: ElementPropertyMap) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(INTROSPECTION_SOURCE_ID, element_id),
            labels: labels.iter().map(|l| Arc::from(*l)).collect::<Vec<_>>().into(),
            effective_from: now_ms(),
        },
        properties: props,
    }
}

/// Create a relation Element
fn make_relation(
    element_id: &str,
    labels: &[&str],
    in_node_id: &str,
    out_node_id: &str,
    props: ElementPropertyMap,
) -> Element {
    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(INTROSPECTION_SOURCE_ID, element_id),
            labels: labels.iter().map(|l| Arc::from(*l)).collect::<Vec<_>>().into(),
            effective_from: now_ms(),
        },
        in_node: ElementReference::new(INTROSPECTION_SOURCE_ID, in_node_id),
        out_node: ElementReference::new(INTROSPECTION_SOURCE_ID, out_node_id),
        properties: props,
    }
}

/// Create an ElementMetadata for delete operations
fn make_delete_metadata(element_id: &str, labels: &[&str]) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(INTROSPECTION_SOURCE_ID, element_id),
        labels: labels.iter().map(|l| Arc::from(*l)).collect::<Vec<_>>().into(),
        effective_from: now_ms(),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn status_str(status: &ComponentStatus) -> &'static str {
    match status {
        ComponentStatus::Stopped => "Stopped",
        ComponentStatus::Starting => "Starting",
        ComponentStatus::Running => "Running",
        ComponentStatus::Stopping => "Stopping",
        ComponentStatus::Reconfiguring => "Reconfiguring",
        ComponentStatus::Error => "Error",
        ComponentStatus::Added => "Added",
        ComponentStatus::Removed => "Removed",
    }
}

fn build_props(pairs: &[(&str, &str)]) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    for (k, v) in pairs {
        props.insert(k, ElementValue::String(Arc::from(*v)));
    }
    props
}

// --------------------------------------------------------------------------
// IntrospectionBootstrapProvider
// --------------------------------------------------------------------------

/// Bootstrap provider that snapshots current Drasi state as graph elements.
pub struct IntrospectionBootstrapProvider {
    instance_id: String,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
}

impl IntrospectionBootstrapProvider {
    pub fn new(
        instance_id: String,
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
    ) -> Self {
        Self {
            instance_id,
            source_manager,
            query_manager,
            reaction_manager,
        }
    }
}

#[async_trait]
impl BootstrapProvider for IntrospectionBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        _context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Introspection bootstrap for query '{}' starting",
            request.query_id
        );

        let mut count: u64 = 0;

        // 1. DrasiInstance node
        let instance_node_id = format!("instance:{}", self.instance_id);
        let instance_node = make_node(
            &instance_node_id,
            &["DrasiInstance"],
            build_props(&[("id", &self.instance_id), ("running", "true")]),
        );
        let _ = event_tx
            .send(BootstrapEvent {
                source_id: INTROSPECTION_SOURCE_ID.to_string(),
                change: SourceChange::Insert {
                    element: instance_node,
                },
                timestamp: chrono::Utc::now(),
                sequence: count,
            })
            .await;
        count += 1;

        // 2. Source nodes + HAS_SOURCE relations
        let sources = self.source_manager.list_sources().await;
        for (source_id, status) in &sources {
            // Skip the introspection source itself to avoid self-reference
            if source_id == INTROSPECTION_SOURCE_ID {
                continue;
            }

            let node_id = format!("source:{source_id}");
            let source_runtime = self.source_manager.get_source(source_id.clone()).await;
            let (kind, auto_start) = match &source_runtime {
                Ok(rt) => (rt.source_type.clone(), "true".to_string()),
                Err(_) => ("unknown".to_string(), "true".to_string()),
            };

            let node = make_node(
                &node_id,
                &["Source"],
                build_props(&[
                    ("id", source_id),
                    ("kind", &kind),
                    ("status", status_str(status)),
                    ("autoStart", &auto_start),
                ]),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: node },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;

            // HAS_SOURCE relation: Instance -> Source
            let rel_id = format!("rel:has_source:{}:{}", self.instance_id, source_id);
            let rel = make_relation(
                &rel_id,
                &["HAS_SOURCE"],
                &instance_node_id,
                &node_id,
                ElementPropertyMap::new(),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: rel },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;
        }

        // 3. Query nodes + HAS_QUERY relations + SUBSCRIBES_TO relations
        let queries = self.query_manager.list_queries().await;
        for (query_id, status) in &queries {
            let node_id = format!("query:{query_id}");

            let mut query_text = String::new();
            let mut source_ids: Vec<String> = Vec::new();
            if let Some(config) = self.query_manager.get_query_config(query_id).await {
                query_text = config.query.clone();
                source_ids = config.sources.iter().map(|s| s.source_id.clone()).collect();
            }

            let node = make_node(
                &node_id,
                &["Query"],
                build_props(&[
                    ("id", query_id),
                    ("status", status_str(status)),
                    ("query", &query_text),
                ]),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: node },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;

            // HAS_QUERY relation
            let rel_id = format!("rel:has_query:{}:{}", self.instance_id, query_id);
            let rel = make_relation(
                &rel_id,
                &["HAS_QUERY"],
                &instance_node_id,
                &node_id,
                ElementPropertyMap::new(),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: rel },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;

            // SUBSCRIBES_TO relations: Query -> Source
            for sid in &source_ids {
                if sid == INTROSPECTION_SOURCE_ID {
                    continue;
                }
                let sub_rel_id = format!("rel:subscribes:{query_id}:{sid}");
                let source_node_id = format!("source:{sid}");
                let sub_rel = make_relation(
                    &sub_rel_id,
                    &["SUBSCRIBES_TO"],
                    &node_id,
                    &source_node_id,
                    ElementPropertyMap::new(),
                );
                let _ = event_tx
                    .send(BootstrapEvent {
                        source_id: INTROSPECTION_SOURCE_ID.to_string(),
                        change: SourceChange::Insert { element: sub_rel },
                        timestamp: chrono::Utc::now(),
                        sequence: count,
                    })
                    .await;
                count += 1;
            }
        }

        // 4. Reaction nodes + HAS_REACTION relations + LISTENS_TO relations
        let reactions = self.reaction_manager.list_reactions().await;
        for (reaction_id, status) in &reactions {
            let node_id = format!("reaction:{reaction_id}");

            let runtime = self
                .reaction_manager
                .get_reaction(reaction_id.clone())
                .await;
            let (kind, query_ids) = match &runtime {
                Ok(rt) => (rt.reaction_type.clone(), rt.queries.clone()),
                Err(_) => ("unknown".to_string(), vec![]),
            };

            let node = make_node(
                &node_id,
                &["Reaction"],
                build_props(&[
                    ("id", reaction_id),
                    ("kind", &kind),
                    ("status", status_str(status)),
                ]),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: node },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;

            // HAS_REACTION relation
            let rel_id = format!("rel:has_reaction:{}:{}", self.instance_id, reaction_id);
            let rel = make_relation(
                &rel_id,
                &["HAS_REACTION"],
                &instance_node_id,
                &node_id,
                ElementPropertyMap::new(),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: INTROSPECTION_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: rel },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;

            // LISTENS_TO relations: Reaction -> Query
            for qid in &query_ids {
                let listen_rel_id = format!("rel:listens:{reaction_id}:{qid}");
                let query_node_id = format!("query:{qid}");
                let listen_rel = make_relation(
                    &listen_rel_id,
                    &["LISTENS_TO"],
                    &node_id,
                    &query_node_id,
                    ElementPropertyMap::new(),
                );
                let _ = event_tx
                    .send(BootstrapEvent {
                        source_id: INTROSPECTION_SOURCE_ID.to_string(),
                        change: SourceChange::Insert {
                            element: listen_rel,
                        },
                        timestamp: chrono::Utc::now(),
                        sequence: count,
                    })
                    .await;
                count += 1;
            }
        }

        info!(
            "Introspection bootstrap complete: {} elements for query '{}'",
            count, request.query_id
        );
        Ok(count as usize)
    }
}

// --------------------------------------------------------------------------
// IntrospectionSource
// --------------------------------------------------------------------------

/// Built-in source that models Drasi component state as a graph.
///
/// On `start()`, subscribes to the component event broadcast channel and
/// translates `ComponentEvent`s into `SourceChange` Insert/Update/Delete
/// operations on graph nodes and relations.
pub struct IntrospectionSource {
    base: SourceBase,
    /// Broadcast sender for subscribing to all component events
    broadcast_tx: ComponentEventBroadcastSender,
    /// Instance ID for building element IDs
    instance_id: String,
}

impl IntrospectionSource {
    pub fn new(
        broadcast_tx: ComponentEventBroadcastSender,
        instance_id: String,
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
    ) -> Result<Self> {
        let params = SourceBaseParams::new(INTROSPECTION_SOURCE_ID)
            .with_dispatch_mode(DispatchMode::Broadcast)
            .with_auto_start(true)
            .with_bootstrap_provider(IntrospectionBootstrapProvider::new(
                instance_id.clone(),
                source_manager,
                query_manager,
                reaction_manager,
            ));

        Ok(Self {
            base: SourceBase::new(params)?,
            broadcast_tx,
            instance_id,
        })
    }
}

#[async_trait]
impl Source for IntrospectionSource {
    fn id(&self) -> &str {
        INTROSPECTION_SOURCE_ID
    }

    fn type_name(&self) -> &str {
        "introspection"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "instance_id".to_string(),
            serde_json::json!(self.instance_id),
        );
        props
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Broadcast
    }

    fn auto_start(&self) -> bool {
        true
    }

    async fn start(&self) -> Result<()> {
        info!("Starting introspection source for instance '{}'", self.instance_id);

        self.base
            .set_status_with_event(ComponentStatus::Starting, Some("Starting introspection source".to_string()))
            .await?;

        // Subscribe to the broadcast channel
        let mut rx = self.broadcast_tx.subscribe();
        let base = self.base.clone_shared();
        let instance_id = self.instance_id.clone();

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let handle = tokio::spawn(async move {
            debug!("Introspection source event loop started");

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("Introspection source received shutdown signal");
                        break;
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(event) => {
                                if let Err(e) = handle_component_event(&base, &instance_id, &event).await {
                                    warn!("Introspection source failed to process event: {e}");
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("Introspection source lagged by {n} events");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                info!("Introspection source broadcast channel closed");
                                break;
                            }
                        }
                    }
                }
            }

            debug!("Introspection source event loop ended");
        });

        self.base.set_task_handle(handle).await;
        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Introspection source running".to_string()))
            .await?;

        info!("Introspection source started for instance '{}'", self.instance_id);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "introspection")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

// --------------------------------------------------------------------------
// Event → SourceChange mapping
// --------------------------------------------------------------------------

/// Convert a ComponentEvent into SourceChange operations and dispatch them.
async fn handle_component_event(
    base: &SourceBase,
    instance_id: &str,
    event: &ComponentEvent,
) -> Result<()> {
    // Skip events from the introspection source itself
    if event.component_id == INTROSPECTION_SOURCE_ID {
        return Ok(());
    }

    let changes = match event.status {
        ComponentStatus::Added => build_added_changes(instance_id, event),
        ComponentStatus::Removed => build_removed_changes(instance_id, event),
        _ => build_status_update_changes(event),
    };

    for change in changes {
        base.dispatch_source_change(change).await?;
    }

    Ok(())
}

/// Build Insert changes when a component is added
fn build_added_changes(instance_id: &str, event: &ComponentEvent) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let (label, prefix) = component_label_prefix(&event.component_type);

    let node_id = format!("{prefix}:{}", event.component_id);
    let node = make_node(
        &node_id,
        &[label],
        build_props(&[
            ("id", &event.component_id),
            ("status", "Stopped"),
        ]),
    );
    changes.push(SourceChange::Insert { element: node });

    // HAS_* relation: Instance -> Component
    let instance_node_id = format!("instance:{instance_id}");
    let has_label = match event.component_type {
        ComponentType::Source => "HAS_SOURCE",
        ComponentType::Query => "HAS_QUERY",
        ComponentType::Reaction => "HAS_REACTION",
    };
    let rel_prefix = match event.component_type {
        ComponentType::Source => "has_source",
        ComponentType::Query => "has_query",
        ComponentType::Reaction => "has_reaction",
    };
    let rel_id = format!("rel:{rel_prefix}:{instance_id}:{}", event.component_id);
    let rel = make_relation(
        &rel_id,
        &[has_label],
        &instance_node_id,
        &node_id,
        ElementPropertyMap::new(),
    );
    changes.push(SourceChange::Insert { element: rel });

    changes
}

/// Build Delete changes when a component is removed
fn build_removed_changes(instance_id: &str, event: &ComponentEvent) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let (label, prefix) = component_label_prefix(&event.component_type);

    // Delete the HAS_* relation first
    let rel_prefix = match event.component_type {
        ComponentType::Source => "has_source",
        ComponentType::Query => "has_query",
        ComponentType::Reaction => "has_reaction",
    };
    let has_label = match event.component_type {
        ComponentType::Source => "HAS_SOURCE",
        ComponentType::Query => "HAS_QUERY",
        ComponentType::Reaction => "HAS_REACTION",
    };
    let rel_id = format!("rel:{rel_prefix}:{instance_id}:{}", event.component_id);
    changes.push(SourceChange::Delete {
        metadata: make_delete_metadata(&rel_id, &[has_label]),
    });

    // Delete the component node
    let node_id = format!("{prefix}:{}", event.component_id);
    changes.push(SourceChange::Delete {
        metadata: make_delete_metadata(&node_id, &[label]),
    });

    changes
}

/// Build Update changes for status transitions
fn build_status_update_changes(event: &ComponentEvent) -> Vec<SourceChange> {
    let (label, prefix) = component_label_prefix(&event.component_type);
    let node_id = format!("{prefix}:{}", event.component_id);

    let mut props = build_props(&[
        ("id", &event.component_id),
        ("status", status_str(&event.status)),
    ]);

    // Include error message if present
    if let Some(ref msg) = event.message {
        if event.status == ComponentStatus::Error {
            props.insert("error", ElementValue::String(Arc::from(msg.as_str())));
        }
    }

    let node = make_node(&node_id, &[label], props);
    vec![SourceChange::Update { element: node }]
}

fn component_label_prefix(ct: &ComponentType) -> (&'static str, &'static str) {
    match ct {
        ComponentType::Source => ("Source", "source"),
        ComponentType::Query => ("Query", "query"),
        ComponentType::Reaction => ("Reaction", "reaction"),
    }
}
