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

#![cfg(feature = "computation")]
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{computation::v1::*, DrasiLib, Reaction, StateStoreProvider};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn endpoint(node: &str, name: &str) -> Endpoint {
    Endpoint::new(id(node), port(name))
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource")
}
struct Bootstrap;
#[async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "one"),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 1000,
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"name":"bootstrapped"}),
                    ),
                },
            },
            StreamId::try_new("bootstrap/out")?,
            1,
            None,
        )?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(envelope)])),
            watermarks: Vec::new(),
        })
    }
}
struct IdleSource(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for IdleSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for IdleSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}
async fn make_graph(
    host: Arc<ReactionPluginHost>,
    catalog: QueryResultsCatalog,
    services: &LegacyPluginServices,
) -> ComputationGraph {
    make_graph_query(
        host,
        catalog,
        services,
        "MATCH (n:Item) RETURN n.name AS name",
    )
    .await
}
async fn make_graph_query(
    host: Arc<ReactionPluginHost>,
    catalog: QueryResultsCatalog,
    services: &LegacyPluginServices,
    text: &str,
) -> ComputationGraph {
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "reactions".into(),
            id: id("query"),
            query: text.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .expect("query")
    .with_bootstrap(Arc::new(Bootstrap))
    .with_result_catalog(&catalog)
    .expect("catalogue");
    let factory = Arc::new(ReactionPluginAdapterFactory::default());
    let mut builder = services
        .declare(ComputationGraph::builder("reactions"))
        .expect("services")
        .source(Box::new(IdleSource(
            ComponentDescriptor::try_new(
                id("source"),
                vec![PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source"),
        )))
        .query(Box::new(query))
        .component(
            ComponentSpecification {
                descriptor: ComponentDescriptor::try_new(
                    id("sink"),
                    vec![PortDescriptor::new(
                        port("in"),
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )
                .expect("sink"),
                role: ComponentRole::Sink,
                completion: Some(SinkCompletion::Accepted),
                implementation: factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: [
                    (Arc::from("reaction"), vec![resource("reaction")]),
                    (Arc::from("catalog"), vec![resource("catalog")]),
                ]
                .into_iter()
                .chain(services.dependencies())
                .collect(),
            },
            factory,
        )
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").expect("stream"),
        )
        .bind_stream(
            endpoint("query", "out"),
            StreamId::try_new("query/out").expect("stream"),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    for (name, role, ownership, handle) in [
        (
            "reaction",
            ResourceRole::LegacyReaction,
            ResourceOwnership::Graph,
            host.resource(),
        ),
        (
            "catalog",
            ResourceRole::QueryCatalog,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(ResourceRole::QueryCatalog, Arc::new(catalog)),
        ),
    ] {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource(name),
                role,
                ownership,
                binding: Arc::from(name),
            })
            .expect("declare")
            .provide_resource(resource(name), handle)
            .expect("provide");
    }
    builder.build().expect("graph")
}

#[tokio::test]
async fn existing_snapshot_plugin_uses_native_query_snapshot_and_bootstrap_contexts() {
    let temp = tempfile::tempdir().expect("temp");
    let state = Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(temp.path().join("state.redb"))
            .expect("store"),
    );
    let drasi = DrasiLib::builder()
        .with_id("reaction-instance")
        .with_state_store_provider(state.clone())
        .build()
        .await
        .expect("instance");
    let services = drasi
        .computation_plugin_services("reactions")
        .expect("services");
    let catalog = QueryResultsCatalog::new("reactions").expect("catalogue");
    let (reaction, report) =
        drasi_reaction_snapshot_test::SnapshotTestReaction::new("snapshot", vec!["query".into()]);
    let host = ReactionPluginHost::owned(
        Box::new(reaction),
        services.clone(),
        catalog.clone(),
        ReactionPluginOptions {
            bootstrap_timeout_secs: 5,
            ..Default::default()
        },
    )
    .expect("host");
    let graph = make_graph(host, catalog, &services).await;
    let managed = drasi
        .add_computation_graph(graph, ComputationOptions { auto_start: false })
        .await
        .expect("register");
    let started = tokio::time::timeout(Duration::from_secs(10), managed.start())
        .await
        .expect("startup deadlock")
        .expect("start");
    assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
    {
        let report = report.lock().await;
        assert!(report.checkpoint_written);
        assert!(!report.snapshot_rows.is_empty());
        assert!(report
            .snapshot_rows
            .iter()
            .all(|row| row == &serde_json::json!({"name":"bootstrapped"})));
    }
    assert!(services
        .state_store
        .as_ref()
        .expect("state")
        .get("snapshot", "checkpoint:query")
        .await
        .expect("checkpoint")
        .is_some());
    assert!(state
        .get("snapshot", "checkpoint:query")
        .await
        .expect("legacy namespace")
        .is_none());
    drasi.shutdown().await.expect("shutdown");
}

struct FailingBootstrap {
    failure: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Reaction for FailingBootstrap {
    fn id(&self) -> &str {
        "failing"
    }
    fn type_name(&self) -> &str {
        "failing-bootstrap"
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        panic!("desired state must not inspect resolved plugin properties");
    }
    fn query_ids(&self) -> Vec<String> {
        vec!["query".into()]
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }
    fn default_recovery_policy(&self) -> drasi_lib::ReactionRecoveryPolicy {
        drasi_lib::ReactionRecoveryPolicy::AutoReset
    }
    async fn initialize(&self, _: drasi_lib::ReactionRuntimeContext) {}
    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn status(&self) -> drasi_lib::ComponentStatus {
        drasi_lib::ComponentStatus::Running
    }
    async fn bootstrap(
        &self,
        context: drasi_lib::reactions::bootstrap_context::BootstrapContext,
    ) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = context.fetch_snapshot().await?;
        context
            .write_checkpoint(&drasi_lib::reactions::checkpoint::ReactionCheckpoint {
                sequence: snapshot.as_of_sequence,
                config_hash: snapshot.config_hash,
            })
            .await?;
        if self.failure.swap(false, Ordering::AcqRel) {
            anyhow::bail!("bootstrap failed after staging checkpoint");
        }
        Ok(())
    }
}

#[tokio::test]
async fn failed_plugin_bootstrap_cannot_persist_a_staged_checkpoint() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let services = drasi
        .computation_plugin_services("reactions")
        .expect("services");
    let catalog = QueryResultsCatalog::new("reactions").expect("catalogue");
    let calls = Arc::new(AtomicUsize::new(0));
    let reaction = FailingBootstrap {
        failure: Arc::new(AtomicBool::new(true)),
        calls: calls.clone(),
    };
    let host = ReactionPluginHost::owned(
        Box::new(reaction),
        services.clone(),
        catalog.clone(),
        ReactionPluginOptions {
            bootstrap_timeout_secs: 5,
            ..Default::default()
        },
    )
    .expect("host");
    let managed = drasi
        .add_computation_graph(
            make_graph(host, catalog, &services).await,
            ComputationOptions { auto_start: false },
        )
        .await
        .expect("register");
    assert_eq!(
        managed.start().await.expect("partial start").summary,
        OperationSummary::CompletedWithFailures
    );
    assert!(services
        .state_store
        .as_ref()
        .expect("state")
        .get("failing", "checkpoint:query")
        .await
        .expect("checkpoint")
        .is_none());
    managed.stop().await.expect("stop failed attempt");
    assert_eq!(
        managed.start().await.expect("retry").summary,
        OperationSummary::Completed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(services
        .state_store
        .as_ref()
        .expect("state")
        .get("failing", "checkpoint:query")
        .await
        .expect("checkpoint")
        .is_some());
    drasi.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn existing_snapshot_reaction_accepts_empty_and_aggregate_native_snapshots() {
    for (text, expected) in [
        (
            "MATCH (n:Item) WHERE n.name = 'absent' RETURN n.name AS name",
            None,
        ),
        (
            "MATCH (n:Item) RETURN count(n) AS total",
            Some(serde_json::json!({"total":1})),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let drasi = DrasiLib::builder()
            .with_state_store_provider(Arc::new(
                drasi_state_store_redb::RedbStateStoreProvider::new(temp.path().join("state.redb"))
                    .expect("state"),
            ))
            .build()
            .await
            .expect("instance");
        let services = drasi
            .computation_plugin_services("reactions")
            .expect("services");
        let catalog = QueryResultsCatalog::new("reactions").expect("catalogue");
        let (reaction, report) = drasi_reaction_snapshot_test::SnapshotTestReaction::new(
            "snapshot",
            vec!["query".into()],
        );
        let host = ReactionPluginHost::owned(
            Box::new(reaction),
            services.clone(),
            catalog.clone(),
            ReactionPluginOptions::default(),
        )
        .expect("host");
        let graph = make_graph_query(host, catalog, &services, text).await;
        let handle = drasi
            .add_computation_graph(graph, ComputationOptions { auto_start: false })
            .await
            .expect("register");
        let started = tokio::time::timeout(Duration::from_secs(5), handle.start())
            .await
            .expect("bootstrap deadline")
            .expect("start");
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        {
            let report = report.lock().await;
            assert!(report.checkpoint_written);
            if let Some(expected) = expected {
                assert!(!report.snapshot_rows.is_empty());
                assert!(report.snapshot_rows.iter().all(|row| row == &expected));
            } else {
                assert!(report.snapshot_rows.is_empty());
            }
        }
        drasi.shutdown().await.expect("shutdown");
    }
}

#[derive(Clone, Default)]
struct MaterializedReaction {
    rows: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    resets: Arc<std::sync::Mutex<Vec<bool>>>,
}
#[async_trait]
impl Reaction for MaterializedReaction {
    fn id(&self) -> &str {
        "materialized"
    }
    fn type_name(&self) -> &str {
        "materialized-fixture"
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        panic!("never reconstruct desired configuration from resolved properties");
    }
    fn query_ids(&self) -> Vec<String> {
        vec!["query".into()]
    }
    fn is_durable(&self) -> bool {
        true
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }
    fn default_recovery_policy(&self) -> drasi_lib::ReactionRecoveryPolicy {
        drasi_lib::ReactionRecoveryPolicy::AutoReset
    }
    async fn initialize(&self, _: drasi_lib::ReactionRuntimeContext) {}
    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn status(&self) -> drasi_lib::ComponentStatus {
        drasi_lib::ComponentStatus::Running
    }
    async fn bootstrap(
        &self,
        context: drasi_lib::reactions::bootstrap_context::BootstrapContext,
    ) -> anyhow::Result<()> {
        let snapshot = context.fetch_snapshot().await?;
        let checkpoint = drasi_lib::reactions::checkpoint::ReactionCheckpoint {
            sequence: snapshot.as_of_sequence,
            config_hash: snapshot.config_hash,
        };
        let replacement = snapshot.collect_vec().await;
        self.resets.lock().expect("resets").push(context.is_reset);
        {
            let mut rows = self.rows.lock().expect("rows");
            if context.is_reset {
                rows.clear();
            }
            rows.extend(replacement);
        }
        context.write_checkpoint(&checkpoint).await
    }
}

fn named_change(name: &str, sequence: u64, delete: bool) -> InputEnvelope {
    let metadata = ElementMetadata {
        reference: ElementReference::new("source", name),
        labels: Arc::from([Arc::from("Item")]),
        effective_from: 1000,
    };
    let change = if delete {
        SourceChange::Delete { metadata }
    } else {
        SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties: ElementPropertyMap::from(serde_json::json!({"name":name})),
            },
        }
    };
    InputEnvelope {
        port: port("in"),
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("source/out").expect("stream"),
            sequence,
            None,
        )
        .expect("event"),
    }
}
struct NamedBootstrap(&'static str);
#[async_trait]
impl ComputationBootstrapProvider for NamedBootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(
                named_change(self.0, 1, false).envelope
            )])),
            watermarks: vec![],
        })
    }
}
async fn snapshot_query(
    catalog: &QueryResultsCatalog,
    name: &'static str,
) -> ContinuousQueryTransformer {
    ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "reactions".into(),
            id: id("query"),
            query: "MATCH (n:Item) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: NonZeroUsize::new(1).expect("capacity"),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .expect("query")
    .with_bootstrap(Arc::new(NamedBootstrap(name)))
    .with_result_catalog(catalog)
    .expect("catalogue")
}
async fn materialization_instance(state: Arc<dyn StateStoreProvider>) -> DrasiLib {
    DrasiLib::builder()
        .with_id("durable-materialization")
        .with_state_store_provider(state)
        .build()
        .await
        .expect("instance")
}

