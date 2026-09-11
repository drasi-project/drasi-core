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

use super::*;
use crate::{
    channels::{QueryResult, SubscriptionResponse},
    config::SourceSubscriptionSettings,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    sources::{SourceBase, SourceBaseParams},
    SourceRuntimeContext,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize},
        Mutex as StdMutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

#[derive(Default)]
struct SourceControl {
    starts: AtomicUsize,
    stops: AtomicUsize,
    deprovisions: AtomicUsize,
    subscriptions: AtomicUsize,
    fail_starts: AtomicUsize,
    fail_stop: AtomicBool,
    pending_start: AtomicBool,
    fail_initialize: AtomicBool,
    pending_initialize: AtomicBool,
    entered: Notify,
    release: Notify,
}
struct ControlledSource {
    inner: SourceBase,
    control: Arc<SourceControl>,
}
impl ControlledSource {
    fn new(id: &str) -> (Self, Arc<SourceControl>) {
        let control = Arc::new(SourceControl::default());
        (
            Self {
                inner: SourceBase::new(SourceBaseParams::new(id).with_auto_start(false)).unwrap(),
                control: control.clone(),
            },
            control,
        )
    }
}
#[async_trait]
impl Source for ControlledSource {
    fn id(&self) -> &str {
        &self.inner.id
    }
    fn type_name(&self) -> &str {
        "controlled"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn auto_start(&self) -> bool {
        false
    }
    fn supports_replay(&self) -> bool {
        false
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        let updates = context.update_tx.clone();
        self.inner.initialize(context).await;
        if self.control.fail_initialize.load(Ordering::Acquire) {
            updates
                .send(ComponentUpdate::Status {
                    component_id: self.id().into(),
                    status: ComponentStatus::Error,
                    message: Some("injected initialization failure".into()),
                })
                .await
                .unwrap();
            std::future::pending::<()>().await;
        }
        if self.control.pending_initialize.load(Ordering::Acquire) {
            self.control.entered.notify_one();
            self.control.release.notified().await;
        }
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.control.starts.fetch_add(1, Ordering::AcqRel);
        if self.control.pending_start.load(Ordering::Acquire) {
            self.control.entered.notify_one();
            std::future::pending::<()>().await;
        }
        if self
            .control
            .fail_starts
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            anyhow::bail!("injected start failure");
        }
        log::info!("controlled-source-start");
        self.inner.set_status(ComponentStatus::Starting, None).await;
        self.inner.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.control.stops.fetch_add(1, Ordering::AcqRel);
        if self.control.fail_stop.load(Ordering::Acquire) {
            anyhow::bail!("injected stop failure");
        }
        self.inner.stop_common().await
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.control.deprovisions.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.inner.get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.control.subscriptions.fetch_add(1, Ordering::AcqRel);
        self.inner
            .subscribe_with_bootstrap(&settings, "Controlled")
            .await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Default)]
struct ReactionControl {
    starts: AtomicUsize,
    stops: AtomicUsize,
    snapshot: AtomicBool,
    updates: StdMutex<Option<crate::component_graph::ComponentUpdateSender>>,
}
struct ControlledReaction {
    base: ReactionBase,
    control: Arc<ReactionControl>,
    output: mpsc::UnboundedSender<QueryResult>,
}
impl ControlledReaction {
    fn new(
        id: &str,
        queries: &[&str],
    ) -> (
        Self,
        Arc<ReactionControl>,
        mpsc::UnboundedReceiver<QueryResult>,
    ) {
        let control = Arc::new(ReactionControl::default());
        let (output, receiver) = mpsc::unbounded_channel();
        (
            Self {
                base: ReactionBase::new(
                    ReactionBaseParams::new(id, queries.iter().map(|id| (*id).into()).collect())
                        .with_auto_start(false),
                ),
                control: control.clone(),
                output,
            },
            control,
            receiver,
        )
    }
}
#[async_trait]
impl Reaction for ControlledReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "controlled"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        false
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.control.snapshot.load(Ordering::Acquire)
    }
    fn default_recovery_policy(&self) -> crate::ReactionRecoveryPolicy {
        if self.needs_snapshot_on_fresh_start() {
            crate::ReactionRecoveryPolicy::AutoReset
        } else {
            crate::ReactionRecoveryPolicy::Strict
        }
    }
    async fn bootstrap(&self, context: crate::reactions::BootstrapContext) -> anyhow::Result<()> {
        let _rows = context.fetch_snapshot().await?.collect_keyed_vec().await;
        Ok(())
    }
    async fn initialize(&self, context: crate::ReactionRuntimeContext) {
        *self.control.updates.lock().unwrap() = Some(context.update_tx.clone());
        self.base.initialize(context).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.control.starts.fetch_add(1, Ordering::AcqRel);
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.control.stops.fetch_add(1, Ordering::AcqRel);
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.output.send(result).map_err(Into::into)
    }
}

