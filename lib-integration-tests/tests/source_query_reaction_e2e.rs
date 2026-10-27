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

//! End-to-end integration tests for the full data flow:
//!   Source → Query → Reaction
//!
//! These tests verify that:
//! - Source events flow through queries and arrive at reactions
//! - The host-managed query subscription model works correctly
//! - `enqueue_query_result()` is called on reactions with correct data
//! - Multiple queries and reactions can be wired together

use async_trait::async_trait;
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    ComponentStatus, DrasiLib, Query, Reaction, ReactionRuntimeContext, Source, SourceBase,
    SourceBaseParams, SourceRuntimeContext, SourceSubscriptionSettings, SubscriptionResponse,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
use tokio::time::timeout;

// ============================================================================
// Test Source — supports programmatic event injection via shared handle
// ============================================================================

/// Shared handle for injecting events into the source after it's been
/// passed to DrasiLib.
#[derive(Clone)]
struct SourceInjector {
    base: Arc<SourceBase>,
}

impl SourceInjector {
    async fn inject(&self, change: drasi_core::models::SourceChange) -> anyhow::Result<()> {
        self.base.dispatch_source_change(change).await
    }
}

struct InjectableSource {
    base: Arc<SourceBase>,
}

impl InjectableSource {
    fn new(id: &str) -> anyhow::Result<(Self, SourceInjector)> {
        let params = SourceBaseParams::new(id);
        let base = Arc::new(SourceBase::new(params)?);
        let injector = SourceInjector { base: base.clone() };
        Ok((Self { base }, injector))
    }
}

#[async_trait]
impl Source for InjectableSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "injectable"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "injectable")
            .await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    fn dispatch_mode(&self) -> drasi_lib::DispatchMode {
        drasi_lib::DispatchMode::Channel
    }
}

// ============================================================================
// Test Reaction — captures received QueryResults
// ============================================================================

struct CapturingReaction {
    base: drasi_lib::ReactionBase,
    captured: Arc<RwLock<Vec<QueryResult>>>,
    notify: Arc<Notify>,
}

impl CapturingReaction {
    fn new(id: &str, query_ids: Vec<String>) -> Self {
        let params = drasi_lib::ReactionBaseParams::new(id, query_ids);
        Self {
            base: drasi_lib::ReactionBase::new(params),
            captured: Arc::new(RwLock::new(Vec::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    fn captured(&self) -> Arc<RwLock<Vec<QueryResult>>> {
        self.captured.clone()
    }

    fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }
}

#[async_trait]
impl Reaction for CapturingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "capturing"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;

        // Spawn processing task that dequeues from priority queue and captures results
        let priority_queue = self.base.priority_queue.clone();
        let captured = self.captured.clone();
        let notify = self.notify.clone();
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let task = tokio::spawn(async move {
            loop {
                let result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = priority_queue.dequeue() => result,
                };
                let result = (*result_arc).clone();
                captured.write().await.push(result);
                notify.notify_waiters();
            }
        });

        self.base.set_processing_task(task).await;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

// ============================================================================
// Helper functions
// ============================================================================

async fn wait_for_status(drasi: &DrasiLib, component: &str, id: &str, expected: ComponentStatus) {
    let mut rx = drasi.subscribe_all_component_events();
    let snapshot = drasi.get_graph().await;
    if snapshot
        .nodes
        .iter()
        .any(|n| n.id == id && n.status == expected)
    {
        return;
    }
    let result = timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.component_id == id && event.status == expected => return,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Event channel closed while waiting for {component} '{id}'");
                }
                _ => continue,
            }
        }
    })
    .await;

    if result.is_err() {
        panic!("Timed out waiting for {component} '{id}' to reach {expected:?}");
    }
}

// ============================================================================
// Tests
// ============================================================================

