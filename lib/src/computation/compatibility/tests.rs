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
struct PeerControl {
    messages: StdMutex<Vec<PeerMessage>>,
    received: Notify,
}

impl PeerControl {
    async fn wait_for(&self, predicate: impl Fn(&PeerMessage) -> bool) -> PeerMessage {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let notified = self.received.notified();
                if let Some(message) = self
                    .messages
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|message| predicate(message))
                    .cloned()
                {
                    return message;
                }
                notified.await;
            }
        })
        .await
        .expect("the attached control handler must receive the notification")
    }
}

#[async_trait]
impl ControlHandler for PeerControl {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        self.messages.lock().unwrap().push(message);
        self.received.notify_one();
        Ok(())
    }
}

struct NativeService {
    descriptor: ComponentDescriptor,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for NativeService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl ComputationService for NativeService {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

struct NativeServiceFactory {
    descriptor: FactoryDescriptor,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    index_providers: Vec<Arc<dyn drasi_core::interface::IndexBackendPlugin>>,
}

#[async_trait]
impl ComponentFactory for NativeServiceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        if context.specification.configuration.get("fail")
            == Some(&ConfigurationValue::Literal(true.into()))
        {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "injected native creation failure"
            )));
        }
        if !self.index_providers.is_empty() {
            let provided = context
                .resources::<Arc<dyn drasi_core::interface::IndexBackendPlugin>>("indexes")
                .map_err(ComponentCreationError::terminal)?;
            if provided.len() != self.index_providers.len()
                || provided
                    .iter()
                    .zip(&self.index_providers)
                    .any(|(actual, expected)| !Arc::ptr_eq(actual.as_ref(), expected))
            {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "index inventory does not retain the configured provider instances"
                )));
            }
        }
        Ok(ConstructedComponent::service(Box::new(NativeService {
            descriptor: context.specification.descriptor.clone(),
            starts: self.starts.clone(),
            stops: self.stops.clone(),
        })))
    }
}

struct UnboundNativeSource {
    descriptor: ComponentDescriptor,
}

#[async_trait]
impl ComputationComponent for UnboundNativeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("an unbound native source must not start");
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for UnboundNativeSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