fn builder() -> crate::DrasiLibBuilder {
    DrasiLib::builder().with_execution_mode(crate::ExecutionMode::ComputationGraph)
}
fn config(id: &str, source: Option<&str>) -> QueryConfig {
    let query = crate::Query::cypher(id)
        .query("MATCH (n:Person) RETURN n.name AS name")
        .auto_start(false)
        .enable_bootstrap(false);
    match source {
        Some(source) => query.from_source(source).build(),
        None => query.build(),
    }
}
async fn insert(core: &DrasiLib, source: &str) {
    insert_person(core, source, "one", "Alice").await;
}
async fn insert_person(core: &DrasiLib, source: &str, id: &str, name: &str) {
    let instance = core
        .source_manager
        .get_source_instance(source)
        .await
        .unwrap();
    let base = &instance
        .as_any()
        .downcast_ref::<ControlledSource>()
        .unwrap()
        .inner;
    SourceBase::dispatch_from_task(
        base.dispatchers.clone(),
        crate::channels::SourceEventWrapper::new(
            source.into(),
            crate::channels::SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source, id),
                        labels: vec!["Person".into()].into(),
                        effective_from: 1000,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({"name":name})),
                },
            }),
            chrono::Utc::now(),
        ),
        source,
    )
    .await
    .unwrap();
}
async fn next(receiver: &mut mpsc::UnboundedReceiver<QueryResult>) -> QueryResult {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn transient_start_failure_can_be_retried_without_replacing_the_source() {
    let (source, control) = ControlledSource::new("source");
    control.fail_starts.store(1, Ordering::Release);
    let core = builder().with_source(source).build().await.unwrap();
    assert!(core.start_source("source").await.is_err());
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Error
    );
    core.start_source("source").await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 2);
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupts_a_pending_component_start() {
    let (source, control) = ControlledSource::new("source");
    control.pending_start.store(true, Ordering::Release);
    let core = Arc::new(builder().with_source(source).build().await.unwrap());
    let start = tokio::spawn({
        let core = core.clone();
        async move { core.start_source("source").await }
    });
    control.entered.notified().await;
    tokio::time::timeout(Duration::from_secs(2), core.stop_source("source"))
        .await
        .expect("stop must reach the controller while start is pending")
        .unwrap();
    assert!(start.await.unwrap().is_err());
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Stopped
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_replacement_rebinds_a_query_that_has_never_started() {
    let (old, old_control) = ControlledSource::new("source");
    let (new, new_control) = ControlledSource::new("source");
    let (reaction, _, mut output) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_source(old)
        .with_query(config("query", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.update_source("source", new).await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    insert(&core, "source").await;
    assert_eq!(next(&mut output).await.results.len(), 1);
    assert_eq!(old_control.subscriptions.load(Ordering::Acquire), 0);
    assert_eq!(new_control.subscriptions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_replacement_rebinds_a_reaction_that_has_never_started() {
    let (source, _) = ControlledSource::new("source");
    let (reaction, _, mut output) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.update_query("query", config("query", Some("source")))
        .await
        .unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    insert(&core, "source").await;
    assert_eq!(next(&mut output).await.results.len(), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn updated_dependencies_do_not_block_removal_of_the_old_source_or_query() {
    let (a, _) = ControlledSource::new("a");
    let (b, _) = ControlledSource::new("b");
    let (reaction, _, _) = ControlledReaction::new("reaction", &["q1"]);
    let core = builder()
        .with_source(a)
        .with_source(b)
        .with_query(config("q1", Some("a")))
        .with_query(config("q2", Some("b")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.update_query("q1", config("q1", Some("b")))
        .await
        .unwrap();
    core.remove_source("a", false).await.unwrap();
    let (reaction, _, _) = ControlledReaction::new("reaction", &["q2"]);
    core.update_reaction("reaction", reaction).await.unwrap();
    core.remove_query("q1").await.unwrap();
    let (a, _) = ControlledSource::new("a");
    core.add_source(a).await.unwrap();
    core.add_query(config("q1", Some("a"))).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_reaction_constructor_does_not_leave_a_phantom_registration() {
    let core = builder()
        .with_query(config("query", None))
        .build()
        .await
        .unwrap();
    let (bad, _, _) = ControlledReaction::new("reaction", &["query", "query"]);
    assert!(core.add_reaction(bad).await.is_err());
    assert!(core.list_reactions().await.unwrap().is_empty());
    let (good, _, _) = ControlledReaction::new("reaction", &["query"]);
    core.add_reaction(good).await.unwrap();
    core.remove_reaction("reaction", false).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_creation_failure_remains_addressable_for_awaited_cleanup() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.fail_initialize.store(true, Ordering::Release);
    assert!(core.add_source(source).await.is_err());
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Error
    );
    core.remove_source("source", true).await.unwrap();
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
    assert_eq!(control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_replacement_retains_the_actual_owner_for_deprovisioning() {
    let (old, old_control) = ControlledSource::new("source");
    old_control.fail_starts.store(1, Ordering::Release);
    old_control.fail_stop.store(true, Ordering::Release);
    let core = builder().with_source(old).build().await.unwrap();
    assert!(core.start_source("source").await.is_err());
    let (new, new_control) = ControlledSource::new("source");
    assert!(core.update_source("source", new).await.is_err());
    old_control.fail_stop.store(false, Ordering::Release);
    core.remove_source("source", true).await.unwrap();
    assert_eq!(old_control.deprovisions.load(Ordering::Acquire), 1);
    assert_eq!(new_control.deprovisions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_reaction_observes_plugin_failure_and_awaits_stop() {
    let (reaction, control, _) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_query(config("query", None))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    let mut events = core.component_graph.read().await.subscribe();
    let updates = control.updates.lock().unwrap().clone().unwrap();
    updates
        .send(ComponentUpdate::Status {
            component_id: "reaction".into(),
            status: ComponentStatus::Error,
            message: Some("background worker failed".into()),
        })
        .await
        .unwrap();
    crate::test_helpers::wait_for_component_status(
        &mut events,
        "reaction",
        ComponentStatus::Error,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_replacement_caller_still_selects_the_committed_instance() {
    let (old, old_control) = ControlledSource::new("source");
    let core = Arc::new(builder().with_source(old).build().await.unwrap());
    let (new, new_control) = ControlledSource::new("source");
    new_control
        .pending_initialize
        .store(true, Ordering::Release);
    let update = tokio::spawn({
        let core = core.clone();
        async move { core.update_source("source", new).await }
    });
    tokio::time::timeout(Duration::from_secs(2), new_control.entered.notified())
        .await
        .unwrap();
    update.abort();
    assert!(update.await.unwrap_err().is_cancelled());
    new_control.release.notify_one();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let control = runtime.parent().unwrap().control();
    control
        .preview(control.desired_snapshot().revision, vec![])
        .await
        .unwrap();
    runtime.project().await.unwrap();
    let current = core
        .source_manager
        .get_source_instance("source")
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &current
            .as_any()
            .downcast_ref::<ControlledSource>()
            .unwrap()
            .control,
        &new_control,
    ));
    core.remove_source("source", true).await.unwrap();
    assert_eq!(new_control.deprovisions.load(Ordering::Acquire), 1);
    assert_eq!(old_control.deprovisions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn retired_query_handles_cannot_operate_on_the_replacement_and_unbound_queries_stay_live() {
    let core = builder()
        .with_query(config("query", None))
        .build()
        .await
        .unwrap();
    let old = core
        .query_manager
        .get_query_instance("query")
        .await
        .unwrap();
    core.update_query("query", config("query", None))
        .await
        .unwrap();
    assert!(old.start().await.is_err());
    let mut events = core.component_graph.read().await.subscribe();
    core.start_query("query").await.unwrap();
    crate::test_helpers::wait_for_component_status(
        &mut events,
        "query",
        ComponentStatus::Running,
        Duration::from_secs(2),
    )
    .await;
    assert!(old.stop().await.is_err());
    let rows = tokio::time::timeout(Duration::from_secs(2), core.get_query_results("query"))
        .await
        .unwrap()
        .unwrap();
    assert!(rows.is_empty());
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn declared_references_validate_roles_before_native_mutation() {
    let (source, _) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let revision = runtime.inspector().unwrap().snapshot().desired.revision;
    let (reaction, _, _) = ControlledReaction::new("reaction", &["source"]);
    assert!(runtime.declare_reaction(Box::new(reaction)).await.is_err());
    assert_eq!(
        runtime.inspector().unwrap().snapshot().desired.revision,
        revision
    );
    assert!(core.list_reactions().await.unwrap().is_empty());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_service_logs_use_the_ordinary_instance_and_component_keys() {
    let (source, _) = ControlledSource::new("source");
    let core = builder()
        .with_id("native-log-parity")
        .with_source(source)
        .build()
        .await
        .unwrap();
    let (_, mut logs) = core.subscribe_source_logs("source").await.unwrap();
    core.start_source("source").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let message = logs.recv().await.unwrap();
            if message.message.contains("controlled-source-start") {
                break;
            }
        }
    })
    .await
    .expect("native source logs must use the normal source log subscription");
    core.shutdown().await.unwrap();
}

struct DeferredPersistentProvider;
#[async_trait]
impl drasi_core::interface::IndexBackendPlugin for DeferredPersistentProvider {
    async fn create_indexes(
        &self,
        _: &str,
    ) -> std::result::Result<drasi_core::interface::CreatedIndexes, drasi_core::interface::IndexError>
    {
        use drasi_core::{
            in_memory_index::{
                in_memory_element_index::InMemoryElementIndex,
                in_memory_future_queue::InMemoryFutureQueue,
                in_memory_result_index::InMemoryResultIndex,
            },
            interface::{CreatedIndexes, IndexSet, NoOpSessionControl},
        };
        let elements = Arc::new(InMemoryElementIndex::new());
        Ok(CreatedIndexes {
            set: IndexSet {
                element_index: elements.clone(),
                archive_index: elements,
                result_index: Arc::new(InMemoryResultIndex::new()),
                future_queue: Arc::new(InMemoryFutureQueue::new()),
                session_control: Arc::new(NoOpSessionControl),
            },
            checkpoint_store: None,
            outbox_writer: None,
            live_results_writer: None,
        })
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn early_query_activation_failure_is_visible_and_bulk_stop_allows_another_attempt() {
    let (source, _) = ControlledSource::new("source");
    let mut query_config = config("query", Some("source"));
    query_config.auto_start = true;
    let core = builder()
        .with_source(source)
        .with_query(query_config)
        .with_default_index_provider("persistent", Arc::new(DeferredPersistentProvider))
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    let query = core
        .query_manager
        .get_query_instance("query")
        .await
        .unwrap();
    assert_eq!(query.status().await, ComponentStatus::Error);
    assert!(matches!(
        query.fetch_snapshot().await,
        Err(crate::queries::FetchError::NotRunning {
            status: ComponentStatus::Error
        })
    ));
    let runtime = core.computation_runtime.as_ref().unwrap();
    let record = runtime.record("query", "query").await.unwrap();
    let (_, _, first) = runtime.control_for(&record).unwrap();
    assert!(format!("{:#}", first.failure.as_ref().unwrap().cause).contains("IncompatibleSource"));
    core.stop().await.unwrap();
    core.start().await.unwrap();
    let (_, _, second) = runtime.control_for(&record).unwrap();
    assert!(second.operation.0 > first.operation.0);
    assert!(format!("{:#}", second.failure.as_ref().unwrap().cause).contains("IncompatibleSource"));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_query_replacement_rebinds_consumers_without_restarting_unaffected_components() {
    let (source, source_control) = ControlledSource::new("source");
    let (reaction, reaction_control, mut output) = ControlledReaction::new("reaction", &["query"]);
    reaction_control.snapshot.store(true, Ordering::Release);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_query(config("unrelated", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_query("unrelated").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let source_token = runtime.record("source", "source").await.unwrap().token;
    let unrelated_token = runtime.record("unrelated", "query").await.unwrap().token;
    insert(&core, "source").await;
    assert_eq!(next(&mut output).await.results.len(), 1);

    let mut updated = config("query", Some("source"));
    updated.query = "MATCH (n:Person) RETURN n.name AS name, 'new' AS version".into();
    core.update_query("query", updated).await.unwrap();
    insert_person(&core, "source", "two", "Bob").await;
    let result = next(&mut output).await;
    assert!(matches!(&result.results[..],
        [crate::channels::ResultDiff::Add { data, .. }]
        if data == &serde_json::json!({"name":"Bob","version":"new"})));
    assert_eq!(reaction_control.starts.load(Ordering::Acquire), 2);
    assert_eq!(
        runtime.record("source", "source").await.unwrap().token,
        source_token
    );
    assert_eq!(
        runtime.record("unrelated", "query").await.unwrap().token,
        unrelated_token
    );
    let query_token = runtime.record("query", "query").await.unwrap().token;

    let (replacement, replacement_control, _replacement_output) =
        ControlledReaction::new("reaction", &["query"]);
    replacement_control.snapshot.store(true, Ordering::Release);
    core.update_reaction("reaction", replacement).await.unwrap();
    assert_eq!(
        runtime.record("query", "query").await.unwrap().token,
        query_token
    );
    assert_eq!(
        runtime.record("source", "source").await.unwrap().token,
        source_token
    );
    assert_eq!(
        core.get_query_status("unrelated").await.unwrap(),
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Running
    );
    assert_eq!(source_control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}