async fn dependencies_arriving_out_of_order_activate_the_complete_pipeline(
    stop_before_source: bool,
    declare_before_start: bool,
) {
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use drasi_lib::computation::v1::{ComponentId, DependencyWait, RealizationState};

    let drasi = DrasiLib::builder().build().await.expect("instance");
    if !declare_before_start {
        drasi.start().await.expect("start instance");
    }
    let reaction = CapturingReaction::new("late-reaction", vec!["late-query".into()]);
    let captured = reaction.captured();
    let notify = reaction.notify();
    let reaction = drasi
        .add_reaction_with_handle(reaction)
        .await
        .expect("declare reaction");
    let observed = reaction.observed().expect("reaction observation");
    assert_eq!(
        observed.realization,
        RealizationState::Blocked,
        "{observed:#?}"
    );
    assert_eq!(
        observed.waiting_for,
        [DependencyWait::Component {
            id: ComponentId::try_new("late-query").expect("query ID"),
        }]
    );
    assert!(observed.failure.is_none());

    let query = drasi
        .add_query_with_handle(
            Query::cypher("late-query")
                .query("MATCH (s:Sensor) RETURN s.temperature AS temperature")
                .from_input("late-source")
                .enable_bootstrap(false)
                .build(),
        )
        .await
        .expect("declare query");
    let observed = query.observed().expect("query observation");
    assert_eq!(
        observed.realization,
        RealizationState::Blocked,
        "{observed:#?}"
    );
    assert!(observed.waiting_for.contains(&DependencyWait::Component {
        id: ComponentId::try_new("late-source").expect("source ID"),
    }));
    assert!(observed.failure.is_none());
    let saved = drasi
        .snapshot_computation_configuration()
        .await
        .expect("save waiting instance");
    assert_eq!(saved.instance.queries.len(), 1);
    if declare_before_start {
        timeout(Duration::from_secs(5), drasi.start())
            .await
            .expect("instance startup must not wait for an absent dependency")
            .expect("start with waiting declarations");
    }
    if stop_before_source {
        drasi
            .stop_query("late-query")
            .await
            .expect("withdraw pending startup");
    }

    let (source, injector) = InjectableSource::new("late-source").expect("source");
    drasi
        .add_source_with_handle(source)
        .await
        .expect("declare source");
    if stop_before_source {
        timeout(Duration::from_secs(5), query.wait_created())
            .await
            .expect("creation deadline")
            .expect("query constructs without activation");
        let control = drasi.computation_control().expect("control");
        timeout(Duration::from_secs(3), async {
            loop {
                match control
                    .preview(control.desired_snapshot().revision, Vec::new())
                    .await
                {
                    Ok(_) => break,
                    Err(drasi_lib::computation::v1::GraphError::StaleRevision { .. }) => {}
                    Err(error) => panic!("controller barrier failed: {error}"),
                }
            }
        })
        .await
        .expect("controller barrier deadline");
        assert_eq!(
            query.observed().expect("stopped query").lifecycle,
            drasi_lib::computation::v1::ComponentLifecycle::Stopped
        );
        drasi
            .start_query("late-query")
            .await
            .expect("explicitly activate the stopped query");
    }
    timeout(Duration::from_secs(5), async {
        query
            .wait_started()
            .await
            .expect("query automatically starts");
        reaction
            .wait_started()
            .await
            .expect("reaction automatically starts");
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "dependency arrival did not activate the original handles: {:#?}",
            drasi.computation_control().expect("control").observed()
        )
    });
    assert!(query.observed().expect("query").waiting_for.is_empty());
    assert!(reaction
        .observed()
        .expect("reaction")
        .waiting_for
        .is_empty());

    let metadata = ElementMetadata {
        reference: ElementReference::new("late-source", "sensor"),
        labels: Arc::from([Arc::from("Sensor")]),
        effective_from: 1000,
    };
    for (index, value) in [Some(23), Some(31), None].into_iter().enumerate() {
        let change = match value {
            Some(temperature) => {
                let element = Element::Node {
                    metadata: ElementMetadata {
                        effective_from: 1000 + index as u64,
                        ..metadata.clone()
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"temperature": temperature}),
                    ),
                };
                if index == 0 {
                    SourceChange::Insert { element }
                } else {
                    SourceChange::update(element)
                }
            }
            None => SourceChange::Delete {
                metadata: ElementMetadata {
                    effective_from: 1000 + index as u64,
                    ..metadata.clone()
                },
            },
        };
        injector.inject(change).await.expect("source change");
        timeout(Duration::from_secs(5), async {
            loop {
                let changed = notify.notified();
                if captured.read().await.len() > index {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("exact result delivery");
        let expected: Vec<_> = value
            .into_iter()
            .map(|temperature| serde_json::json!({"temperature": temperature}))
            .collect();
        assert_eq!(
            drasi
                .get_query_results("late-query")
                .await
                .expect("query snapshot"),
            expected
        );
    }
    let results = captured.read().await;
    assert_eq!(results.len(), 3);
    assert!(results
        .iter()
        .all(|result| result.query_id == "late-query" && result.results.len() == 1));
    assert!(matches!(results[0].results[0], ResultDiff::Add { .. }));
    assert!(matches!(results[1].results[0], ResultDiff::Update { .. }));
    assert!(matches!(results[2].results[0], ResultDiff::Delete { .. }));
    drop(results);
    drasi.shutdown().await.expect("complete cleanup");
}

#[tokio::test(flavor = "current_thread")]
async fn late_dependencies_activate_query_and_reaction_current_thread() {
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(false, false).await;
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(true, false).await;
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_dependencies_activate_query_and_reaction_multi_thread() {
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(false, false).await;
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(true, false).await;
    dependencies_arriving_out_of_order_activate_the_complete_pipeline(false, true).await;
}

/// Full end-to-end test: Source emits data → Query processes → Reaction receives results.
///
/// This validates the host-managed query subscription model where:
/// 1. Source dispatches SourceChange events
/// 2. Query subscribes to source, processes events through drasi-core
/// 3. The computation runtime subscribes to the query on behalf of the reaction
/// 4. QueryResults are forwarded to reaction via `enqueue_query_result()`
/// 5. Reaction's processing task receives and processes the results
#[tokio::test]
async fn test_source_to_query_to_reaction_data_flow() {
    let (source, injector) = InjectableSource::new("test-source").unwrap();

    let reaction = CapturingReaction::new("test-reaction", vec!["test-query".to_string()]);
    let captured = reaction.captured();
    let notify = reaction.notify();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("test-query")
                .query("MATCH (s:Sensor) RETURN s")
                .from_input("test-source")
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    wait_for_status(&drasi, "source", "test-source", ComponentStatus::Running).await;
    wait_for_status(&drasi, "query", "test-query", ComponentStatus::Running).await;
    wait_for_status(
        &drasi,
        "reaction",
        "test-reaction",
        ComponentStatus::Running,
    )
    .await;

    // Inject a source event — insert a Sensor node
    let element = drasi_core::models::Element::Node {
        metadata: drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference::new("test-source", "sensor-1"),
            labels: Arc::from(vec![Arc::from("Sensor")]),
            effective_from: 1000,
        },
        properties: drasi_core::models::ElementPropertyMap::from(serde_json::json!({
            "id": "sensor-1",
            "temperature": 85.0
        })),
    };

    let change = drasi_core::models::SourceChange::Insert { element };

    injector.inject(change).await.unwrap();

    // Wait for the result to flow through query → reaction
    let received = timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            {
                let results = captured.read().await;
                if !results.is_empty() {
                    return results.clone();
                }
            }
            notified.await;
        }
    })
    .await
    .expect("Timed out waiting for reaction to receive query results");

    // Verify the reaction received the correct data
    assert_eq!(received.len(), 1, "Should receive exactly one QueryResult");
    let result = &received[0];
    assert_eq!(result.query_id, "test-query");
    assert_eq!(result.results.len(), 1, "Should have one result diff");

    match &result.results[0] {
        ResultDiff::Add { data, .. } => {
            assert!(
                data.get("s").is_some(),
                "Result should contain variable 's' from RETURN clause"
            );
        }
        other => panic!("Expected Add result diff, got: {other:?}"),
    }

    let _ = drasi.stop().await;
}