#[tokio::test]
async fn persisted_reaction_checkpoint_is_reset_for_a_new_volatile_query_incarnation() {
    let temp = tempfile::tempdir().expect("temp");
    let state = Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(temp.path().join("state.redb"))
            .expect("state"),
    );
    let destination = MaterializedReaction::default();
    for name in ["A", "B"] {
        let drasi = materialization_instance(state.clone()).await;
        let services = drasi
            .computation_plugin_services("reactions")
            .expect("services");
        let catalog = QueryResultsCatalog::new("reactions").expect("catalogue");
        let mut query = snapshot_query(&catalog, name).await;
        query.start().await.expect("bootstrap native query");
        assert_eq!(
            query.results().snapshot().expect("snapshot").as_of_sequence,
            0
        );
        let host = ReactionPluginHost::owned(
            Box::new(destination.clone()),
            services,
            catalog,
            ReactionPluginOptions::default(),
        )
        .expect("reaction");
        let mut adapter = ReactionPluginAdapter::new(id("sink"), host.clone()).expect("adapter");
        adapter.start().await.expect("recover destination");
        assert_eq!(
            *destination.rows.lock().expect("rows"),
            vec![serde_json::json!({"name":name})]
        );
        adapter.stop().await.expect("stop reaction");
        host.shutdown().await.expect("host cleanup");
        query.stop().await.expect("stop query");
        drasi.shutdown().await.expect("instance cleanup");
    }
    assert_eq!(
        *destination.resets.lock().expect("resets"),
        vec![false, true]
    );
}