#[derive(Default)]
struct SourceControl {
    resource_observers: StdMutex<Vec<Arc<dyn crate::context::ComponentResourceObserver>>>,
    initializations: AtomicUsize,
    auto_start: AtomicBool,
    starts: AtomicUsize,
    stops: AtomicUsize,
    deprovisions: AtomicUsize,
    subscriptions: AtomicUsize,
    fail_starts: AtomicUsize,
    fail_stop: AtomicBool,
    pending_start: AtomicBool,
    starting_only: AtomicBool,
    subscription_fences: AtomicUsize,
    release_bootstrap_at_fence: AtomicBool,
    bootstrap_entered: Notify,
    bootstrap_release: Notify,
    fail_bootstrap: AtomicBool,
    fail_initialize: AtomicBool,
    pending_initialize: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[tokio::test]
async fn graph_registry_remains_authoritative_without_adapter_candidates_or_projection_entries() {
    let (source, source_control) = ControlledSource::new("source");
    let (reaction, reaction_control, _) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    for id in ["source", "query", "reaction"] {
        core.computation_component(id)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    let runtime = core.computation_runtime.as_ref().unwrap();
    runtime.records.write().await.clear();
    let projecting = runtime.projecting.lock().await;
    {
        let mut projection = runtime.projection.write().await;
        for id in ["reaction", "query", "source"] {
            projection.remove_component(id).unwrap();
        }
    }
    let records = runtime.current_records().await.unwrap();
    assert!(["source", "query", "reaction"]
        .iter()
        .all(|id| records.contains_key(*id)));
    assert!(core
        .list_sources()
        .await
        .unwrap()
        .iter()
        .any(|(id, _)| id == "source"));
    assert_eq!(
        core.list_queries().await.unwrap(),
        vec![("query".into(), ComponentStatus::Added)]
    );
    assert_eq!(
        core.list_reactions().await.unwrap(),
        vec![("reaction".into(), ComponentStatus::Added)]
    );
    assert_eq!(
        core.get_source_info("source").await.unwrap().source_type,
        "controlled"
    );
    assert_eq!(
        core.get_query_info("query").await.unwrap().status,
        ComponentStatus::Added
    );
    assert_eq!(
        core.get_reaction_info("reaction").await.unwrap().queries,
        vec!["query"]
    );
    assert!(core.get_source_info("query").await.is_err());
    assert_eq!(
        core.get_query_config("query").await.unwrap().sources[0].source_id,
        "source"
    );
    assert_eq!(core.get_current_config().await.unwrap().queries.len(), 1);
    core.get_query_output_metrics("query").await.unwrap();
    core.subscribe_source_logs("source").await.unwrap();
    core.subscribe_query_logs("query").await.unwrap();
    core.subscribe_reaction_logs("reaction").await.unwrap();
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(snapshot.sources.iter().any(|source| source.id == "source"));
    assert_eq!(snapshot.queries.len(), 1);
    assert_eq!(snapshot.reactions.len(), 1);
    assert!(snapshot
        .edges
        .iter()
        .any(|edge| edge.from == "source" && edge.to == "query"));
    drop(projecting);
    runtime.project().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    assert_eq!(source_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(reaction_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(
        core.get_query_config("query").await.unwrap().sources[0].source_id,
        "source"
    );
    assert!(core.remove_source("source", false).await.is_err());
    core.remove_reaction("reaction", false).await.unwrap();
    core.remove_query("query").await.unwrap();
    core.remove_source("source", false).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_native_operations_never_construct_legacy_execution_managers() {
    use futures::StreamExt;

    let (source, _) = ControlledSource::new("source");
    let (reaction, _, _) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    for id in ["source", "query", "reaction"] {
        core.computation_component(id)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    assert!(core.legacy_backend.initialized().is_none());
    core.list_sources().await.unwrap();
    core.list_queries().await.unwrap();
    core.list_reactions().await.unwrap();
    core.get_source_info("source").await.unwrap();
    core.get_query_info("query").await.unwrap();
    core.get_reaction_info("reaction").await.unwrap();
    core.get_source_schema("source").await.unwrap();
    core.get_graph_schema().await.unwrap();
    core.get_current_config().await.unwrap();
    core.snapshot_configuration().await.unwrap();
    core.get_query_output_metrics("query").await.unwrap();
    core.get_reaction_metrics("reaction").await.unwrap();
    core.get_lifecycle_metrics().await.unwrap();
    core.subscribe_source_logs("source").await.unwrap();
    core.subscribe_query_logs("query").await.unwrap();
    core.subscribe_reaction_logs("reaction").await.unwrap();
    core.subscribe_source_events("source").await.unwrap();
    core.subscribe_query_events("query").await.unwrap();
    core.subscribe_reaction_events("reaction").await.unwrap();
    core.get_source_events("source")
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_query_events("query")
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_reaction_events("reaction")
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_all_source_events()
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_all_query_events()
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_all_reaction_events()
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.get_all_events()
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_started()
        .await
        .unwrap();
    core.start_reaction("reaction").await.unwrap();
    core.get_query_results("query").await.unwrap();
    core.start().await.unwrap();
    core.stop().await.unwrap();
    core.update_query("query", config("query", Some("source")))
        .await
        .unwrap();
    core.remove_reaction("reaction", false).await.unwrap();
    core.remove_query("query").await.unwrap();
    core.remove_source("source", false).await.unwrap();
    core.shutdown().await.unwrap();
    assert!(core.legacy_backend.initialized().is_none());
}

#[tokio::test]
async fn legacy_manager_escape_hatch_is_lazy_and_shared_without_changing_mode() {
    let core = builder()
        .with_query(config("query", None))
        .build()
        .await
        .unwrap();
    core.computation_component("query")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    assert!(core.legacy_backend.initialized().is_none());
    let clone = core.clone();
    assert!(std::ptr::eq(core.query_manager(), clone.query_manager()));
    assert!(core.legacy_backend.initialized().is_some());
    assert_eq!(
        core.execution_mode(),
        crate::ExecutionMode::ComputationGraph
    );
    let query = core
        .query_manager()
        .get_query_instance("query")
        .await
        .unwrap();
    assert!(query.as_any().is::<QueryInstance>());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_recipes_do_not_depend_on_the_legacy_projection() {
    let (source, _) = ControlledSource::new("source");
    let properties = HashMap::from([("file".into(), serde_json::json!("fixture.jsonl"))]);
    let core = builder()
        .with_source(source)
        .with_bootstrap_for_source("source", "scriptfile", properties.clone())
        .with_identity_provider(Arc::new(crate::identity::PasswordIdentityProvider::new(
            "user", "password",
        )))
        .build()
        .await
        .unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let projecting = runtime.projecting.lock().await;
    {
        let mut projection = runtime.projection.write().await;
        projection.remove_component("source-bootstrap").unwrap();
        projection.remove_component("identity-provider").unwrap();
    }
    let snapshot = core.snapshot_configuration().await.unwrap();
    let source = snapshot
        .sources
        .iter()
        .find(|source| source.id == "source")
        .unwrap();
    let recipe = source.bootstrap_provider.as_ref().unwrap();
    assert_eq!(recipe.kind, "scriptfile");
    assert_eq!(recipe.properties, properties);
    assert!(snapshot
        .edges
        .iter()
        .any(|edge| edge.from == "source-bootstrap" && edge.to == "source"));
    assert!(snapshot
        .edges
        .iter()
        .any(|edge| edge.from == "identity-provider" && edge.to == "source"));
    drop(projecting);
    runtime.project().await.unwrap();
    assert!(runtime.projection.read().await.contains("source-bootstrap"));
    core.remove_source("source", false).await.unwrap();
    let (replacement, _) = ControlledSource::new("source");
    core.add_source_with_handle(replacement)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(snapshot
        .sources
        .iter()
        .find(|source| source.id == "source")
        .unwrap()
        .bootstrap_provider
        .is_none());
    assert!(!snapshot
        .edges
        .iter()
        .any(|edge| edge.from == "source-bootstrap" && edge.to == "source"));
    assert!(core.legacy_backend.initialized().is_none());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_projection_status_and_membership_do_not_change_public_inspection() {
    let (source, _) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let projecting = runtime.projecting.lock().await;
    {
        let mut projection = runtime.projection.write().await;
        projection
            .register_query("phantom", HashMap::new(), &[])
            .unwrap();
        projection.get_component_mut("source").unwrap().status = ComponentStatus::Error;
    }
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Added
    );
    assert_eq!(
        core.get_source_info("source").await.unwrap().status,
        ComponentStatus::Added
    );
    assert!(core.list_queries().await.unwrap().is_empty());
    assert!(core.get_query_status("phantom").await.is_err());
    assert!(core.get_source_status("phantom").await.is_err());
    assert!(core.subscribe_query_logs("phantom").await.is_err());
    assert!(core
        .snapshot_configuration()
        .await
        .unwrap()
        .queries
        .is_empty());
    drop(projecting);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_plugin_metadata_fails_the_added_node_not_the_add_call() {
    let core = builder().build().await.unwrap();
    let (source, source_control) = ControlledSource::new("source");
    core.add_source_with_metadata(
        source,
        HashMap::from([
            ("pluginId".into(), "invalid plugin id".into()),
            ("pluginVersion".into(), "1".into()),
        ]),
    )
    .await
    .unwrap();
    assert!(core
        .computation_component("source")
        .unwrap()
        .wait_created()
        .await
        .is_err());
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Error
    );
    assert!(core.start_source("source").await.is_err());
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert!(core
        .get_source_info("source")
        .await
        .unwrap()
        .error_message
        .is_some());
    core.remove_source("source", false).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_projection_cannot_veto_a_graph_addition() {
    let core = builder().build().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    runtime
        .projection
        .write()
        .await
        .register_query("source", HashMap::new(), &[])
        .unwrap();
    let (source, source_control) = ControlledSource::new("source");
    let handle = core.add_source_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    runtime.project().await.unwrap();
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 1);
    assert_eq!(
        runtime
            .projection
            .read()
            .await
            .get_component("source")
            .unwrap()
            .kind,
        crate::component_graph::ComponentKind::Source,
    );
    assert!(runtime
        .current_records()
        .await
        .unwrap()
        .contains_key("source"));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn graph_activation_policy_controls_ordinary_startup_and_configuration_reads() {
    let (source, source_control) = ControlledSource::new("source");
    source_control.auto_start.store(true, Ordering::Release);
    let mut query = config("query", None);
    query.auto_start = true;
    let core = builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await
        .unwrap();
    for id in ["source", "query"] {
        core.computation_component(id)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    let control = core.computation_control().unwrap();
    let desired = control.desired_snapshot();
    let mut changes = Vec::new();
    for id in ["source", "query"] {
        let mut component = desired
            .select(GraphSelection::Exact(vec![
                ComponentId::try_new(id).unwrap()
            ]))
            .unwrap()
            .components
            .remove(0);
        component.lifecycle.auto_start = false;
        changes.push(DesiredMutation::PutComponent(component));
    }
    let preview = control.preview(desired.revision, changes).await.unwrap();
    let report = control
        .reconcile(preview, TopologyBindings::default())
        .await
        .unwrap();
    assert_eq!(report.summary, OperationSummary::Completed);
    core.start().await.unwrap();
    assert_eq!(source_control.starts.load(Ordering::Acquire), 0);
    assert!(
        !core
            .computation_component("query")
            .unwrap()
            .observed()
            .unwrap()
            .started
    );
    assert!(!core.get_query_config("query").await.unwrap().auto_start);
    assert!(!core.get_current_config().await.unwrap().queries[0].auto_start);
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(
        !snapshot
            .sources
            .iter()
            .find(|source| source.id == "source")
            .unwrap()
            .auto_start
    );
    assert!(!snapshot.queries[0].config.auto_start);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn replacement_preserves_declared_autostart_without_starting_a_stopped_instance() {
    let (source, _) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    core.computation_component("source")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let (replacement, replacement_control) = ControlledSource::new("source");
    replacement_control
        .auto_start
        .store(true, Ordering::Release);
    core.update_source("source", replacement).await.unwrap();
    let source = core.computation_component("source").unwrap();
    assert!(
        core.computation_control()
            .unwrap()
            .desired_snapshot()
            .lifecycle_policies[source.id()]
        .auto_start
    );
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 0);
    core.start().await.unwrap();
    source.wait_started().await.unwrap();
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn subscriptions_are_declared_with_nodes_and_enforce_graph_removal_policy() {
    let core = builder().build().await.unwrap();
    let query = core
        .add_query_with_handle(config("query", Some("later")))
        .await
        .unwrap();
    let control = core.computation_control().unwrap();
    let source_id = ComponentId::try_new("later").unwrap();
    assert!(control
        .desired_snapshot()
        .subscriptions
        .contains(&(source_id.clone(), query.id().clone())));
    assert!(query.wait_created().await.is_err());
    let (unrelated, _) = ControlledSource::new("unrelated");
    core.add_source_with_handle(unrelated)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.remove_source("unrelated", false).await.unwrap();
    let exported = control
        .desired_snapshot()
        .select(GraphSelection::All)
        .unwrap();
    let imported = DesiredTopology::from_json(&exported.to_json().unwrap()).unwrap();
    assert!(imported
        .subscriptions
        .contains(&(source_id.clone(), query.id().clone())));
    let (source, _) = ControlledSource::new("later");
    core.add_source_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    assert!(control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![source_id.clone()]),
                policy: RemovalPolicy::Reject,
            }],
        )
        .await
        .is_err());
    assert!(core.remove_source("later", false).await.is_err());
    let closure = control
        .desired_snapshot()
        .select(GraphSelection::Dependencies(vec![query.id().clone()]))
        .unwrap();
    assert!(closure
        .components
        .iter()
        .any(|node| node.descriptor.id() == &source_id));
    let downstream = control
        .desired_snapshot()
        .select(GraphSelection::Dependents(vec![source_id.clone()]))
        .unwrap();
    assert!(downstream
        .components
        .iter()
        .any(|node| node.descriptor.id() == query.id()));
    let cascade = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![source_id.clone()]),
                policy: RemovalPolicy::Cascade,
            }],
        )
        .await
        .unwrap();
    assert!(cascade.removed().contains(query.id()));
    core.remove_query("query").await.unwrap();
    core.remove_source("later", false).await.unwrap();
    core.shutdown().await.unwrap();
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
        self.control.auto_start.load(Ordering::Acquire)
    }
    fn supports_replay(&self) -> bool {
        false
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.control.initializations.fetch_add(1, Ordering::AcqRel);
        if let Some(observer) = &context.resource_observer {
            self.control
                .resource_observers
                .lock()
                .unwrap()
                .push(observer.clone());
        }
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
        if !self.control.starting_only.load(Ordering::Acquire) {
            self.inner.set_status(ComponentStatus::Running, None).await;
        }
        Ok(())
    }
    async fn on_subscriptions_complete(&self) {
        self.control
            .subscription_fences
            .fetch_add(1, Ordering::AcqRel);
        if self
            .control
            .release_bootstrap_at_fence
            .load(Ordering::Acquire)
        {
            self.control.bootstrap_release.notify_one();
        }
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
    resource_observers: StdMutex<Vec<Arc<dyn crate::context::ComponentResourceObserver>>>,
    initializations: AtomicUsize,
    deprovisions: AtomicUsize,
    pending_enqueue: AtomicBool,
    enqueue_entered: Notify,
    enqueue_release: Notify,
    starts: AtomicUsize,
    starting_only: AtomicBool,
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
        self.control.initializations.fetch_add(1, Ordering::AcqRel);
        if let Some(observer) = &context.resource_observer {
            self.control
                .resource_observers
                .lock()
                .unwrap()
                .push(observer.clone());
        }
        *self.control.updates.lock().unwrap() = Some(context.update_tx.clone());
        self.base.initialize(context).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.control.starts.fetch_add(1, Ordering::AcqRel);
        let status = if self.control.starting_only.load(Ordering::Acquire) {
            ComponentStatus::Starting
        } else {
            ComponentStatus::Running
        };
        self.base.set_status(status, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.control.stops.fetch_add(1, Ordering::AcqRel);
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.control.deprovisions.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        if self.control.pending_enqueue.load(Ordering::Acquire) {
            self.control.enqueue_entered.notify_one();
            self.control.enqueue_release.notified().await;
        }
        self.output.send(result).map_err(Into::into)
    }
}

struct GatedBootstrap(Arc<SourceControl>);

#[async_trait]
impl crate::bootstrap::BootstrapProvider for GatedBootstrap {
    async fn bootstrap(
        &self,
        _: crate::bootstrap::BootstrapRequest,
        _: &crate::bootstrap::BootstrapContext,
        _: crate::channels::BootstrapEventSender,
        _: Option<&SourceSubscriptionSettings>,
    ) -> anyhow::Result<crate::bootstrap::BootstrapResult> {
        self.0.bootstrap_entered.notify_one();
        self.0.bootstrap_release.notified().await;
        if self.0.fail_bootstrap.load(Ordering::Acquire) {
            anyhow::bail!("injected bootstrap failure");
        }
        Ok(crate::bootstrap::BootstrapResult::default())
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
    let instance = core.source_instance(source).await.unwrap();
    let base = &instance
        .as_any()
        .downcast_ref::<ControlledSource>()
        .unwrap()
        .inner;
    base.dispatch_event(crate::channels::SourceEventWrapper::new(
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
    ))
    .await
    .unwrap();
}
async fn next(receiver: &mut mpsc::UnboundedReceiver<QueryResult>) -> QueryResult {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

async fn component_handle(core: &DrasiLib, id: &str, kind: &str) -> ComponentHandle {
    let runtime = core.computation_runtime.as_ref().unwrap();
    let record = runtime.record(id, kind).await.unwrap();
    assert_eq!(record.node.as_str(), id);
    runtime.handle_for(&record).unwrap()
}

async fn assert_creation_failed(core: &DrasiLib, handle: &ComponentHandle, message: &str) {
    tokio::time::timeout(Duration::from_secs(3), handle.wait_created())
        .await
        .expect("creation must complete with a failure")
        .expect_err("failed initialization must not be reported as ready");
    let observed = handle.observed().unwrap();
    assert_eq!(observed.realization, RealizationState::CreationFailed);
    let failure = format!("{:#}", observed.failure.as_ref().unwrap().cause);
    assert!(failure.contains(message), "{failure}");
    core.computation_runtime
        .as_ref()
        .unwrap()
        .project()
        .await
        .unwrap();
}

async fn wait_for_attached_resources(
    core: &DrasiLib,
    component: &ComponentId,
    roles: &[ResourceRole],
) -> Arc<ComputationInspection> {
    let mut changes = core
        .computation_runtime
        .as_ref()
        .unwrap()
        .inspector()
        .unwrap()
        .subscribe();
    let snapshot = tokio::time::timeout(
        Duration::from_secs(3),
        changes.wait_for(|snapshot| {
            snapshot
                .desired
                .component_resources
                .get(component)
                .is_some_and(|resources| {
                    roles.iter().all(|role| {
                        resources.iter().any(|id| {
                            snapshot
                                .desired
                                .resources
                                .get(id)
                                .is_some_and(|resource| resource.role == *role)
                        })
                    })
                })
        }),
    )
    .await
    .expect("the initializer must publish its actual provider inventory")
    .unwrap()
    .clone();
    snapshot
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_reports_selected_providers_before_initialization_completes() {
    let core = builder().build().await.unwrap();
    let selected: Arc<dyn crate::state_store::StateStoreProvider> =
        Arc::new(crate::state_store::MemoryStateStoreProvider::new());
    let (mut source, control) = ControlledSource::new("source");
    source.inner = SourceBase::new(
        SourceBaseParams::new("source")
            .with_auto_start(false)
            .with_state_store(selected.clone()),
    )
    .unwrap();
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core.add_source_with_handle(source).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    let observed =
        wait_for_attached_resources(&core, handle.id(), &[ResourceRole::StateStore]).await;
    assert_eq!(
        handle.observed().unwrap().realization,
        RealizationState::Creating
    );
    let provider = observed.desired.component_resources[handle.id()]
        .iter()
        .find(|id| observed.desired.resources[*id].role == ResourceRole::StateStore)
        .unwrap();
    assert_ne!(provider.as_str(), "instance.state");
    assert_eq!(
        observed.desired.resources[provider].ownership,
        ResourceOwnership::Borrowed
    );
    let old_observer = control.resource_observers.lock().unwrap()[0].clone();
    let (replacement, replacement_control) = ControlledSource::new("source");
    core.update_source("source", replacement).await.unwrap();
    assert_eq!(
        replacement_control.resource_observers.lock().unwrap().len(),
        1
    );
    let current = component_handle(&core, "source", "source").await;
    assert_ne!(handle.generation(), current.generation());
    let error = old_observer
        .observe(vec![crate::context::ComponentResource::StateStore(
            selected,
        )])
        .await
        .unwrap_err();
    assert!(matches!(
        error
            .downcast_ref::<GraphError>()
            .map(GraphError::underlying),
        Some(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_and_reaction_report_shared_provider_identity_with_generation_scoped_observers() {
    let core = builder().build().await.unwrap();
    let shared: Arc<dyn crate::identity::IdentityProvider> = Arc::new(
        crate::identity::PasswordIdentityProvider::new("user", "password"),
    );
    let (source, _) = ControlledSource::new("source");
    source.inner.set_identity_provider(shared.clone()).await;
    let source = core.add_source_with_handle(source).await.unwrap();
    source.wait_created().await.unwrap();
    core.add_query_with_handle(config("query", None))
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::new("reaction", &["query"]);
    reaction.base.set_identity_provider(shared.clone()).await;
    let reaction = core.add_reaction_with_handle(reaction).await.unwrap();
    reaction.wait_created().await.unwrap();
    wait_for_attached_resources(&core, source.id(), &[ResourceRole::Identity]).await;
    let observed =
        wait_for_attached_resources(&core, reaction.id(), &[ResourceRole::Identity]).await;
    let source_identity = observed.desired.component_resources[source.id()]
        .iter()
        .find(|id| observed.desired.resources[*id].role == ResourceRole::Identity)
        .unwrap();
    assert!(observed.desired.component_resources[reaction.id()].contains(source_identity));
    assert_eq!(
        observed.desired.resources[source_identity].ownership,
        ResourceOwnership::Borrowed
    );
    assert!(!observed
        .desired
        .resources
        .contains_key(&ResourceId::try_new("instance.identity").unwrap()));
    let old_observer = reaction_control.resource_observers.lock().unwrap()[0].clone();
    let (replacement, replacement_control, _) = ControlledReaction::new("reaction", &["query"]);
    replacement.base.set_identity_provider(shared.clone()).await;
    core.update_reaction("reaction", replacement).await.unwrap();
    assert_eq!(
        replacement_control.resource_observers.lock().unwrap().len(),
        1
    );
    let current = component_handle(&core, "reaction", "reaction").await;
    assert_ne!(reaction.generation(), current.generation());
    let error = old_observer
        .observe(vec![crate::context::ComponentResource::Identity(shared)])
        .await
        .unwrap_err();
    assert!(matches!(
        error
            .downcast_ref::<GraphError>()
            .map(GraphError::underlying),
        Some(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_query_addition_retains_the_original_declared_configuration() {
    let core = builder().build().await.unwrap();
    let mut invalid = config("invalid", None);
    invalid.query = "this is not a query".into();
    core.add_query(invalid.clone()).await.unwrap();
    let handle = component_handle(&core, "invalid", "query").await;
    assert_creation_failed(&core, &handle, "").await;
    let query = core.get_query_config("invalid").await.unwrap();
    assert_eq!(query.query, invalid.query);
    assert_eq!(
        core.get_query_status("invalid").await.unwrap(),
        ComponentStatus::Error
    );
    let runtime = core.computation_runtime.as_ref().unwrap();
    let snapshot = runtime.inspector().unwrap().snapshot();
    assert_eq!(
        snapshot.desired.specifications[handle.id()]
            .configuration
            .get("definition"),
        Some(&ConfigurationValue::Literal(
            serde_json::to_value(invalid).unwrap()
        )),
    );
    let declared = runtime.query("invalid").await.unwrap();
    assert_eq!(
        declared.inspector().snapshot().desired.id,
        snapshot.desired.id
    );
    let (source, _) = ControlledSource::new("independent");
    core.add_source_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.remove_query("invalid").await.unwrap();
    assert!(core.get_query_config("invalid").await.is_err());
    assert!(matches!(
        handle.observed(),
        Err(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn builder_query_validation_failures_remain_on_declared_nodes() {
    use crate::indexes::{StorageBackendRef, StorageBackendSpec};

    let mut syntax = config("syntax", None);
    syntax.query = "this is not a query".into();
    let dependency = config("dependency", Some("missing-source"));
    let mut unknown_backend = config("unknown-backend", None);
    unknown_backend.storage_backend = Some(StorageBackendRef::Named("missing-backend".into()));
    let mut invalid_inline = config("invalid-inline", None);
    invalid_inline.storage_backend = Some(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
        kind: "".into(),
    }));
    let mut unsupported_inline = config("unsupported-inline", None);
    unsupported_inline.storage_backend =
        Some(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
            kind: "rocksdb".into(),
        }));
    let mut capacity = config("capacity", None);
    capacity.dispatch_buffer_capacity = Some(0);
    let invalid = [
        (syntax, ""),
        (dependency, "missing-source"),
        (unknown_backend, "missing-backend"),
        (invalid_inline, "must not be empty"),
        (unsupported_inline, ""),
        (capacity, "capacity must be nonzero"),
    ];
    let mut configured = builder().with_query(config("valid", None));
    for (query, _) in &invalid {
        configured = configured.with_query(query.clone());
    }
    let core = configured
        .build()
        .await
        .expect("query-specific validation must happen after graph addition");
    for (query, message) in invalid {
        let handle = core.computation_component(&query.id).unwrap();
        assert_creation_failed(&core, &handle, message).await;
        assert_eq!(
            serde_json::to_value(core.get_query_config(&query.id).await.unwrap()).unwrap(),
            serde_json::to_value(
                core.config
                    .queries
                    .iter()
                    .find(|configured| configured.id == query.id)
                    .unwrap()
            )
            .unwrap(),
        );
        core.remove_query(&query.id).await.unwrap();
    }
    let valid = core.computation_component("valid").unwrap();
    valid.wait_created().await.unwrap();
    valid.start().await.unwrap();
    valid.wait_started().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_constructor_validation_happens_after_declaration() {
    let core = builder().build().await.unwrap();
    let mut invalid = config("query", None);
    invalid.dispatch_buffer_capacity = Some(0);
    let handle = core.add_query_with_handle(invalid).await.unwrap();
    assert_creation_failed(&core, &handle, "").await;
    assert_eq!(
        core.get_query_status("query").await.unwrap(),
        ComponentStatus::Error
    );
    core.update_query("query", config("query", None))
        .await
        .unwrap();
    let replacement = component_handle(&core, "query", "query").await;
    replacement.wait_created().await.unwrap();
    assert_ne!(handle.generation(), replacement.generation());
    assert!(matches!(
        handle.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    core.start_query("query").await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_dependencies_fail_on_nodes_and_can_be_retried_when_available() {
    let core = builder().build().await.unwrap();
    core.add_query(config("query", Some("source")))
        .await
        .unwrap();
    let query = component_handle(&core, "query", "query").await;
    assert_creation_failed(&core, &query, "Source 'source' not found").await;
    assert!(!core.component_graph.read().await.contains("source"));

    let (reaction, _, _) = ControlledReaction::new("reaction", &["missing-query"]);
    let reaction = core.add_reaction_with_handle(reaction).await.unwrap();
    assert_creation_failed(&core, &reaction, "Query 'missing-query' not found").await;
    assert!(!core.component_graph.read().await.contains("missing-query"));

    let (source, _) = ControlledSource::new("source");
    core.add_source_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.start_query("query").await.unwrap();
    assert!(matches!(
        query.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    core.add_query_with_handle(config("missing-query", None))
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.start_query("missing-query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    assert!(matches!(
        reaction.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_retry_reuses_the_owned_source_and_never_retries_in_a_loop() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.fail_initialize.store(true, Ordering::Release);
    let handle = core.add_source_with_handle(source).await.unwrap();
    assert_creation_failed(&core, &handle, "injected initialization failure").await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    control.fail_initialize.store(false, Ordering::Release);
    core.start_source("source").await.unwrap();
    assert_eq!(control.initializations.load(Ordering::Acquire), 2);
    assert_eq!(control.resource_observers.lock().unwrap().len(), 2);
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    assert!(matches!(
        handle.wait_started().await,
        Err(GraphError::StaleGeneration)
    ));
    let source = core
        .computation_runtime
        .as_ref()
        .unwrap()
        .source("source")
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &source
            .as_any()
            .downcast_ref::<ControlledSource>()
            .unwrap()
            .control,
        &control
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_initialization_does_not_block_addition_or_removal() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("pending");
    control.pending_initialize.store(true, Ordering::Release);
    let handle = tokio::time::timeout(Duration::from_secs(3), core.add_source_with_handle(source))
        .await
        .expect("addition must not wait for initialization")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    let invalid = crate::Query::cypher("invalid-dependency")
        .query("MATCH (n) RETURN n")
        .from_source("pending")
        .from_source("missing")
        .auto_start(false)
        .build();
    let invalid = core.add_query_with_handle(invalid).await.unwrap();
    assert_creation_failed(&core, &invalid, "Source 'missing' not found").await;
    core.remove_query("invalid-dependency").await.unwrap();
    let (independent, independent_control) = ControlledSource::new("independent");
    let independent = tokio::time::timeout(
        Duration::from_secs(3),
        core.add_source_with_handle(independent),
    )
    .await
    .expect("one initializer must not hold a global addition lock")
    .unwrap();
    independent.wait_created().await.unwrap();
    assert_eq!(
        independent_control.initializations.load(Ordering::Acquire),
        1
    );
    tokio::time::timeout(Duration::from_secs(3), core.remove_source("pending", true))
        .await
        .expect("removal must cancel the pending initializer")
        .unwrap();
    assert!(matches!(
        handle.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
    assert_eq!(control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_wait_does_not_cancel_controller_owned_creation() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core.add_source_with_handle(source).await.unwrap();
    let wait = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_created().await }
    });
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    wait.abort();
    assert!(wait.await.unwrap_err().is_cancelled());
    control.release.notify_one();
    handle.wait_created().await.unwrap();
    handle.start().await.unwrap();
    handle.wait_started().await.unwrap();
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn instance_stop_cancels_pending_auto_start_and_restart_can_retry_creation() {
    let core = builder().build().await.unwrap();
    core.start().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core.add_source_with_handle(source).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), core.stop())
        .await
        .expect("instance stop must cancel a pending initializer")
        .unwrap();
    assert!(handle.wait_created().await.is_err());
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    control.pending_initialize.store(false, Ordering::Release);
    core.start().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    assert!(matches!(
        handle.wait_started().await,
        Err(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_cancels_pending_creation_and_fences_existing_waiters() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control.pending_initialize.store(true, Ordering::Release);
    let old = core.add_source_with_handle(source).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    let wait = tokio::spawn({
        let old = old.clone();
        async move { old.wait_created().await }
    });
    let (replacement, replacement_control) = ControlledSource::new("source");
    replacement_control
        .auto_start
        .store(true, Ordering::Release);
    tokio::time::timeout(
        Duration::from_secs(3),
        core.update_source("source", replacement),
    )
    .await
    .expect("replacement must cancel the obsolete initializer")
    .unwrap();
    let wait_error = wait.await.unwrap().unwrap_err();
    assert!(matches!(
        wait_error.underlying(),
        GraphError::Cancelled | GraphError::StaleGeneration
    ));
    assert!(matches!(
        old.start().await,
        Err(GraphError::StaleGeneration)
    ));
    assert!(matches!(
        old.set_control_handler(Arc::new(PeerControl::default()))
            .await,
        Err(GraphError::StaleGeneration)
    ));
    let current = component_handle(&core, "source", "source").await;
    assert_eq!(current.id().as_str(), "source");
    assert_eq!(old.id(), current.id());
    assert_ne!(old.generation(), current.generation());
    current.wait_created().await.unwrap();
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 0);
    current.start().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 1);
    let retired_control = current.control().unwrap();
    core.remove_source("source", true).await.unwrap();
    let (source, _) = ControlledSource::new("source");
    let readded = core.add_source_with_handle(source).await.unwrap();
    readded.wait_created().await.unwrap();
    assert_eq!(current.id(), readded.id());
    assert_ne!(current.generation(), readded.generation());
    assert!(matches!(
        current.observed(),
        Err(GraphError::StaleGeneration)
    ));
    assert!(matches!(
        retired_control.ready(),
        Err(ControlError::StaleComponent { .. })
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_additions_are_rejected_without_replacing_the_declared_node() {
    let core = builder().build().await.unwrap();
    let query = core
        .add_query_with_handle(config("query", None))
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let revision = runtime.inspector().unwrap().snapshot().desired.revision;
    assert!(core.add_query(config("query", None)).await.is_err());
    let (source, control) = ControlledSource::new("query");
    assert!(core.add_source_with_handle(source).await.is_err());
    assert_eq!(control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.inspector().unwrap().snapshot().desired.revision,
        revision
    );
    query.wait_created().await.unwrap();
    assert_eq!(
        component_handle(&core, "query", "query").await.generation(),
        query.generation()
    );
    core.shutdown().await.unwrap();
}

fn rejected_addition(error: crate::DrasiError) -> GraphError {
    let crate::DrasiError::Internal(error) = error else {
        panic!("ownership-bearing rejection must preserve its original error");
    };
    let error = error.downcast::<GraphError>().expect("native graph cause");
    let GraphError::AdditionRejected { cause, .. } = &error else {
        panic!("the rejected addition and cause must remain recoverable");
    };
    assert!(matches!(cause.as_ref(), GraphError::Topology { .. }));
    error
}

#[tokio::test]
async fn rejected_ordinary_addition_retains_ownership_and_can_be_resubmitted() {
    let core = builder().build().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let control = runtime.control().unwrap();
    let reserved = ResourceId::try_new(format!(
        "instance/{}",
        runtime.next_instance.load(Ordering::Acquire)
    ))
    .unwrap();
    core.add_computation_component(
        ComponentAddition::new(ConstructedComponent::service(Box::new(NativeService {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("resource-holder").unwrap(),
                vec![],
            )
            .unwrap(),
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        })))
        .auto_start(false)
        .with_resource(
            ResourceSpecification {
                id: reserved.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "test-reservation".into(),
            },
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(LegacyStateStoreResource(Arc::new(
                    crate::state_store::MemoryStateStoreProvider::new(),
                ))),
            ),
        ),
    )
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let (source, source_control) = ControlledSource::new("source");
    let GraphError::AdditionRejected {
        addition: rejection,
        ..
    } = rejected_addition(core.add_source_with_handle(source).await.unwrap_err())
    else {
        unreachable!("validated rejection");
    };
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert!(!runtime
        .current_records()
        .await
        .unwrap()
        .contains_key("source"));
    let addition = rejection
        .take()
        .await
        .expect("ownership must be recoverable");
    assert_eq!(addition.definition.descriptor.id().as_str(), "source");
    assert!(addition.bindings.resources.contains_key(&reserved));
    assert!(rejection.take().await.is_none());

    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![
                DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![
                        ComponentId::try_new("resource-holder").unwrap()
                    ]),
                    policy: RemovalPolicy::Reject,
                },
                DesiredMutation::RemoveResource {
                    resource: reserved,
                    policy: RemovalPolicy::Reject,
                },
            ],
        )
        .await
        .unwrap();
    let removed = control
        .reconcile(preview, TopologyBindings::default())
        .await
        .unwrap();
    assert_eq!(removed.summary, OperationSummary::Completed);
    let handle = control.add_component(addition).await.unwrap();
    handle.wait_created().await.unwrap();
    core.start_source("source").await.unwrap();
    handle.wait_started().await.unwrap();
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 1);
    core.remove_source("source", true).await.unwrap();
    assert_eq!(source_control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_query_and_reaction_errors_retain_graph_owned_cleanup() {
    let core = builder().build().await.unwrap();
    let GraphError::AdditionRejected {
        addition: query, ..
    } = rejected_addition(
        core.add_query_with_handle(config("__inspection_projection__", None))
            .await
            .unwrap_err(),
    )
    else {
        unreachable!("validated rejection");
    };
    let (reaction, reaction_control, _) = ControlledReaction::new("__inspection_projection__", &[]);
    let GraphError::AdditionRejected {
        addition: reaction, ..
    } = rejected_addition(core.add_reaction_with_handle(reaction).await.unwrap_err())
    else {
        unreachable!("validated rejection");
    };
    assert_eq!(reaction_control.initializations.load(Ordering::Acquire), 0);
    assert!(core.list_queries().await.unwrap().is_empty());
    assert!(core.list_reactions().await.unwrap().is_empty());
    core.shutdown().await.unwrap();
    assert!(query.take().await.is_none());
    assert!(reaction.take().await.is_none());
}

mockall::mock! {
    InventoryWal {}

    #[async_trait]
    impl crate::wal::WalProvider for InventoryWal {
        async fn register(&self, source_id: &str, config: crate::wal::WriteAheadLogConfig) -> std::result::Result<(), crate::wal::WalError>;
        async fn append(&self, source_id: &str, event: &drasi_core::models::SourceChange) -> std::result::Result<u64, crate::wal::WalError>;
        async fn read_from(&self, source_id: &str, sequence: u64) -> std::result::Result<Vec<(u64, drasi_core::models::SourceChange)>, crate::wal::WalError>;
        async fn prune_up_to(&self, source_id: &str, sequence: u64) -> std::result::Result<u64, crate::wal::WalError>;
        async fn head_sequence(&self, source_id: &str) -> std::result::Result<u64, crate::wal::WalError>;
        async fn oldest_sequence(&self, source_id: &str) -> std::result::Result<Option<u64>, crate::wal::WalError>;
        async fn event_count(&self, source_id: &str) -> std::result::Result<u64, crate::wal::WalError>;
        async fn delete_wal(&self, source_id: &str) -> std::result::Result<(), crate::wal::WalError>;
    }
}

#[tokio::test]
async fn runtime_services_are_declared_once_and_bound_to_public_component_ids() {
    let state: Arc<dyn crate::state_store::StateStoreProvider> =
        Arc::new(crate::state_store::MemoryStateStoreProvider::new());
    let core = builder()
        .with_state_store_provider(state.clone())
        .with_identity_provider(Arc::new(crate::identity::PasswordIdentityProvider::new(
            "test-user",
            "test-password",
        )))
        .with_wal_provider(Arc::new(MockInventoryWal::new()))
        .with_secret_store_provider(Arc::new(
            crate::secret_store::MemorySecretStoreProvider::new(),
        ))
        .build()
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let scoped = core.computation_plugin_services("separate-graph").unwrap();
    assert!(
        scoped.wal.is_some(),
        "native instance WAL must also reach separately hosted graphs"
    );
    assert!(scoped.state_store.is_some());
    assert!(scoped.identity.is_some());
    assert!(scoped.secrets.is_some());
    let services = [
        ("state", ResourceRole::StateStore),
        ("identity", ResourceRole::Identity),
        ("wal", ResourceRole::Wal),
        ("secrets", ResourceRole::SecretStore),
    ];
    let initial = runtime.inspector().unwrap().snapshot();
    for (name, role) in services {
        let id = ResourceId::try_new(format!("instance.{name}")).unwrap();
        let declaration = &initial.desired.resources[&id];
        assert_eq!(declaration.role, role);
        assert_eq!(declaration.ownership, ResourceOwnership::Borrowed);
        assert_eq!(declaration.binding.as_ref(), id.as_str());
        assert_eq!(
            initial.observed.resources[&id].realization,
            ResourceRealization::Created
        );
        let requirement = &runtime.factory.descriptor().dependencies[name];
        assert_eq!(requirement.role, role);
        assert_eq!(requirement.minimum, 0);
        assert_eq!(requirement.maximum, Some(1));
        assert!(requirement.concrete_type.is_some());
    }

    let (source, _) = ControlledSource::new("source");
    let source = core.add_source_with_handle(source).await.unwrap();
    let query = core
        .add_query_with_handle(config("query", Some("source")))
        .await
        .unwrap();
    let (reaction, _, _) = ControlledReaction::new("reaction", &["query"]);
    let reaction = core.add_reaction_with_handle(reaction).await.unwrap();
    for (id, kind, handle) in [
        ("source", "source", source),
        ("query", "query", query),
        ("reaction", "reaction", reaction),
    ] {
        assert_eq!(handle.id().as_str(), id);
        handle.wait_created().await.unwrap();
        let record = runtime.record(id, kind).await.unwrap();
        assert_eq!(record.node, *handle.id());
        assert_eq!(
            record.resource.as_str(),
            format!("instance/{}", record.token)
        );
        let snapshot = runtime.inspector().unwrap().snapshot();
        let spec = &snapshot.desired.specifications[handle.id()];
        assert_eq!(
            spec.dependencies.get("instance"),
            Some(&vec![record.resource.clone()])
        );
        assert_eq!(spec.dependencies.len(), services.len() + 1);
        for (name, _) in services {
            let id = ResourceId::try_new(format!("instance.{name}")).unwrap();
            assert_eq!(spec.dependencies.get(name), Some(&vec![id.clone()]));
            assert_eq!(
                snapshot.observed.resources[&id].generation,
                initial.observed.resources[&id].generation
            );
        }
    }

    state
        .set("unrelated", "retained", b"shared-state".to_vec())
        .await
        .unwrap();
    core.remove_reaction("reaction", true).await.unwrap();
    core.remove_query("query").await.unwrap();
    core.remove_source("source", true).await.unwrap();
    let snapshot = runtime.inspector().unwrap().snapshot();
    for (name, _) in services {
        let id = ResourceId::try_new(format!("instance.{name}")).unwrap();
        assert!(snapshot.desired.resources.contains_key(&id));
        assert_eq!(
            snapshot.observed.resources[&id].realization,
            ResourceRealization::Created
        );
    }
    assert_eq!(
        state.get("unrelated", "retained").await.unwrap(),
        Some(b"shared-state".to_vec())
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn configured_index_providers_are_borrowed_resources_with_the_original_handles() {
    let primary: Arc<dyn drasi_core::interface::IndexBackendPlugin> =
        Arc::new(DeferredPersistentProvider);
    let secondary: Arc<dyn drasi_core::interface::IndexBackendPlugin> =
        Arc::new(DeferredPersistentProvider);
    let core = builder()
        .with_default_index_provider("primary", primary.clone())
        .with_index_provider("archive / secondary", secondary.clone())
        .build()
        .await
        .unwrap();
    let control = core.computation_control().unwrap();
    let snapshot = control.inspector().snapshot();
    let mut resources = Vec::new();
    for name in ["primary", "archive / secondary"] {
        let encoded: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
        let id = ResourceId::try_new(format!("instance.index/{encoded}")).unwrap();
        let declaration = &snapshot.desired.resources[&id];
        assert_eq!(declaration.role, ResourceRole::IndexBackend);
        assert_eq!(declaration.ownership, ResourceOwnership::Borrowed);
        assert_eq!(declaration.binding.as_ref(), id.as_str());
        assert_eq!(
            snapshot.observed.resources[&id].realization,
            ResourceRealization::Created
        );
        resources.push(id);
    }
    assert_eq!(
        snapshot
            .desired
            .resources
            .values()
            .filter(|resource| resource.role == ResourceRole::IndexBackend)
            .count(),
        2
    );
    let factory = Arc::new(NativeServiceFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/index-inventory", "1").unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::new(),
                allow_additional: false,
            },
            dependencies: BTreeMap::from([(
                Arc::from("indexes"),
                ResourceRequirement {
                    minimum: 2,
                    maximum: Some(2),
                    ..ResourceRequirement::exactly_one::<
                        Arc<dyn drasi_core::interface::IndexBackendPlugin>,
                    >(ResourceRole::IndexBackend)
                },
            )]),
        },
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        index_providers: vec![primary, secondary],
    });
    let spec = ComponentSpecification {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new("inventory-probe").unwrap(),
            vec![],
        )
        .unwrap(),
        role: ComponentRole::Service,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::from([(Arc::from("indexes"), resources)]),
    };
    core.add_computation_component(
        ComponentAddition::from_specification(spec, factory).auto_start(false),
    )
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn index_aliases_share_one_node_and_query_dependencies_follow_reconfiguration() {
    let primary: Arc<dyn drasi_core::interface::IndexBackendPlugin> =
        Arc::new(DeferredPersistentProvider);
    let secondary: Arc<dyn drasi_core::interface::IndexBackendPlugin> =
        Arc::new(DeferredPersistentProvider);
    let core = builder()
        .with_default_index_provider("primary", primary.clone())
        .with_index_provider("alias", primary.clone())
        .with_index_provider("secondary", secondary)
        .with_query(config("query", None))
        .build()
        .await
        .unwrap();
    let query = core.computation_component("query").unwrap();
    query.wait_created().await.unwrap();
    let control = core.computation_control().unwrap();
    let primary_id = index_resource_id("alias").unwrap();
    let secondary_id = index_resource_id("secondary").unwrap();
    let publication = control.registry_snapshot();
    assert_eq!(
        publication
            .desired
            .resources
            .values()
            .filter(|resource| resource.role == ResourceRole::IndexBackend)
            .count(),
        2
    );
    let bound = publication
        .resource(&primary_id)
        .unwrap()
        .get::<Arc<dyn drasi_core::interface::IndexBackendPlugin>>()
        .unwrap();
    assert!(Arc::ptr_eq(bound.as_ref(), &primary));
    assert_eq!(
        publication.desired.specifications[query.id()].dependencies["indexes"],
        vec![primary_id.clone()]
    );
    assert!(control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveResource {
                resource: primary_id.clone(),
                policy: RemovalPolicy::Reject
            }]
        )
        .await
        .is_err());
    let mut replacement = config("query", None);
    replacement.storage_backend =
        Some(crate::indexes::StorageBackendRef::Named("secondary".into()));
    core.update_query("query", replacement).await.unwrap();
    let desired = control.desired_snapshot();
    assert_eq!(
        desired.specifications[query.id()].dependencies["indexes"],
        vec![secondary_id]
    );
    control
        .preview(
            desired.revision,
            vec![DesiredMutation::RemoveResource {
                resource: primary_id,
                policy: RemovalPolicy::Reject,
            }],
        )
        .await
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn inventory_includes_all_scopes_without_aliasing_names_or_retaining_removed_queries() {
    let query_id = ComponentId::try_new("query").unwrap();
    let extra_id = "lib-query/7175657279";
    let extra = ComputationGraph::builder(extra_id)
        .service(Box::new(NativeService {
            descriptor: ComponentDescriptor::try_new(query_id.clone(), vec![]).unwrap(),
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
        }))
        .build()
        .unwrap();
    let (source, _) = ControlledSource::new("source");
    let plugin = PluginIdentity {
        id: "known-source-plugin".into(),
        version: "1.2.3".into(),
    };
    let core = builder()
        .with_source_metadata(
            source,
            HashMap::from([
                ("pluginId".into(), plugin.id.to_string()),
                ("pluginVersion".into(), plugin.version.to_string()),
            ]),
        )
        .with_default_index_provider("primary", Arc::new(DeferredPersistentProvider))
        .with_query(config("query", Some("source")))
        .with_query(config("unresolved", Some("missing")))
        .with_computation_graph(extra, ComputationOptions { auto_start: false })
        .build()
        .await
        .unwrap();
    let handle = core.computation_component("query").unwrap();
    handle.wait_created().await.unwrap();
    assert!(core
        .computation_component("unresolved")
        .unwrap()
        .wait_created()
        .await
        .is_err());
    let runtime = core.computation_runtime.as_ref().unwrap();
    runtime.records.write().await.clear();
    let inventory = core.inspect_computation_inventory().await.unwrap();
    let root = ComputationScope::root("__drasi_lib_runtime__");
    let nested = root.nested(query_id.clone());
    let independent = ComputationScope::root(extra_id);
    assert_eq!(inventory.scopes.len(), 3);
    assert_eq!(
        inventory.scopes[&nested].topology.graph_id.as_ref(),
        extra_id
    );
    assert!(inventory.scopes[&independent].owner.is_none());
    let owner = inventory.scopes[&nested].owner.as_ref().unwrap();
    assert_eq!(
        owner.component,
        root.entity(GraphEntityId::Component(query_id.clone()))
    );
    assert_eq!(owner.generation, handle.generation());
    for scope in [&root, &nested, &independent] {
        assert!(inventory.scopes[scope]
            .topology
            .nodes
            .contains_key(&GraphEntityId::Component(query_id.clone())));
    }
    assert!(inventory
        .entities()
        .any(|(id, entity)| id.scope == nested && matches!(entity, GraphEntity::Pipe(_))));
    assert!(
        inventory
            .entities()
            .any(|(id, entity)| id.scope == root
                && matches!(entity, GraphEntity::SubscriptionPipe(_)))
    );
    assert!(inventory.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(
            ComponentId::try_new("unresolved").unwrap()
        )));
    assert_eq!(
        inventory.plugin_dependents(&plugin),
        BTreeSet::from([root.entity(GraphEntityId::Component(
            ComponentId::try_new("source").unwrap()
        ))])
    );
    let provider = root.entity(GraphEntityId::Resource(
        index_resource_id("primary").unwrap(),
    ));
    let nested_provider = nested.entity(GraphEntityId::Resource(
        ComputationPipelineBuilder::index_resource_id("query").unwrap(),
    ));
    assert!(inventory
        .dependencies(&nested_provider)
        .any(|link| link.to == provider));
    let nested_source = nested.entity(GraphEntityId::Resource(
        ComputationPipelineBuilder::source_resource_id("source").unwrap(),
    ));
    let source = root.entity(GraphEntityId::Component(
        ComponentId::try_new("source").unwrap(),
    ));
    assert!(inventory
        .dependencies(&nested_source)
        .any(|link| link.to == source));
    let state = ResourceId::try_new("instance.state").unwrap();
    assert!(inventory
        .dependencies(&nested.entity(GraphEntityId::Resource(state.clone())))
        .any(|link| link.to == root.entity(GraphEntityId::Resource(state.clone()))));
    // The fixture's legacy index interface cannot delete persistent namespaces.
    let mut memory = config("query", Some("source"));
    memory.storage_backend = Some(crate::indexes::StorageBackendRef::Inline(
        crate::indexes::StorageBackendSpec::Memory {
            enable_archive: false,
        },
    ));
    core.update_query("query", memory).await.unwrap();
    core.remove_query("query").await.unwrap();
    let after = core.inspect_computation_inventory().await.unwrap();
    assert!(!after.scopes.contains_key(&nested));
    assert!(after.scopes.contains_key(&independent));
    assert!(!after
        .links
        .iter()
        .any(|link| link.from.scope == nested || link.to.scope == nested));
    assert!(
        inventory.scopes.contains_key(&nested),
        "earlier snapshots remain immutable"
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn instance_root_identity_is_rejected_by_native_admission_with_cleanup_ownership() {
    let core = builder().with_id("instance").build().await.unwrap();
    let (source, source_control) = ControlledSource::new("instance");
    let GraphError::AdditionRejected { addition, .. } =
        rejected_addition(core.add_source_with_handle(source).await.unwrap_err())
    else {
        unreachable!("ownership-bearing rejection");
    };
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert!(core.get_source_status("instance").await.is_err());
    assert!(core
        .computation_control()
        .unwrap()
        .inspector()
        .snapshot()
        .observed
        .components[&ComponentId::try_new("__inspection_projection__").unwrap()]
        .failure
        .is_none());
    core.shutdown().await.unwrap();
    assert!(addition.take().await.is_none());
}

#[tokio::test]
async fn runtime_factory_validates_captured_services_without_requiring_absent_providers() {
    let core = builder().build().await.unwrap();
    let (source, _) = ControlledSource::new("source");
    let handle = core.add_source_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let record = runtime.record("source", "source").await.unwrap();
    let snapshot = runtime.inspector().unwrap().snapshot();
    assert!(!snapshot
        .desired
        .resources
        .values()
        .any(|resource| resource.role == ResourceRole::IndexBackend));
    let spec = &snapshot.desired.specifications[handle.id()];
    for name in ["identity", "wal", "secrets"] {
        assert!(!spec.dependencies.contains_key(name));
        assert!(!snapshot
            .desired
            .resources
            .contains_key(&ResourceId::try_new(format!("instance.{name}")).unwrap()));
    }
    let state = ResourceId::try_new("instance.state").unwrap();
    let mut bindings = BTreeMap::from([
        (
            record.resource.clone(),
            ResourceHandle::new(ResourceRole::Component, record.owner.clone())
                .with_cleanup(record.owner.clone()),
        ),
        (
            state.clone(),
            ResourceHandle::new(
                ResourceRole::StateStore,
                Arc::new(LegacyStateStoreResource(
                    runtime.services.state_store.clone().unwrap(),
                )),
            ),
        ),
    ]);
    runtime
        .factory
        .validate_resources(spec, &snapshot.desired.resources, &bindings)
        .unwrap();
    let mut missing = spec.clone();
    missing.dependencies.remove("state");
    let error = runtime
        .factory
        .validate_resources(&missing, &snapshot.desired.resources, &bindings)
        .unwrap_err();
    assert!(error.to_string().contains("state must match its host"));

    bindings.insert(
        state,
        ResourceHandle::new(
            ResourceRole::StateStore,
            Arc::new(LegacyStateStoreResource(Arc::new(
                crate::state_store::MemoryStateStoreProvider::new(),
            ))),
        ),
    );
    let error = runtime
        .factory
        .validate_resources(spec, &snapshot.desired.resources, &bindings)
        .unwrap_err();
    assert!(error.to_string().contains("state differs from its host"));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn auto_start_is_requested_only_for_a_running_instance_and_failures_stay_on_nodes() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    let source = core.add_source_with_handle(source).await.unwrap();
    source.wait_created().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    let runtime = core.computation_runtime.as_ref().unwrap();
    runtime.project().await.unwrap();
    assert_eq!(
        core.get_source_status("source").await.unwrap(),
        ComponentStatus::Added
    );
    let snapshot = runtime.inspector().unwrap().snapshot();
    assert!(snapshot.desired.lifecycle_policies[source.id()].auto_start);
    assert_eq!(
        snapshot.desired.specifications[source.id()]
            .configuration
            .get("definition"),
        Some(&ConfigurationValue::Literal(
            serde_json::json!({"auto_start": true})
        ))
    );
    core.start().await.unwrap();
    source.wait_started().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    let (bad, bad_control) = ControlledSource::new("bad");
    bad_control.auto_start.store(true, Ordering::Release);
    bad_control.fail_starts.store(1, Ordering::Release);
    core.add_source(bad).await.unwrap();
    let bad = component_handle(&core, "bad", "source").await;
    bad.wait_created().await.unwrap();
    assert!(bad.wait_started().await.is_err());
    runtime.project().await.unwrap();
    assert_eq!(
        core.get_source_status("bad").await.unwrap(),
        ComponentStatus::Error
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_start_hooks_do_not_complete_handle_waits_before_plugin_running() {
    let core = builder().build().await.unwrap();
    let (source, source_control) = ControlledSource::new("source");
    source_control.starting_only.store(true, Ordering::Release);
    let source_status = source.inner.status_handle();
    let source = core.add_source_with_handle(source).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), core.start_source("source"))
        .await
        .expect("ordinary start must complete after the plugin's start hook")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), source.wait_started())
            .await
            .is_err()
    );
    assert!(!source.observed().unwrap().started);
    source_status
        .set_status(ComponentStatus::Running, None)
        .await;
    tokio::time::timeout(Duration::from_secs(3), source.wait_started())
        .await
        .unwrap()
        .unwrap();

    let query = core
        .add_query_with_handle(config("query", None))
        .await
        .unwrap();
    core.start_query("query").await.unwrap();
    query.wait_started().await.unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::new("reaction", &["query"]);
    reaction_control
        .starting_only
        .store(true, Ordering::Release);
    let reaction_status = reaction.base.status_handle();
    let reaction = core.add_reaction_with_handle(reaction).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), core.start_reaction("reaction"))
        .await
        .expect("ordinary reaction start must retain hook-completion semantics")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.wait_started())
            .await
            .is_err()
    );
    assert!(!reaction.observed().unwrap().started);
    reaction_status
        .set_status(ComponentStatus::Running, None)
        .await;
    tokio::time::timeout(Duration::from_secs(3), reaction.wait_started())
        .await
        .unwrap()
        .unwrap();
    reaction.stop().await.unwrap();
    reaction.wait_started().await.unwrap();
    source.stop().await.unwrap();
    source.wait_started().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_plugin_failure_before_running_fails_the_start_wait_not_the_addition() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.starting_only.store(true, Ordering::Release);
    let status = source.inner.status_handle();
    let handle = core.add_source_with_handle(source).await.unwrap();
    core.start_source("source").await.unwrap();
    assert!(!handle.observed().unwrap().started);
    status
        .set_status(
            ComponentStatus::Error,
            Some("failure before plugin readiness".into()),
        )
        .await;
    let error = tokio::time::timeout(Duration::from_secs(3), handle.wait_started())
        .await
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains("failure before plugin readiness"));
    assert!(!handle.observed().unwrap().started);
    core.remove_source("source", true).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_start_does_not_wait_for_the_instance_subscription_fence() {
    let (source, control) = ControlledSource::new("source");
    control.auto_start.store(true, Ordering::Release);
    control
        .release_bootstrap_at_fence
        .store(true, Ordering::Release);
    source
        .inner
        .set_bootstrap_provider(GatedBootstrap(control.clone()))
        .await;
    let query = crate::Query::cypher("query")
        .query("MATCH (n:Person) RETURN n")
        .from_source("source")
        .enable_bootstrap(true)
        .auto_start(true)
        .build();
    let core = builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await
        .unwrap();
    let query = component_handle(&core, "query", "query").await;
    tokio::time::timeout(Duration::from_secs(3), core.start())
        .await
        .expect("query start must let the instance release its source subscription fence")
        .unwrap();
    assert_eq!(control.subscription_fences.load(Ordering::Acquire), 1);
    tokio::time::timeout(Duration::from_secs(3), query.wait_started())
        .await
        .unwrap()
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_handle_waits_for_bootstrap_completion_and_reports_bootstrap_failure() {
    for fail in [false, true] {
        let (source, control) = ControlledSource::new("source");
        control.auto_start.store(true, Ordering::Release);
        control.fail_bootstrap.store(fail, Ordering::Release);
        source
            .inner
            .set_bootstrap_provider(GatedBootstrap(control.clone()))
            .await;
        let query = crate::Query::cypher("query")
            .query("MATCH (n:Person) RETURN n")
            .from_source("source")
            .enable_bootstrap(true)
            .auto_start(true)
            .build();
        let core = builder()
            .with_source(source)
            .with_query(query)
            .build()
            .await
            .unwrap();
        let query = component_handle(&core, "query", "query").await;
        tokio::time::timeout(Duration::from_secs(3), core.start())
            .await
            .expect("instance startup must not wait for unfinished query bootstrap")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), control.bootstrap_entered.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), query.wait_started())
                .await
                .is_err()
        );
        assert!(!query.observed().unwrap().started);
        control.bootstrap_release.notify_one();
        let ready = tokio::time::timeout(Duration::from_secs(3), query.wait_started())
            .await
            .expect("bootstrap completion or failure must settle the start wait");
        if fail {
            assert!(ready.is_err());
            assert!(!query.observed().unwrap().started);
            assert!(query.observed().unwrap().failure.is_some());
        } else {
            ready.unwrap();
            let instance = core
                .computation_runtime
                .as_ref()
                .unwrap()
                .query("query")
                .await
                .unwrap();
            assert_eq!(
                QueryTrait::status(instance.as_ref()).await,
                ComponentStatus::Running
            );
        }
        core.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn native_components_share_instance_lifecycle_without_starting_manual_components() {
    let core = builder().build().await.unwrap();
    let mut counts = Vec::new();
    for (id, auto_start) in [("automatic", true), ("manual", false)] {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let handle = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::service(Box::new(NativeService {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new(id).unwrap(),
                        vec![],
                    )
                    .unwrap(),
                    starts: starts.clone(),
                    stops: stops.clone(),
                })))
                .auto_start(auto_start),
            )
            .await
            .unwrap();
        handle.wait_created().await.unwrap();
        assert_eq!(starts.load(Ordering::Acquire), 0);
        counts.push((handle, starts, stops));
    }
    for expected in 1..=2 {
        core.start().await.unwrap();
        counts[0].0.wait_started().await.unwrap();
        assert_eq!(counts[0].1.load(Ordering::Acquire), expected);
        assert_eq!(counts[1].1.load(Ordering::Acquire), 0);
        core.stop().await.unwrap();
        assert_eq!(counts[0].2.load(Ordering::Acquire), expected);
        assert_eq!(counts[1].2.load(Ordering::Acquire), 0);
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_startup_reports_blocked_and_failed_nodes_without_misclassifying_native_config() {
    let (source, source_control) = ControlledSource::new("ordinary");
    let core = builder().with_source(source).build().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let record = runtime.record("ordinary", "source").await.unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(NativeServiceFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/native-service", "1").unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::new(),
                allow_additional: true,
            },
            dependencies: BTreeMap::new(),
        },
        starts: starts.clone(),
        stops: Arc::new(AtomicUsize::new(0)),
        index_providers: Vec::new(),
    });
    for (id, fail) in [("native-ready", false), ("native-failed", true)] {
        let spec = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(ComponentId::try_new(id).unwrap(), vec![])
                .unwrap(),
            role: ComponentRole::Service,
            completion: None,
            implementation: factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("record_token"),
                    ConfigurationValue::Literal(record.token.into()),
                ),
                (Arc::from("fail"), ConfigurationValue::Literal(fail.into())),
            ]),
            dependencies: BTreeMap::new(),
        };
        core.add_computation_component(
            ComponentAddition::from_specification(spec, factory.clone()).auto_start(true),
        )
        .await
        .unwrap();
    }
    let blocked = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::source(Box::new(
                UnboundNativeSource {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("native-blocked").unwrap(),
                        vec![PortDescriptor::new(
                            PortId::try_new("out").unwrap(),
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                },
            )))
            .auto_start(true),
        )
        .await
        .unwrap();
    assert_eq!(runtime.current_records().await.unwrap().len(), 2);
    let error = tokio::time::timeout(Duration::from_secs(3), runtime.start_native_components())
        .await
        .expect("blocked and failed creation must not leave startup waiting")
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("native-blocked"), "{message}");
    assert!(
        message.contains("injected native creation failure"),
        "{message}"
    );
    assert_eq!(
        blocked.observed().unwrap().realization,
        RealizationState::Blocked
    );
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert_eq!(source_control.starts.load(Ordering::Acquire), 0);
    assert_eq!(
        core.computation_control()
            .unwrap()
            .component_handle(&record.node)
            .unwrap()
            .generation(),
        runtime.handle_for(&record).unwrap().generation()
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_addition_errors_and_public_result_signatures_are_unchanged() {
    let core = DrasiLib::builder()
        .with_execution_mode(crate::ExecutionMode::ComponentGraph)
        .build()
        .await
        .unwrap();
    let (source, control) = ControlledSource::new("not-added");
    assert!(matches!(
        core.add_source_with_handle(source).await,
        Err(crate::DrasiError::InvalidState { .. })
    ));
    assert_eq!(control.initializations.load(Ordering::Acquire), 0);
    let mut invalid = config("invalid", None);
    invalid.query = "this is not a query".into();
    let result: crate::Result<()> = core.add_query(invalid).await;
    result.unwrap();
    assert!(core.start_query("invalid").await.is_err());
    core.remove_query("invalid").await.unwrap();
    assert!(core
        .add_query(config("missing", Some("source")))
        .await
        .is_err());
    assert!(core.list_queries().await.unwrap().is_empty());
    let (source, source_control) = ControlledSource::new("source");
    let added: crate::Result<()> = core.add_source(source).await;
    added.unwrap();
    assert!(source_control.resource_observers.lock().unwrap().is_empty());
    core.add_query(config("query", Some("source")))
        .await
        .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::new("reaction", &["query"]);
    let added: crate::Result<()> = core.add_reaction(reaction).await;
    added.unwrap();
    assert!(reaction_control
        .resource_observers
        .lock()
        .unwrap()
        .is_empty());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn plugin_origin_is_preserved_only_when_the_host_supplies_an_identity() {
    let core = builder().build().await.unwrap();
    let metadata = HashMap::from([
        ("pluginId".into(), "example/plugin".into()),
        ("pluginVersion".into(), "1.2.3".into()),
    ]);
    let expected = PluginIdentity {
        id: "example/plugin".into(),
        version: "1.2.3".into(),
    };
    let (plugin, _) = ControlledSource::new("plugin-source");
    core.add_source_with_metadata(plugin, metadata.clone())
        .await
        .unwrap();
    let (custom, _) = ControlledSource::new("custom-source");
    core.add_source(custom).await.unwrap();
    core.add_query(config("query", None)).await.unwrap();
    let (plugin, _, _) = ControlledReaction::new("plugin-reaction", &["query"]);
    core.add_reaction_with_metadata(plugin, metadata)
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    for (id, kind, expected) in [
        ("plugin-source", "source", Some(&expected)),
        ("custom-source", "source", None),
        ("plugin-reaction", "reaction", Some(&expected)),
    ] {
        let handle = component_handle(&core, id, kind).await;
        handle.wait_created().await.unwrap();
        let snapshot = runtime.inspector().unwrap().snapshot();
        assert_eq!(
            snapshot.desired.specifications[handle.id()]
                .descriptor
                .plugin_identity(),
            expected,
        );
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn data_subscriptions_track_actual_nodes_and_changed_dependencies() {
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
    let runtime = core.computation_runtime.as_ref().unwrap();
    let nodes = runtime.current_records().await.unwrap();
    for id in ["a", "b", "q1", "q2", "reaction"] {
        assert_eq!(nodes[id].node.as_str(), id);
    }
    let a = nodes["a"].node.clone();
    let b = nodes["b"].node.clone();
    let q1 = nodes["q1"].node.clone();
    let q2 = nodes["q2"].node.clone();
    let reaction = nodes["reaction"].node.clone();
    let control_only = vec![(b.clone(), reaction.clone())];
    runtime
        .control()
        .unwrap()
        .set_control_connections(control_only.clone())
        .await
        .unwrap();
    let mut expected =
        vec![(a.clone(), q1.clone()), (b.clone(), q2.clone()), (q1.clone(), reaction.clone())];
    expected.sort();
    assert_eq!(
        runtime
            .parent()
            .unwrap()
            .control()
            .desired_snapshot()
            .subscriptions
            .as_ref(),
        expected.as_slice()
    );
    assert_eq!(
        runtime
            .control()
            .unwrap()
            .desired_snapshot()
            .control_connections
            .as_ref(),
        control_only.as_slice()
    );
    core.update_query("q1", config("q1", Some("b")))
        .await
        .unwrap();
    let (new_reaction, _, _) = ControlledReaction::new("reaction", &["q2"]);
    core.update_reaction("reaction", new_reaction)
        .await
        .unwrap();
    let mut expected =
        vec![(b.clone(), q1.clone()), (b.clone(), q2.clone()), (q2.clone(), reaction.clone())];
    expected.sort();
    assert_eq!(
        runtime
            .parent()
            .unwrap()
            .control()
            .desired_snapshot()
            .subscriptions
            .as_ref(),
        expected.as_slice()
    );
    assert_eq!(
        runtime
            .control()
            .unwrap()
            .desired_snapshot()
            .control_connections
            .as_ref(),
        control_only.as_slice()
    );
    core.remove_source("a", false).await.unwrap();
    core.remove_reaction("reaction", false).await.unwrap();
    core.remove_query("q1").await.unwrap();
    assert_eq!(
        runtime
            .parent()
            .unwrap()
            .control()
            .desired_snapshot()
            .subscriptions
            .as_ref(),
        &[(b, q2)]
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_control_handlers_run_while_legacy_start_and_enqueue_are_blocked() {
    let (source, source_control) = ControlledSource::new("source");
    source_control.pending_start.store(true, Ordering::Release);
    let (reaction, reaction_control, mut output) = ControlledReaction::new("reaction", &["query"]);
    let core = Arc::new(builder().build().await.unwrap());
    let source = core.add_source_with_handle(source).await.unwrap();
    let query = core
        .add_query_with_handle(config("query", Some("source")))
        .await
        .unwrap();
    let reaction = core.add_reaction_with_handle(reaction).await.unwrap();
    reaction.wait_created().await.unwrap();
    let source_messages = Arc::new(PeerControl::default());
    let reaction_messages = Arc::new(PeerControl::default());
    source
        .set_control_handler(source_messages.clone())
        .await
        .unwrap();
    reaction
        .set_control_handler(reaction_messages.clone())
        .await
        .unwrap();
    let start = tokio::spawn({
        let core = core.clone();
        async move { core.start_source("source").await }
    });
    tokio::time::timeout(Duration::from_secs(3), source_control.entered.notified())
        .await
        .unwrap();
    let sender = query.control().unwrap();
    assert_eq!(sender.id().as_str(), "query");
    sender.ready().unwrap();
    let ready = source_messages
        .wait_for(|message| {
            message.from == *query.id() && message.notification == ControlNotification::Ready
        })
        .await;
    assert_eq!(ready.direction, ControlDirection::Upstream);
    core.stop_source("source").await.unwrap();
    assert!(start.await.unwrap().is_err());
    source_control.pending_start.store(false, Ordering::Release);
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    reaction_control
        .pending_enqueue
        .store(true, Ordering::Release);
    insert(&core, "source").await;
    tokio::time::timeout(
        Duration::from_secs(3),
        reaction_control.enqueue_entered.notified(),
    )
    .await
    .unwrap();
    sender
        .notify_downstream(ControlNotification::Custom {
            kind: "data-operation-blocked".into(),
            payload: serde_json::json!({"query": "query"}),
        })
        .unwrap();
    let notification = reaction_messages
        .wait_for(|message| {
            message.from == *query.id()
                && matches!(&message.notification, ControlNotification::Custom { kind, .. }
                    if kind == "data-operation-blocked")
        })
        .await;
    assert_eq!(notification.direction, ControlDirection::Downstream);
    reaction_control.enqueue_release.notify_one();
    assert_eq!(next(&mut output).await.results.len(), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn controller_notifies_neighbors_of_legacy_plugin_failures_without_trait_hooks() {
    let core = builder().build().await.unwrap();
    let (source, source_control) = ControlledSource::new("source");
    source_control.fail_starts.store(1, Ordering::Release);
    core.add_source(source).await.unwrap();
    let query = core
        .add_query_with_handle(config("query", Some("source")))
        .await
        .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::new("reaction", &["query"]);
    core.add_reaction_with_handle(reaction)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let messages = Arc::new(PeerControl::default());
    query.set_control_handler(messages.clone()).await.unwrap();

    assert!(core.start_source("source").await.is_err());
    let failure = messages
        .wait_for(|message| {
            message.from.as_str() == "source"
                && matches!(&message.notification, ControlNotification::Unavailable { reason }
                    if reason.contains("injected start failure"))
        })
        .await;
    assert_eq!(failure.direction, ControlDirection::Downstream);

    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    let updates = reaction_control.updates.lock().unwrap().clone().unwrap();
    updates
        .send(ComponentUpdate::Status {
            component_id: "reaction".into(),
            status: ComponentStatus::Error,
            message: Some("legacy reaction background failure".into()),
        })
        .await
        .unwrap();
    let failure = messages
        .wait_for(|message| {
            message.from.as_str() == "reaction"
                && matches!(&message.notification, ControlNotification::Unavailable { reason }
                    if reason.contains("legacy reaction background failure"))
        })
        .await;
    assert_eq!(failure.direction, ControlDirection::Upstream);
    core.shutdown().await.unwrap();
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
async fn invalid_reaction_constructor_leaves_an_owned_error_node_not_a_ready_instance() {
    let core = builder()
        .with_query(config("query", None))
        .build()
        .await
        .unwrap();
    let (bad, _, _) = ControlledReaction::new("reaction", &["query", "query"]);
    core.add_reaction(bad).await.unwrap();
    let handle = component_handle(&core, "reaction", "reaction").await;
    assert_creation_failed(&core, &handle, "duplicate query IDs").await;
    let reactions = core.list_reactions().await.unwrap();
    assert_eq!(reactions, vec![("reaction".into(), ComponentStatus::Error)]);
    let runtime = core.computation_runtime.as_ref().unwrap();
    let record = runtime.record("reaction", "reaction").await.unwrap();
    let Value::Reaction(reaction) = &record.value else {
        panic!("declared reaction must retain its plugin");
    };
    assert_eq!(reaction.query_ids, ["query", "query"]);
    assert_eq!(reaction.reaction.id(), "reaction");
    let (duplicate, _, _) = ControlledReaction::new("reaction", &["query"]);
    assert!(core.add_reaction(duplicate).await.is_err());
    core.remove_reaction("reaction", false).await.unwrap();
    assert!(handle.wait_created().await.is_err());
    let (good, _, _) = ControlledReaction::new("reaction", &["query"]);
    let replacement = core.add_reaction_with_handle(good).await.unwrap();
    replacement.wait_created().await.unwrap();
    core.remove_reaction("reaction", false).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_creation_failure_remains_addressable_for_awaited_cleanup() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::new("source");
    control.fail_initialize.store(true, Ordering::Release);
    core.add_source(source).await.unwrap();
    let handle = component_handle(&core, "source", "source").await;
    assert_creation_failed(&core, &handle, "injected initialization failure").await;
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
    let old_handle = component_handle(&core, "source", "source").await;
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
    let replacement = component_handle(&core, "source", "source").await;
    assert_ne!(replacement.generation(), old_handle.generation());
    update.abort();
    assert!(update.await.unwrap_err().is_cancelled());
    new_control.release.notify_one();
    let runtime = core.computation_runtime.as_ref().unwrap();
    replacement.wait_created().await.unwrap();
    runtime.project().await.unwrap();
    let current = core.source_instance("source").await.unwrap();
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
        .query_manager()
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
async fn declared_references_validate_roles_on_the_committed_node() {
    let (source, _) = ControlledSource::new("source");
    let core = builder().with_source(source).build().await.unwrap();
    let runtime = core.computation_runtime.as_ref().unwrap();
    let revision = runtime.inspector().unwrap().snapshot().desired.revision;
    let (reaction, _, _) = ControlledReaction::new("reaction", &["source"]);
    runtime.declare_reaction(Box::new(reaction)).await.unwrap();
    assert_ne!(
        runtime.inspector().unwrap().snapshot().desired.revision,
        revision
    );
    let handle = component_handle(&core, "reaction", "reaction").await;
    assert_creation_failed(&core, &handle, "not a Query").await;
    assert_eq!(
        core.list_reactions().await.unwrap(),
        vec![("reaction".into(), ComponentStatus::Error)]
    );
    core.remove_reaction("reaction", false).await.unwrap();
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
        .query_manager()
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

async fn quiescing_ordinary_query_pauses_its_independently_driven_graph() {
    let (source, source_control) = ControlledSource::new("source");
    let (reaction, reaction_control, mut output) = ControlledReaction::new("reaction", &["query"]);
    let core = builder()
        .with_source(source)
        .with_query(config("query", Some("source")))
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source("source").await.unwrap();
    core.start_query("query").await.unwrap();
    core.start_reaction("reaction").await.unwrap();
    insert(&core, "source").await;
    assert_eq!(next(&mut output).await.results.len(), 1);

    let runtime = core.computation_runtime.as_ref().unwrap();
    let control = runtime.control().unwrap();
    let query = component_handle(&core, "query", "query").await;
    let selected = GraphSelection::Exact(vec![ComponentId::try_new("query").unwrap()]);
    control
        .quiesce_components(control.desired_snapshot().revision, selected)
        .await
        .unwrap();
    assert_eq!(
        query.observed().unwrap().lifecycle,
        ComponentLifecycle::Quiesced
    );
    insert_person(&core, "source", "two", "Bob").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), output.recv())
            .await
            .is_err(),
        "a quiesced ordinary query must not keep evaluating on its nested driver"
    );
    query.start().await.unwrap();
    let instance = runtime.query("query").await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), output.recv())
        .await
        .unwrap_or_else(|error| {
            panic!(
                "query did not resume: {error}; parent: {:?}; nested: {:?}",
                query.observed().unwrap(),
                instance.inspector().snapshot().observed.components
            )
        })
        .unwrap();
    assert!(matches!(&result.results[..],
        [crate::channels::ResultDiff::Add { data, .. }]
        if data == &serde_json::json!({"name":"Bob"})));
    assert_eq!(source_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(source_control.subscriptions.load(Ordering::Acquire), 1);
    assert_eq!(reaction_control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_query_quiescence_current_thread() {
    quiescing_ordinary_query_pauses_its_independently_driven_graph().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_query_quiescence_multi_thread() {
    quiescing_ordinary_query_pauses_its_independently_driven_graph().await;
}