/// Test that multiple events flow through correctly.
#[tokio::test]
async fn test_multiple_events_flow_through() {
    let (source, injector) = InjectableSource::new("multi-src").unwrap();

    let reaction = CapturingReaction::new("multi-rx", vec!["multi-query".to_string()]);
    let captured = reaction.captured();
    let notify = reaction.notify();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("multi-query")
                .query("MATCH (s:Sensor) RETURN s")
                .from_input("multi-src")
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    wait_for_status(&drasi, "source", "multi-src", ComponentStatus::Running).await;
    wait_for_status(&drasi, "query", "multi-query", ComponentStatus::Running).await;
    wait_for_status(&drasi, "reaction", "multi-rx", ComponentStatus::Running).await;

    // Inject 3 sensor nodes
    for i in 1..=3 {
        let element = drasi_core::models::Element::Node {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new(
                    "multi-src",
                    &format!("sensor-{i}"),
                ),
                labels: Arc::from(vec![Arc::from("Sensor")]),
                effective_from: (1000 + i) as u64,
            },
            properties: drasi_core::models::ElementPropertyMap::from(serde_json::json!({
                "id": format!("sensor-{i}")
            })),
        };

        injector
            .inject(drasi_core::models::SourceChange::Insert { element })
            .await
            .unwrap();
    }

    // Wait for all 3 results
    let received = timeout(Duration::from_secs(5), async {
        loop {
            let notified = notify.notified();
            {
                let results = captured.read().await;
                if results.len() >= 3 {
                    return results.clone();
                }
            }
            notified.await;
        }
    })
    .await
    .expect("Timed out waiting for 3 query results");

    assert_eq!(received.len(), 3, "Should receive 3 QueryResults");
    for result in &received {
        assert_eq!(result.query_id, "multi-query");
        assert!(!result.results.is_empty());
    }

    let _ = drasi.stop().await;
}