#[tokio::test]
async fn a_gap_after_a_sequence_zero_checkpoint_discards_prior_materialized_state() {
    let temp = tempfile::tempdir().expect("temp");
    let drasi = materialization_instance(Arc::new(
        drasi_state_store_redb::RedbStateStoreProvider::new(temp.path().join("state.redb"))
            .expect("state"),
    ))
    .await;
    let services = drasi
        .computation_plugin_services("reactions")
        .expect("services");
    let catalog = QueryResultsCatalog::new("reactions").expect("catalogue");
    let mut query = snapshot_query(&catalog, "A").await;
    query.start().await.expect("query bootstrap");
    let destination = MaterializedReaction::default();
    let host = ReactionPluginHost::owned(
        Box::new(destination.clone()),
        services.clone(),
        catalog,
        ReactionPluginOptions::default(),
    )
    .expect("host");
    let mut adapter = ReactionPluginAdapter::new(id("sink"), host.clone()).expect("adapter");
    adapter.start().await.expect("initial snapshot");
    adapter.stop().await.expect("pause reaction");
    let bytes = services
        .state_store
        .as_ref()
        .expect("state")
        .get("materialized", "checkpoint:query")
        .await
        .expect("checkpoint")
        .expect("saved checkpoint");
    let checkpoint: drasi_lib::reactions::checkpoint::ReactionCheckpoint =
        bincode::deserialize(&bytes).expect("checkpoint bytes");
    assert_eq!(checkpoint.sequence, 0);
    query
        .transform(named_change("A", 1, true))
        .await
        .expect("remove old row");
    query
        .transform(named_change("B", 2, false))
        .await
        .expect("new row evicts earlier outbox");
    assert!(
        query.results().replay(0).is_err(),
        "fixture must force a retained-history gap"
    );
    adapter.start().await.expect("gap snapshot reset");
    assert_eq!(
        *destination.rows.lock().expect("rows"),
        vec![serde_json::json!({"name":"B"})]
    );
    assert_eq!(
        *destination.resets.lock().expect("resets"),
        vec![false, true]
    );
    adapter.stop().await.expect("stop reaction");
    host.shutdown().await.expect("host cleanup");
    query.stop().await.expect("query cleanup");
    drasi.shutdown().await.expect("instance cleanup");
}