/// Test that aggregation query results are stored in current_results and accessible
/// via get_query_results().
///
/// This validates that when a continuous query produces `Aggregation` results,
/// they are stored in the query's result set — not silently discarded.
#[tokio::test]
async fn test_aggregation_results_stored_in_current_results() {
    let (source, injector) = InjectableSource::new("agg-src").unwrap();

    let reaction = CapturingReaction::new("agg-rx", vec!["agg-query".to_string()]);
    let captured = reaction.captured();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("agg-query")
                .query(
                    "MATCH (p:Product)<-[:REVIEWED]-(r:Review) \
                     RETURN p.name AS product_name, count(r) AS review_count",
                )
                .from_input("agg-src")
                .build(),
        )
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    wait_for_status(&drasi, "source", "agg-src", ComponentStatus::Running).await;
    wait_for_status(&drasi, "query", "agg-query", ComponentStatus::Running).await;
    wait_for_status(&drasi, "reaction", "agg-rx", ComponentStatus::Running).await;

    // Insert a Product node
    let product = drasi_core::models::Element::Node {
        metadata: drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference::new("agg-src", "p1"),
            labels: Arc::from(vec![Arc::from("Product")]),
            effective_from: 1000,
        },
        properties: drasi_core::models::ElementPropertyMap::from(serde_json::json!({
            "name": "Widget"
        })),
    };
    injector
        .inject(drasi_core::models::SourceChange::Insert { element: product })
        .await
        .unwrap();

    // Insert a Review node
    let review = drasi_core::models::Element::Node {
        metadata: drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference::new("agg-src", "r1"),
            labels: Arc::from(vec![Arc::from("Review")]),
            effective_from: 1001,
        },
        properties: drasi_core::models::ElementPropertyMap::from(serde_json::json!({
            "rating": 5
        })),
    };
    injector
        .inject(drasi_core::models::SourceChange::Insert { element: review })
        .await
        .unwrap();

    // Insert the REVIEWED relationship: (r1)-[:REVIEWED]->(p1)
    // in_node = start of arrow = r1, out_node = end of arrow = p1
    let rel = drasi_core::models::Element::Relation {
        metadata: drasi_core::models::ElementMetadata {
            reference: drasi_core::models::ElementReference::new("agg-src", "r1-p1"),
            labels: Arc::from(vec![Arc::from("REVIEWED")]),
            effective_from: 1002,
        },
        properties: drasi_core::models::ElementPropertyMap::default(),
        in_node: drasi_core::models::ElementReference::new("agg-src", "r1"),
        out_node: drasi_core::models::ElementReference::new("agg-src", "p1"),
    };
    injector
        .inject(drasi_core::models::SourceChange::Insert { element: rel })
        .await
        .unwrap();

    // Wait for the aggregation result to reach the reaction
    let _received = timeout(Duration::from_secs(5), async {
        loop {
            let results = captured.read().await;
            let has_agg = results.iter().any(|r| {
                r.results
                    .iter()
                    .any(|d| matches!(d, ResultDiff::Aggregation { .. }))
            });
            if has_agg {
                return results.clone();
            }
            drop(results);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Timed out waiting for aggregation results to reach reaction");

    // Now verify that get_query_results() returns the aggregation data.
    // This is the key assertion: aggregation results must be stored in current_results.
    let query_results = drasi.get_query_results("agg-query").await.unwrap();

    assert!(
        !query_results.is_empty(),
        "get_query_results() should return aggregation results, but got empty"
    );

    // Verify the aggregation result contains the expected data
    let result = &query_results[0];
    assert_eq!(
        result.get("product_name").and_then(|v| v.as_str()),
        Some("Widget"),
        "Aggregation result should contain product_name='Widget'"
    );
    assert_eq!(
        result.get("review_count").and_then(|v| v.as_i64()),
        Some(1),
        "Aggregation result should have review_count=1"
    );

    let _ = drasi.stop().await;
}
