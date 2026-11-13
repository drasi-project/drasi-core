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
use crate::channels::ComponentUpdate;
use crate::test_helpers::creation::{
    BuilderFixtures, CoreFixtures, ReactionFixture, SourceFixture,
};
use crate::{
    channels::{QueryResult, SubscriptionResponse},
    config::SourceSubscriptionSettings,
    reactions::common::base::{ReactionBase, ReactionBaseParams},
    sources::{SourceBase, SourceBaseParams},
    SourceRuntimeContext,
};
use async_trait::async_trait;
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

#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;

#[path = "output_catalogue_tests.rs"]
mod output_catalogue_tests;

#[path = "source_schema_tests.rs"]
mod source_schema_tests;

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

#[derive(Debug, thiserror::Error)]
#[error("injected native creation failure")]
struct NativeCreationFailure;

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
            return Err(ComponentCreationError::terminal(NativeCreationFailure));
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
    drops: AtomicUsize,
    status: std::sync::OnceLock<crate::component_graph::ComponentStatusHandle>,
    schema: StdMutex<Option<crate::schema::SourceSchema>>,
    properties_read: StdMutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    resource_observers: StdMutex<Vec<Arc<dyn crate::context::ComponentResourceObserver>>>,
    updates: StdMutex<Option<crate::channels::ComponentUpdateSender>>,
    initializations: AtomicUsize,
    auto_start: AtomicBool,
    starts: AtomicUsize,
    stops: AtomicUsize,
    deprovisions: AtomicUsize,
    subscriptions: AtomicUsize,
    pending_subscription: AtomicBool,
    subscription_entered: Notify,
    subscription_release: Notify,
    fail_starts: AtomicUsize,
    fail_stop: AtomicBool,
    noop_stop: AtomicBool,
    pending_start: AtomicBool,
    waiting_start: AtomicBool,
    starting_only: AtomicBool,
    subscription_fences: AtomicUsize,
    release_bootstrap_at_fence: AtomicBool,
    panic_subscription_fence: AtomicBool,
    bootstrap_entered: Notify,
    bootstrap_release: Notify,
    fail_bootstrap: AtomicBool,
    fail_initialize: AtomicBool,
    pending_initialize: AtomicBool,
    entered: Notify,
    release: Notify,
    pending_stop: AtomicBool,
    stop_entered: Notify,
    stop_release: Notify,
}

impl SourceControl {
    async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        self.status
            .get()
            .expect("source constructed")
            .set_status(status, message)
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configuration_snapshot_ignores_observation_only_publications() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("snapshot-source")
            .expect("fixture component name"),
    );
    let core = builder().with_source_fixture(source).build().await.unwrap();
    core.start().await.unwrap();
    let handle = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("snapshot-source")
                .expect("fixture component name"),
        )
        .unwrap();
    handle.wait_created().await.unwrap();
    let source_id = handle.id().clone();
    let control = core.computation_control().unwrap();
    let revision = control.desired_snapshot().revision;
    let (requests, mut pending) =
        tokio::sync::mpsc::unbounded_channel::<std::sync::mpsc::SyncSender<()>>();
    let reports = Arc::new(AtomicUsize::new(0));
    let completed = reports.clone();
    let observer = tokio::spawn(async move {
        while let Some(reply) = pending.recv().await {
            let node = handle.observed().unwrap();
            control
                .report_health(
                    revision,
                    HealthObservation {
                        component: handle.id().clone(),
                        generation: node.generation,
                        operation: node.operation,
                        health: ComponentHealth::Healthy,
                    },
                )
                .await
                .unwrap();
            completed.fetch_add(1, Ordering::SeqCst);
            reply.send(()).unwrap();
        }
    });
    *source_control.properties_read.lock().unwrap() = Some(Arc::new(move || {
        let (reply, received) = std::sync::mpsc::sync_channel(1);
        requests.send(reply).unwrap();
        // Force a status publication during each synchronous configuration read.
        received.recv_timeout(Duration::from_secs(5)).unwrap();
    }));
    let snapshot = core.snapshot_computation_configuration().await;
    *source_control.properties_read.lock().unwrap() = None;
    observer.await.unwrap();
    core.shutdown().await.unwrap();
    assert!(reports.load(Ordering::SeqCst) > 0);
    let snapshot = snapshot.expect("status updates do not change configuration");
    assert!(snapshot
        .instance
        .sources
        .iter()
        .any(|source| source.id == source_id));
}

#[tokio::test]
async fn graph_registry_is_authoritative_for_owned_components() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, reaction_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    let source_id = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let query_id = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let reaction_id = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    for id in [&source_id, &query_id, &reaction_id] {
        core.component_by_id(id)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    let runtime = core.computation_runtime.as_ref();
    let records = runtime.current_records().await.unwrap();
    assert!([&source_id, &reaction_id]
        .into_iter()
        .all(|id| records.contains_key(id)));
    assert!(runtime
        .ordinary_queries_at(&runtime.control().unwrap().registry_snapshot())
        .unwrap()
        .contains_key(&query_id));
    assert!(core
        .list_sources()
        .await
        .unwrap()
        .iter()
        .any(|entry| entry.id == source_id));
    let queries = core.list_queries().await.unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0].id, query_id);
    assert_eq!(
        queries[0].name,
        crate::ComponentName::try_new("query").expect("fixture component name")
    );
    assert_eq!(queries[0].status, ComponentStatus::Added);
    let reactions = core.list_reactions().await.unwrap();
    assert_eq!(reactions.len(), 1);
    assert_eq!(reactions[0].id, reaction_id);
    assert_eq!(
        reactions[0].name,
        crate::ComponentName::try_new("reaction").expect("fixture component name")
    );
    assert_eq!(reactions[0].status, ComponentStatus::Added);
    assert_eq!(
        core.get_source_info_by_id(&source_id)
            .await
            .unwrap()
            .source_type,
        "controlled"
    );
    assert_eq!(
        core.get_query_info_by_id(&query_id).await.unwrap().status,
        ComponentStatus::Added
    );
    assert_eq!(
        core.get_reaction_info_by_id(&reaction_id)
            .await
            .unwrap()
            .queries,
        vec![crate::config::ComponentReference::by_id(query_id.clone())]
    );
    assert!(core.get_source_info_by_id(&query_id).await.is_err());
    assert_eq!(
        core.get_query_config_by_id(&query_id).await.unwrap().inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    assert_eq!(core.get_current_config().await.unwrap().queries.len(), 1);
    core.get_query_output_metrics_by_id(&query_id)
        .await
        .unwrap();
    core.subscribe_source_logs_by_id(&source_id).await.unwrap();
    core.subscribe_query_logs_by_id(&query_id).await.unwrap();
    core.subscribe_reaction_logs_by_id(&reaction_id)
        .await
        .unwrap();
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(snapshot
        .sources
        .iter()
        .any(|source| source.id == source_id.clone()));
    assert_eq!(snapshot.queries.len(), 1);
    assert_eq!(snapshot.reactions.len(), 1);
    assert!(snapshot.edges.iter().any(|edge| {
        edge.from == GraphEntityId::Component(source_id.clone())
            && edge.to == GraphEntityId::Component(query_id.clone())
    }));
    core.start_source_by_id(&source_id).await.unwrap();
    core.start_query_by_id(&query_id).await.unwrap();
    core.start_reaction_by_id(&reaction_id).await.unwrap();
    assert_eq!(source_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(reaction_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(
        core.get_query_config_by_id(&query_id).await.unwrap().inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    assert!(core.remove_source_by_id(&source_id, false).await.is_err());
    core.remove_reaction_by_id(&reaction_id, false)
        .await
        .unwrap();
    core.remove_query_by_id(&query_id).await.unwrap();
    core.remove_source_by_id(&source_id, false).await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_operations_share_the_single_computation_runtime() {
    use futures::StreamExt;

    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    for id in ["source", "query", "reaction"] {
        let name = crate::ComponentName::try_new(id).expect("fixture component name");
        core.component_by_name(&name)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    assert_eq!(
        core.inspect_computation_inventory()
            .await
            .unwrap()
            .scopes
            .values()
            .filter(|scope| scope.owner.is_none())
            .count(),
        1
    );
    core.list_sources().await.unwrap();
    core.list_queries().await.unwrap();
    core.list_reactions().await.unwrap();
    core.get_source_info_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_query_info_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_reaction_info_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_source_schema_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_graph_schema().await.unwrap();
    core.get_current_config().await.unwrap();
    core.snapshot_configuration().await.unwrap();
    core.get_query_output_metrics_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_reaction_metrics_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_lifecycle_metrics().await.unwrap();
    core.subscribe_source_logs_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.subscribe_query_logs_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.subscribe_reaction_logs_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.subscribe_source_events_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.subscribe_query_events_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.subscribe_reaction_events_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_source_events_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap()
    .collect::<Vec<_>>()
    .await;
    core.get_query_events_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap()
    .collect::<Vec<_>>()
    .await;
    core.get_reaction_events_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
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
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .unwrap()
    .wait_started()
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.get_query_results_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start().await.unwrap();
    core.stop().await.unwrap();
    let unchanged = core
        .get_query_config_by_name(&ComponentName::try_new("query").unwrap())
        .await
        .unwrap()
        .into_authoring();
    core.update_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        unchanged,
    )
    .await
    .unwrap();
    core.remove_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_manager_is_a_shared_graph_backed_facade() {
    let core = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .build()
        .await
        .unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let clone = core.clone();
    assert!(std::ptr::eq(core.query_manager(), clone.query_manager()));
    assert_eq!(
        core.inspect_computation_inventory()
            .await
            .unwrap()
            .scopes
            .values()
            .filter(|scope| scope.owner.is_none())
            .count(),
        1
    );
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .unwrap();
    assert!(query.as_any().is::<NativeQuery>());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_recipes_are_derived_from_graph_owned_configuration() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let properties = HashMap::from([("file".into(), serde_json::json!("fixture.jsonl"))]);
    let core = builder()
        .with_source_fixture(source)
        .with_bootstrap_for_source_by_name(
            crate::ComponentName::try_new("source").expect("fixture component name"),
            "scriptfile",
            properties.clone(),
        )
        .with_identity_provider(Arc::new(crate::identity::PasswordIdentityProvider::new(
            "user", "password",
        )))
        .build()
        .await
        .unwrap();
    let source_name = crate::ComponentName::try_new("source").expect("fixture component name");
    core.component_by_name(&source_name)
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let mut detached = core.get_graph().await.expect("graph snapshot");
    detached.nodes.retain(|node| {
        node.id != GraphEntityId::InstanceIdentityProvider
            && !matches!(node.id, GraphEntityId::BootstrapProvider(_))
    });
    let snapshot = core.snapshot_configuration().await.unwrap();
    let source = snapshot
        .sources
        .iter()
        .find(|source| source.name == source_name)
        .unwrap();
    let source_id = source.id.clone();
    let recipe = source.bootstrap_provider.as_ref().unwrap();
    assert_eq!(recipe.kind, "scriptfile");
    assert_eq!(recipe.properties, properties);
    assert!(snapshot.edges.iter().any(|edge| {
        edge.from == GraphEntityId::BootstrapProvider(source_id.clone())
            && edge.to == GraphEntityId::Component(source_id.clone())
    }));
    assert!(snapshot.edges.iter().any(|edge| {
        edge.from == GraphEntityId::InstanceIdentityProvider
            && edge.to == GraphEntityId::Component(source_id.clone())
    }));
    assert!(core
        .get_graph()
        .await
        .expect("graph snapshot")
        .nodes
        .iter()
        .any(|node| node.id == GraphEntityId::BootstrapProvider(source_id.clone())));
    core.remove_source_by_name(&source_name, false)
        .await
        .unwrap();
    let (replacement, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    core.add_source_fixture_with_handle(replacement)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(snapshot
        .sources
        .iter()
        .find(|source| source.name == source_name)
        .unwrap()
        .bootstrap_provider
        .is_none());
    assert!(!snapshot.edges.iter().any(|edge| {
        matches!(edge.from, GraphEntityId::BootstrapProvider(_))
            && edge.to == GraphEntityId::Component(source_id.clone())
    }));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn detached_snapshot_status_and_membership_cannot_change_public_inspection() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let core = builder().with_source_fixture(source).build().await.unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let mut snapshot = core.get_graph().await.expect("graph snapshot");
    snapshot.nodes.push(crate::component_graph::ComponentNode {
        id: GraphEntityId::Component(crate::test_helpers::component_id(999)),
        name: Some(crate::ComponentName::try_new("phantom").expect("fixture component name")),
        kind: crate::component_graph::ComponentKind::Query,
        status: ComponentStatus::Running,
        metadata: HashMap::new(),
    });
    snapshot
        .nodes
        .iter_mut()
        .find(|node| {
            node.name
                .as_ref()
                .is_some_and(|name| name.as_str() == "source")
        })
        .unwrap()
        .status = ComponentStatus::Error;
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Added
    );
    assert_eq!(
        core.get_source_info_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap()
        .status,
        ComponentStatus::Added
    );
    assert!(core.list_queries().await.unwrap().is_empty());
    assert!(core
        .get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("phantom").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core
        .get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("phantom").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core
        .subscribe_query_logs_by_name(
            &drasi_core_types::ComponentName::try_new("phantom").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core
        .snapshot_configuration()
        .await
        .unwrap()
        .queries
        .is_empty());
    assert!(!core
        .get_graph()
        .await
        .expect("graph snapshot")
        .contains_by_name(
            &crate::ComponentName::try_new("phantom").expect("fixture component name"),
        ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_plugin_metadata_fails_the_added_node_not_the_add_call() {
    let core = builder().build().await.unwrap();
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    core.add_source_fixture_with_metadata(
        source,
        HashMap::from([
            ("pluginId".into(), "invalid plugin id".into()),
            ("pluginVersion".into(), "1".into()),
        ]),
    )
    .await
    .unwrap();
    assert!(core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .unwrap()
        .wait_created()
        .await
        .is_err());
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    assert!(core
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .is_err());
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert!(core
        .get_source_info_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap()
        .error_message
        .is_some());
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn detached_snapshot_cannot_veto_a_graph_addition() {
    let core = builder().build().await.unwrap();
    let runtime = core.computation_runtime.as_ref();
    let mut detached = core.get_graph().await.expect("graph snapshot");
    detached.nodes.push(crate::component_graph::ComponentNode {
        id: GraphEntityId::Component(crate::test_helpers::component_id(998)),
        name: Some(crate::ComponentName::try_new("source").expect("fixture component name")),
        kind: crate::component_graph::ComponentKind::Query,
        status: ComponentStatus::Running,
        metadata: HashMap::new(),
    });
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 1);
    assert_eq!(
        core.get_graph()
            .await
            .unwrap()
            .get_component_by_id(&component_id_by_name(&core, "source"))
            .unwrap()
            .kind,
        crate::component_graph::ComponentKind::Source,
    );
    assert!(runtime
        .current_records()
        .await
        .unwrap()
        .contains_key(&component_id_by_name(&core, "source")));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn graph_activation_policy_controls_ordinary_startup_and_configuration_reads() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    source_control.auto_start.store(true, Ordering::Release);
    let mut query = config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    );
    query.auto_start = true;
    let core = builder()
        .with_source_fixture(source)
        .with_query(query)
        .build()
        .await
        .unwrap();
    for id in ["source", "query"] {
        let name = crate::ComponentName::try_new(id).expect("fixture component name");
        core.component_by_name(&name)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    let mut current = core.desired_configuration().unwrap();
    for component in &mut current.desired.ordinary {
        component.auto_start = false;
    }
    core.apply_desired_state(current.revision, "disable-auto-start", current.desired)
        .await
        .unwrap();
    let report = core.reconcile_desired_state().await.unwrap();
    assert!(report.applied, "{report:?}");
    assert!(report.error.is_none(), "{report:?}");
    core.start().await.unwrap();
    assert_eq!(source_control.starts.load(Ordering::Acquire), 0);
    assert!(
        !core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
            )
            .unwrap()
            .observed()
            .unwrap()
            .started
    );
    assert!(
        !core
            .get_query_config_by_name(
                &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
            )
            .await
            .unwrap()
            .auto_start
    );
    assert!(!core.get_current_config().await.unwrap().queries[0].auto_start);
    let snapshot = core.snapshot_configuration().await.unwrap();
    assert!(
        !snapshot
            .sources
            .iter()
            .find(|source| source.name.as_str() == "source")
            .unwrap()
            .auto_start
    );
    assert!(!snapshot.queries[0].config.auto_start);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn replacement_preserves_declared_autostart_without_starting_a_stopped_instance() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let core = builder().with_source_fixture(source).build().await.unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let (replacement, replacement_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    replacement_control
        .auto_start
        .store(true, Ordering::Release);
    core.replace_source_fixture(
        crate::config::ComponentReference::by_name(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        ),
        replacement,
    )
    .await
    .unwrap();
    let source = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        )
        .unwrap();
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
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("later").expect("fixture component name"),
            ),
        ))
        .await
        .unwrap();
    let control = core.computation_control().unwrap();
    let later = crate::ComponentName::try_new("later").unwrap();
    assert_waiting_for(&core, &query, &later).await;
    assert_eq!(
        core.get_query_config_by_id(query.id())
            .await
            .unwrap()
            .inputs[0]
            .component,
        crate::config::ComponentReference::by_name(later.clone()),
    );
    let (unrelated, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name"),
    );
    core.add_source_fixture_with_handle(unrelated)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("later").expect("fixture component name"),
    );
    let source = core.add_source_fixture_with_handle(source).await.unwrap();
    source.wait_created().await.unwrap();
    let status = core.reconcile_desired_state().await.unwrap();
    assert!(status.error.is_none(), "{status:?}");
    let query = core.component_by_id(query.id()).unwrap();
    query.wait_created().await.unwrap();
    let source_id = source.id().clone();
    assert!(control
        .desired_snapshot()
        .subscriptions
        .contains(&(source_id.clone(), query.id().clone())));
    let exported = control
        .desired_snapshot()
        .select(GraphSelection::All)
        .unwrap();
    let imported = DesiredTopology::from_json(&exported.to_json().unwrap()).unwrap();
    assert!(imported
        .subscriptions
        .contains(&(source_id.clone(), query.id().clone())));
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
    assert!(core
        .remove_source_by_name(
            &drasi_core_types::ComponentName::try_new("later").expect("fixture component name"),
            false
        )
        .await
        .is_err());
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
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("later").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}
struct ControlledSource {
    inner: SourceBase,
    control: Arc<SourceControl>,
}
impl Drop for ControlledSource {
    fn drop(&mut self) {
        self.control.drops.fetch_add(1, Ordering::AcqRel);
    }
}
impl ControlledSource {
    fn fixture(name: drasi_core_types::ComponentName) -> (SourceFixture<Self>, Arc<SourceControl>) {
        let control = Arc::new(SourceControl::default());
        let captured = control.clone();
        let lifecycle = control.clone();
        (
            SourceFixture::new(name, false, move |id| {
                let inner =
                    SourceBase::new(SourceBaseParams::new(id.clone()).with_auto_start(false))?;
                let _ = captured.status.set(inner.status_handle());
                Ok(Self {
                    inner,
                    control: captured,
                })
            })
            .with_lifecycle(move || lifecycle.auto_start.load(Ordering::Acquire)),
            control,
        )
    }

    fn new(id: drasi_core_types::ComponentId) -> (Self, Arc<SourceControl>) {
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
    fn id(&self) -> &drasi_core_types::ComponentId {
        &self.inner.id
    }
    fn type_name(&self) -> &str {
        "controlled"
    }
    fn describe_schema(&self) -> Option<crate::schema::SourceSchema> {
        self.control.schema.lock().unwrap().clone()
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let hook = self.control.properties_read.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        HashMap::new()
    }
    fn auto_start(&self) -> bool {
        self.control.auto_start.load(Ordering::Acquire)
    }
    fn supports_replay(&self) -> bool {
        false
    }
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.control.initializations.fetch_add(1, Ordering::AcqRel);
        if let Some(observer) = &context.resource_observer {
            self.control
                .resource_observers
                .lock()
                .unwrap()
                .push(observer.clone());
        }
        let updates = context.update_tx.clone();
        *self.control.updates.lock().unwrap() = Some(updates.clone());
        self.inner.bind_runtime_context(context).await;
        if self.control.fail_initialize.load(Ordering::Acquire) {
            updates
                .send(ComponentUpdate::Status {
                    component_id: self.id().clone(),
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
        if self.control.waiting_start.load(Ordering::Acquire) {
            self.control.entered.notify_one();
            self.control.release.notified().await;
        }
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
        assert!(
            !self
                .control
                .panic_subscription_fence
                .load(Ordering::Acquire),
            "injected startup subscription panic"
        );
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
        if self.control.pending_stop.load(Ordering::Acquire) {
            self.control.stop_entered.notify_one();
            self.control.stop_release.notified().await;
        }
        if self.control.fail_stop.load(Ordering::Acquire) {
            anyhow::bail!("injected stop failure");
        }
        if self.control.noop_stop.load(Ordering::Acquire) {
            return Ok(());
        }
        self.inner.stop_common().await
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
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
        if self.control.pending_subscription.load(Ordering::Acquire) {
            self.control.subscription_entered.notify_one();
            self.control.subscription_release.notified().await;
        }
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
    drops: AtomicUsize,
    status: std::sync::OnceLock<crate::component_graph::ComponentStatusHandle>,
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

impl ReactionControl {
    async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        self.status
            .get()
            .expect("reaction constructed")
            .set_status(status, message)
            .await;
    }
}

struct ControlledReaction {
    base: ReactionBase,
    control: Arc<ReactionControl>,
    output: mpsc::UnboundedSender<QueryResult>,
}
impl Drop for ControlledReaction {
    fn drop(&mut self) {
        self.control.drops.fetch_add(1, Ordering::AcqRel);
    }
}
impl ControlledReaction {
    fn fixture(
        name: drasi_core_types::ComponentName,
        queries: &[crate::ComponentName],
    ) -> (
        ReactionFixture<Self>,
        Arc<ReactionControl>,
        mpsc::UnboundedReceiver<QueryResult>,
    ) {
        let control = Arc::new(ReactionControl::default());
        let captured = control.clone();
        let (output, receiver) = mpsc::unbounded_channel();
        let inputs: Vec<_> = queries
            .iter()
            .cloned()
            .map(crate::config::ComponentReference::by_name)
            .collect();
        (
            ReactionFixture::new(name, inputs, false, move |id, resolved_inputs| {
                let inputs = resolved_inputs
                    .into_iter()
                    .map(crate::config::ComponentReference::by_id)
                    .collect();
                let base = ReactionBase::new(
                    ReactionBaseParams::new(id.clone(), inputs).with_auto_start(false),
                );
                let _ = captured.status.set(base.status_handle());
                Ok(Self {
                    base,
                    control: captured,
                    output,
                })
            }),
            control,
            receiver,
        )
    }

    fn new(
        id: drasi_core_types::ComponentId,
        queries: &[crate::ComponentName],
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
                    ReactionBaseParams::new(
                        id,
                        queries
                            .iter()
                            .cloned()
                            .map(crate::config::ComponentReference::by_name)
                            .collect(),
                    )
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
    fn id(&self) -> &drasi_core_types::ComponentId {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "controlled"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
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
    async fn bind_runtime_context(&self, context: crate::ReactionRuntimeContext) {
        self.control.initializations.fetch_add(1, Ordering::AcqRel);
        if let Some(observer) = &context.resource_observer {
            self.control
                .resource_observers
                .lock()
                .unwrap()
                .push(observer.clone());
        }
        *self.control.updates.lock().unwrap() = Some(context.update_tx.clone());
        self.base.bind_runtime_context(context).await;
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
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
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
    DrasiLib::builder().with_query_factory(
        crate::computation::v1::FactoryRegistry::standard()
            .get(
                &crate::computation::v1::ImplementationIdentity::try_new(
                    "drasi/continuous-query",
                    "1",
                )
                .expect("reference query identity"),
            )
            .expect("reference query factory")
            .clone(),
    )
}
fn config(
    name: drasi_core_types::ComponentName,
    source: Option<crate::ComponentName>,
) -> crate::config::QueryConfig {
    let query = crate::Query::cypher(name)
        .query("MATCH (n:Person) RETURN n.name AS name")
        .auto_start(false)
        .enable_bootstrap(false);
    match source {
        Some(source) => query.from_input_by_name(source).build().unwrap(),
        None => query.build().unwrap(),
    }
}
async fn insert(core: &DrasiLib, source: &drasi_core_types::ComponentId) {
    insert_person(core, source, "one", "Alice").await;
}
async fn insert_person(
    core: &DrasiLib,
    source: &drasi_core_types::ComponentId,
    id: &str,
    name: &str,
) {
    let instance = core.source_instance(source).await.unwrap();
    let base = &instance
        .as_any()
        .downcast_ref::<ControlledSource>()
        .unwrap()
        .inner;
    base.dispatch_event(crate::channels::SourceEventWrapper::new(
        source.clone(),
        crate::channels::SourceEvent::Change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source.as_str(), id),
                    labels: vec!["Person".into()].into(),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"name":name})),
            },
        }),
        chrono::Utc::now(),
        base.next_sequence(),
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

async fn component_handle(
    core: &DrasiLib,
    name: &crate::ComponentName,
    kind: &'static str,
) -> ComponentHandle {
    let handle = core.component_by_name(name).unwrap();
    let runtime = core.computation_runtime.as_ref();
    let control = runtime.control().unwrap();
    runtime
        .component_observation_at(&control.registry_snapshot(), handle.id(), kind)
        .unwrap();
    handle
}

fn component_id_by_name(core: &DrasiLib, name: &str) -> ComponentId {
    core.component_by_name(&ComponentName::try_new(name).unwrap())
        .unwrap()
        .id()
        .clone()
}

async fn apply_native_topology(
    core: &DrasiLib,
    edit: impl FnOnce(&mut DesiredTopology),
) -> crate::Result<crate::management::ManagementStatus> {
    let mut current = core.desired_configuration()?;
    edit(&mut current.desired.topology);
    core.apply_desired_state(
        current.revision,
        format!("native-test-{}", current.revision),
        current.desired,
    )
    .await?;
    core.reconcile_desired_state().await
}

fn remove_native_definition(topology: &mut DesiredTopology, id: &ComponentId) {
    topology
        .components
        .retain(|node| node.descriptor.id() != id);
    topology.relationships.retain(|edge| {
        &edge.definition.from.component != id && &edge.definition.to.component != id
    });
    topology.boundary_relationships.retain(|edge| {
        &edge.definition.from.component != id && &edge.definition.to.component != id
    });
    topology
        .subscriptions
        .retain(|(from, to)| from != id && to != id);
    topology
        .control_connections
        .retain(|(from, to)| from != id && to != id);
    topology.readiness_required.remove(id);
    topology.component_plugins.remove(id);
    topology.component_resources.remove(id);
}

async fn assert_creation_failed(_core: &DrasiLib, handle: &ComponentHandle, message: &str) {
    tokio::time::timeout(Duration::from_secs(3), handle.wait_created())
        .await
        .expect("creation must complete with a failure")
        .expect_err("failed initialization must not be reported as ready");
    let observed = handle.observed().unwrap();
    assert_eq!(observed.realization, RealizationState::CreationFailed);
    let failure = format!("{:#}", observed.failure.as_ref().unwrap().cause);
    assert!(failure.contains(message), "{failure}");
}

async fn assert_waiting_for(
    core: &DrasiLib,
    handle: &ComponentHandle,
    dependency: &crate::ComponentName,
) {
    let mut changes = core.computation_control().unwrap().subscribe_registry();
    tokio::time::timeout(
        Duration::from_secs(3),
        changes.wait_for(|snapshot| {
            snapshot.observed.components.get(handle.id()).is_some_and(|node| {
                node.generation == handle.generation()
                    && (node.failure.is_some()
                        || node.waiting_for.iter().any(|waiting| {
                            matches!(waiting, DependencyWait::ComponentName { name } if name == dependency)
                        }))
            })
        }),
    )
    .await
    .expect("dependency observation must be published")
    .unwrap();
    let observed = handle.observed().unwrap();
    assert!(matches!(
        observed.realization,
        RealizationState::Pending | RealizationState::Blocked
    ));
    assert!(observed.failure.is_none(), "{observed:?}");
    assert!(
        observed.waiting_for.iter().any(|waiting| {
            matches!(waiting, DependencyWait::ComponentName { name } if name == dependency)
        }),
        "{observed:?}"
    );
}

async fn wait_for_attached_resources(
    core: &DrasiLib,
    component: &ComponentId,
    roles: &[ResourceRole],
) -> Arc<ComputationInspection> {
    let mut changes = core
        .computation_runtime
        .as_ref()
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
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let provider = selected.clone();
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core
        .create_source_async(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            None,
            false,
            move |id| async move {
                let mut source = source.construct(id.clone())?;
                source.inner = SourceBase::new(
                    SourceBaseParams::new(id.clone())
                        .with_auto_start(false)
                        .with_state_store(provider),
                )?;
                Ok((source, HashMap::new()))
            },
        )
        .await
        .unwrap();
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
    let (replacement, replacement_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    core.replace_source_fixture(
        crate::config::ComponentReference::by_id(handle.id().clone()),
        replacement,
    )
    .await
    .unwrap();
    let current = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        "source",
    )
    .await;
    current.wait_created().await.unwrap();
    assert_eq!(
        replacement_control.resource_observers.lock().unwrap().len(),
        1
    );
    assert_ne!(handle.id(), current.id());
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
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let provider = shared.clone();
    let source = core
        .create_source_async(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            None,
            false,
            move |id| async move {
                let source = source.construct(id)?;
                source.inner.set_identity_provider(provider).await;
                Ok((source, HashMap::new()))
            },
        )
        .await
        .unwrap();
    source.wait_created().await.unwrap();
    core.add_query_with_handle(config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    ))
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let provider = shared.clone();
    let reaction = core
        .create_reaction_async(
            drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
            None,
            vec![crate::config::ComponentReference::by_name(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )],
            false,
            move |id, inputs| async move {
                let reaction = reaction.construct(id, inputs)?;
                reaction.base.set_identity_provider(provider).await;
                Ok((reaction, HashMap::new()))
            },
        )
        .await
        .unwrap();
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
    let (replacement, replacement_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let provider = shared.clone();
    core.remove_reaction_by_id(reaction.id(), false)
        .await
        .unwrap();
    let replacement = core
        .create_reaction_async(
            drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
            None,
            vec![crate::config::ComponentReference::by_name(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )],
            false,
            move |id, inputs| async move {
                let replacement = replacement.construct(id, inputs)?;
                replacement.base.set_identity_provider(provider).await;
                Ok((replacement, HashMap::new()))
            },
        )
        .await
        .unwrap();
    replacement.wait_created().await.unwrap();
    assert_ne!(reaction.id(), replacement.id());
    assert_eq!(
        replacement_control.resource_observers.lock().unwrap().len(),
        1
    );
    let current = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        "reaction",
    )
    .await;
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
    let mut invalid = config(
        drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
        None,
    );
    invalid.query = "this is not a query".into();
    core.add_query(invalid.clone()).await.unwrap();
    let handle = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
        "query",
    )
    .await;
    assert_creation_failed(&core, &handle, "").await;
    let query = core
        .get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
        )
        .await
        .unwrap();
    assert_eq!(query.query, invalid.query);
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    let runtime = core.computation_runtime.as_ref();
    let snapshot = runtime.inspector().unwrap().snapshot();
    assert_eq!(
        snapshot.desired.specifications[handle.id()]
            .configuration
            .get("query_config"),
        Some(&ConfigurationValue::Literal(
            serde_json::to_value(invalid.with_assigned_id(handle.id().clone())).unwrap()
        )),
    );
    let declared = core.computation_runtime.query(handle.id()).await.unwrap();
    assert_eq!(
        declared.inspector().snapshot().desired.id,
        snapshot.desired.id
    );
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("independent").expect("fixture component name"),
    );
    core.add_source_fixture_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
    )
    .await
    .unwrap();
    assert!(core
        .get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(matches!(
        handle.observed(),
        Err(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn builder_query_validation_failures_remain_on_declared_nodes() {
    use crate::indexes::{StorageBackendRef, StorageBackendSpec};

    let mut syntax = config(
        drasi_core_types::ComponentName::try_new("syntax").expect("fixture component name"),
        None,
    );
    syntax.query = "this is not a query".into();
    let dependency = config(
        drasi_core_types::ComponentName::try_new("dependency").expect("fixture component name"),
        Some(
            drasi_core_types::ComponentName::try_new("missing-source")
                .expect("fixture component name"),
        ),
    );
    let mut unknown_backend = config(
        drasi_core_types::ComponentName::try_new("unknown-backend")
            .expect("fixture component name"),
        None,
    );
    unknown_backend.storage_backend = Some(StorageBackendRef::Named("missing-backend".into()));
    let mut invalid_inline = config(
        drasi_core_types::ComponentName::try_new("invalid-inline").expect("fixture component name"),
        None,
    );
    invalid_inline.storage_backend = Some(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
        kind: "".into(),
    }));
    let mut unsupported_inline = config(
        drasi_core_types::ComponentName::try_new("unsupported-inline")
            .expect("fixture component name"),
        None,
    );
    unsupported_inline.storage_backend =
        Some(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
            kind: "rocksdb".into(),
        }));
    let mut capacity = config(
        drasi_core_types::ComponentName::try_new("capacity").expect("fixture component name"),
        None,
    );
    capacity.dispatch_buffer_capacity = Some(0);
    let invalid = [
        (syntax, ""),
        (unknown_backend, "missing-backend"),
        (invalid_inline, "must not be empty"),
        (unsupported_inline, ""),
        (capacity, "bounded pipe capacity"),
    ];
    let mut configured = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("valid").expect("fixture component name"),
            None,
        ))
        .with_query(dependency);
    for (query, _) in &invalid {
        configured = configured.with_query(query.clone());
    }
    let core = configured
        .build()
        .await
        .expect("query-specific validation must happen after graph addition");
    let dependency = core
        .component_by_name(&ComponentName::try_new("dependency").unwrap())
        .unwrap();
    assert_waiting_for(
        &core,
        &dependency,
        &ComponentName::try_new("missing-source").unwrap(),
    )
    .await;
    for (query, message) in invalid {
        let handle = core.component_by_name(&query.name).unwrap();
        let query = query.with_assigned_id(handle.id().clone());
        assert_creation_failed(&core, &handle, message).await;
        assert_eq!(
            serde_json::to_value(core.get_query_config_by_name(&query.name).await.unwrap())
                .unwrap(),
            serde_json::to_value(
                core.config
                    .queries
                    .iter()
                    .find(|configured| configured.name == query.name)
                    .unwrap()
                    .clone()
                    .with_assigned_id(handle.id().clone())
            )
            .unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(3), core.remove_query_by_id(&query.id))
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "removing {} stalled: {error}; node={:?}",
                    query.name,
                    handle.observed()
                )
            })
            .unwrap();
    }
    let valid = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("valid").expect("fixture component name"),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        valid.wait_created().await.unwrap();
        valid.start().await.unwrap();
        valid.wait_started().await.unwrap();
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "valid query stalled: {error}; graph={:?}",
            core.computation_control().unwrap().observed()
        )
    });
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_constructor_validation_happens_after_declaration() {
    let core = builder().build().await.unwrap();
    let mut invalid = config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    );
    invalid.dispatch_buffer_capacity = Some(0);
    let handle = core.add_query_with_handle(invalid).await.unwrap();
    assert_creation_failed(&core, &handle, "").await;
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    let corrected = config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    );
    core.remove_query_by_id(handle.id()).await.unwrap();
    let replacement = core.add_query_with_handle(corrected).await.unwrap();
    replacement.wait_created().await.unwrap();
    assert_ne!(handle.id(), replacement.id());
    assert!(matches!(
        handle.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_dependencies_wait_and_are_created_when_available() {
    let core = builder().build().await.unwrap();
    core.add_query(config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    ))
    .await
    .unwrap();
    let query = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        "query",
    )
    .await;
    assert_waiting_for(
        &core,
        &query,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await;
    assert!(!core
        .get_graph()
        .await
        .expect("graph snapshot")
        .contains_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        ));

    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("missing-query")
            .expect("fixture component name")],
    );
    let reaction = core
        .add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap();
    assert_waiting_for(
        &core,
        &reaction,
        &drasi_core_types::ComponentName::try_new("missing-query").expect("fixture component name"),
    )
    .await;
    assert!(!core
        .get_graph()
        .await
        .expect("graph snapshot")
        .contains_by_name(
            &drasi_core_types::ComponentName::try_new("missing-query")
                .expect("fixture component name"),
        ));

    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    core.add_source_fixture_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let status = core.reconcile_desired_state().await.unwrap();
    assert!(status.error.is_none(), "{status:?}");
    let query = core.component_by_id(query.id()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), query.wait_created())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        query.observed().unwrap().lifecycle,
        ComponentLifecycle::Stopped
    );
    core.add_query_with_handle(config(
        drasi_core_types::ComponentName::try_new("missing-query").expect("fixture component name"),
        None,
    ))
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("missing-query").expect("fixture component name"),
    )
    .await
    .unwrap();
    let status = core.reconcile_desired_state().await.unwrap();
    assert!(status.error.is_none(), "{status:?}");
    let reaction = core.component_by_id(reaction.id()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), reaction.wait_created())
        .await
        .unwrap()
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_initialization_requires_a_new_object_and_never_retries_in_a_loop() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.fail_initialize.store(true, Ordering::Release);
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    assert_creation_failed(&core, &handle, "injected initialization failure").await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    control.fail_initialize.store(false, Ordering::Release);
    let error = core
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("injected initialization failure"));
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    assert_eq!(control.resource_observers.lock().unwrap().len(), 1);
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    core.remove_source_by_id(handle.id(), false).await.unwrap();
    let (fresh, fresh_control) =
        ControlledSource::fixture(ComponentName::try_new("source").unwrap());
    let fresh = core.add_source_fixture_with_handle(fresh).await.unwrap();
    fresh.wait_created().await.unwrap();
    fresh.start().await.unwrap();
    fresh.wait_started().await.unwrap();
    assert_ne!(handle.id(), fresh.id());
    assert_eq!(fresh_control.initializations.load(Ordering::Acquire), 1);
    assert!(matches!(
        handle.wait_started().await,
        Err(GraphError::StaleGeneration)
    ));
    let source = core
        .computation_runtime
        .as_ref()
        .source(&component_id_by_name(&core, "source"))
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &source
            .as_any()
            .downcast_ref::<ControlledSource>()
            .unwrap()
            .control,
        &fresh_control
    ));
    let mut events = core.subscribe_all_component_events();
    fresh_control
        .set_status(
            ComponentStatus::Error,
            Some("failure on the new object".into()),
        )
        .await;
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("source").expect("fixture component name"),
        ComponentStatus::Error,
        Duration::from_secs(2),
    )
    .await;
    core.shutdown().await.unwrap();
}

async fn explicit_start_joins_pending_source_readiness() {
    let core = builder().build().await.unwrap();
    core.start().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let reporter = control.clone();
    control.auto_start.store(true, Ordering::Release);
    control.starting_only.store(true, Ordering::Release);
    let mut events = core.subscribe_all_component_events();
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("source").expect("fixture component name"),
        ComponentStatus::Starting,
        Duration::from_secs(2),
    )
    .await;
    let graph = core.computation_runtime.as_ref().control().unwrap();
    let id = handle.id().clone();
    let mut observed = graph.subscribe_observed();
    drop(
        tokio::time::timeout(
            Duration::from_secs(2),
            observed.wait_for(|state| {
                state
                    .startup
                    .as_ref()
                    .is_some_and(|report| report.components.contains_key(&id))
            }),
        )
        .await
        .expect("automatic activation report")
        .expect("graph observation remains open"),
    );
    let generation = graph.observed().components[&id].generation;
    let report = tokio::time::timeout(
        Duration::from_secs(2),
        graph.start_members([(id, generation)].into_iter().collect()),
    )
    .await
    .expect("joining an activation must not wait for readiness")
    .expect("an existing activation is not a blocked dependency");
    assert_eq!(report.summary, OperationSummary::Completed);
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Starting
    );
    reporter.set_status(ComponentStatus::Running, None).await;
    handle.wait_started().await.unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_start_joins_pending_source_readiness_current_thread() {
    explicit_start_joins_pending_source_readiness().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_start_joins_pending_source_readiness_multi_thread() {
    explicit_start_joins_pending_source_readiness().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_initialization_does_not_block_addition_or_removal() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("pending").expect("fixture component name"),
    );
    control.pending_initialize.store(true, Ordering::Release);
    let handle = tokio::time::timeout(
        Duration::from_secs(3),
        core.add_source_fixture_with_handle(source),
    )
    .await
    .expect("addition must not wait for initialization")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    let invalid = crate::Query::cypher(
        drasi_core_types::ComponentName::try_new("invalid-dependency")
            .expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("pending").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("missing").expect("fixture component name"),
    )
    .auto_start(false)
    .build()
    .unwrap();
    let invalid = tokio::time::timeout(Duration::from_secs(3), core.add_query_with_handle(invalid))
        .await
        .expect("a pending initializer must not block accepting another definition")
        .unwrap();
    assert_waiting_for(
        &core,
        &invalid,
        &drasi_core_types::ComponentName::try_new("missing").expect("fixture component name"),
    )
    .await;
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("invalid-dependency")
            .expect("fixture component name"),
    )
    .await
    .unwrap();
    let (independent, independent_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("independent").expect("fixture component name"),
    );
    let independent = tokio::time::timeout(
        Duration::from_secs(3),
        core.add_source_fixture_with_handle(independent),
    )
    .await
    .expect("one initializer must not hold a global addition lock")
    .unwrap();
    independent.wait_created().await.unwrap();
    assert_eq!(
        independent_control.initializations.load(Ordering::Acquire),
        1
    );
    tokio::time::timeout(
        Duration::from_secs(3),
        core.remove_source_by_name(
            &drasi_core_types::ComponentName::try_new("pending").expect("fixture component name"),
            true,
        ),
    )
    .await
    .expect("removal must cancel the pending initializer")
    .unwrap();
    assert!(matches!(
        handle.wait_created().await,
        Err(GraphError::StaleGeneration)
    ));
    assert_eq!(control.stops.load(Ordering::Acquire), 0);
    assert_eq!(control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_wait_does_not_cancel_controller_owned_creation() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
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
async fn instance_stop_cancels_pending_auto_start_and_a_new_object_can_restart() {
    let core = builder().build().await.unwrap();
    core.start().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.auto_start.store(true, Ordering::Release);
    control.pending_initialize.store(true, Ordering::Release);
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), core.stop())
        .await
        .expect("instance stop must cancel a pending initializer")
        .unwrap();
    assert!(handle.wait_created().await.is_err());
    assert!(!handle.observed().unwrap().started);
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    core.remove_source_by_id(handle.id(), false).await.unwrap();
    let (fresh, fresh_control) =
        ControlledSource::fixture(ComponentName::try_new("source").unwrap());
    fresh_control.auto_start.store(true, Ordering::Release);
    let fresh = core.add_source_fixture_with_handle(fresh).await.unwrap();
    fresh.wait_created().await.unwrap();
    core.start().await.unwrap();
    fresh.wait_started().await.unwrap();
    assert_ne!(fresh.id(), handle.id());
    assert_eq!(fresh_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    assert!(matches!(
        handle.wait_started().await,
        Err(GraphError::StaleGeneration)
    ));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_cancels_pending_creation_and_fences_existing_waiters() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.auto_start.store(true, Ordering::Release);
    control.pending_initialize.store(true, Ordering::Release);
    let old = core.add_source_fixture_with_handle(source).await.unwrap();
    let old_owned = core
        .computation_control()
        .unwrap()
        .management_control()
        .component_handle_by_id(old.id())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), control.entered.notified())
        .await
        .unwrap();
    let wait = tokio::spawn({
        let old = old.clone();
        async move { old.wait_created().await }
    });
    let (replacement, replacement_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    replacement_control
        .auto_start
        .store(true, Ordering::Release);
    tokio::time::timeout(
        Duration::from_secs(3),
        core.remove_source_by_id(old.id(), false),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "retirement must cancel the obsolete initializer: {error}; node={:?}; accepted={:?}",
            old.observed(),
            core.desired_configuration()
        )
    })
    .unwrap();
    let replacement = core
        .add_source_fixture_with_handle(replacement)
        .await
        .unwrap();
    let wait_error = wait.await.unwrap().unwrap_err();
    assert!(matches!(
        wait_error.underlying(),
        GraphError::Cancelled | GraphError::CreationStopped { .. } | GraphError::StaleGeneration
    ));
    assert!(matches!(
        old.start().await,
        Err(GraphError::StaleGeneration)
    ));
    assert!(matches!(
        old_owned
            .set_control_handler(Arc::new(PeerControl::default()))
            .await,
        Err(GraphError::StaleGeneration)
    ));
    let current = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        "source",
    )
    .await;
    assert_eq!(current.id(), replacement.id());
    assert_ne!(old.id(), current.id());
    current.wait_created().await.unwrap();
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 0);
    current.start().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    assert_eq!(replacement_control.starts.load(Ordering::Acquire), 1);
    let retired_control = current.control().unwrap();
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let readded = core.add_source_fixture_with_handle(source).await.unwrap();
    readded.wait_created().await.unwrap();
    assert_ne!(current.id(), readded.id());
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
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref();
    let revision = runtime.inspector().unwrap().snapshot().desired.revision;
    assert!(core
        .add_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None
        ))
        .await
        .is_err());
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    );
    assert!(core.add_source_fixture_with_handle(source).await.is_err());
    assert_eq!(control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(
        runtime.inspector().unwrap().snapshot().desired.revision,
        revision
    );
    query.wait_created().await.unwrap();
    assert_eq!(
        component_handle(
            &core,
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            "query"
        )
        .await
        .generation(),
        query.generation()
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_ordinary_object_is_cleaned_without_granting_readmission_authority() {
    let core = builder().build().await.unwrap();
    let runtime = core.computation_runtime.as_ref();
    let id = ComponentId::generate();
    let (source, source_control) = ControlledSource::new(id.clone());
    let error = core
        .add_source_with_handle(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.downcast_ref::<GraphError>(), Some(GraphError::ExternalComponentId { component }) if component == &id)
    );
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(source_control.drops.load(Ordering::Acquire), 1);
    assert!(runtime.current_records().await.unwrap().is_empty());
    let (retry, retry_control) = ControlledSource::new(id.clone());
    assert!(core
        .add_source(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            retry
        )
        .await
        .is_err());
    assert_eq!(retry_control.drops.load(Ordering::Acquire), 1);
    let (fresh, fresh_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let handle = core.add_source_fixture_with_handle(fresh).await.unwrap();
    assert_ne!(handle.id(), &id);
    handle.wait_created().await.unwrap();
    core.start_source_by_id(handle.id()).await.unwrap();
    handle.wait_started().await.unwrap();
    assert_eq!(fresh_control.initializations.load(Ordering::Acquire), 1);
    core.remove_source_by_id(handle.id(), true).await.unwrap();
    assert_eq!(fresh_control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_query_and_reaction_errors_retain_graph_owned_cleanup() {
    // This case deliberately supplies an existing object to exercise rejection cleanup.
    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("taken").expect("fixture component name"),
    );
    let existing = core.add_source_fixture_with_handle(source).await.unwrap();
    existing.wait_created().await.unwrap();
    let query = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("taken").expect("fixture component name"),
            None,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(query.downcast_ref::<GraphError>(), Some(GraphError::DuplicateComponentName { existing: conflict }) if &conflict.id == existing.id())
    );
    let id = ComponentId::generate();
    let (reaction, reaction_control, _) = ControlledReaction::new(id.clone(), &[]);
    let error = core
        .add_reaction_with_handle(
            drasi_core_types::ComponentName::try_new("taken").expect("fixture component name"),
            reaction,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.downcast_ref::<GraphError>(), Some(GraphError::DuplicateComponentName { existing: conflict }) if &conflict.id == existing.id())
    );
    assert_eq!(reaction_control.initializations.load(Ordering::Acquire), 0);
    assert_eq!(reaction_control.drops.load(Ordering::Acquire), 1);
    assert!(core.list_queries().await.unwrap().is_empty());
    assert!(core.list_reactions().await.unwrap().is_empty());
    core.shutdown().await.unwrap();
    assert_eq!(reaction_control.drops.load(Ordering::Acquire), 1);
}

mockall::mock! {
    InventoryWal {}

    #[async_trait]
    impl crate::wal::WalProvider for InventoryWal {
        async fn register(&self, scope: &crate::StorageScope, source_id: &ComponentId, config: crate::wal::WriteAheadLogConfig) -> std::result::Result<(), crate::wal::WalError>;
        async fn append(&self, scope: &crate::StorageScope, source_id: &ComponentId, event: &drasi_core::models::SourceChange) -> std::result::Result<u64, crate::wal::WalError>;
        async fn read_from(&self, scope: &crate::StorageScope, source_id: &ComponentId, sequence: u64) -> std::result::Result<Vec<(u64, drasi_core::models::SourceChange)>, crate::wal::WalError>;
        async fn prune_up_to(&self, scope: &crate::StorageScope, source_id: &ComponentId, sequence: u64) -> std::result::Result<u64, crate::wal::WalError>;
        async fn head_sequence(&self, scope: &crate::StorageScope, source_id: &ComponentId) -> std::result::Result<u64, crate::wal::WalError>;
        async fn oldest_sequence(&self, scope: &crate::StorageScope, source_id: &ComponentId) -> std::result::Result<Option<u64>, crate::wal::WalError>;
        async fn event_count(&self, scope: &crate::StorageScope, source_id: &ComponentId) -> std::result::Result<u64, crate::wal::WalError>;
        async fn delete_wal(&self, scope: &crate::StorageScope, source_id: &ComponentId) -> std::result::Result<(), crate::wal::WalError>;
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
    let runtime = core.computation_runtime.as_ref();
    let scoped = core.computation_plugin_services().unwrap();
    assert!(
        scoped.wal.is_some(),
        "native components must receive the instance WAL"
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

    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let source = core.add_source_fixture_with_handle(source).await.unwrap();
    let query = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .await
        .unwrap();
    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let reaction = core
        .add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap();
    for (id, kind, handle) in [
        ("source", "source", source),
        ("query", "query", query),
        ("reaction", "reaction", reaction),
    ] {
        assert_eq!(handle.id(), &component_id_by_name(&core, id));
        handle.wait_created().await.unwrap();
        if kind == "query" {
            let publication = runtime.control().unwrap().registry_snapshot();
            assert!(runtime
                .ordinary_queries_at(&publication)
                .unwrap()
                .contains_key(handle.id()));
            assert_eq!(
                publication.desired.specifications[handle.id()].role,
                ComponentRole::Transformer
            );
            for (name, _) in services {
                let id = ResourceId::try_new(format!("instance.{name}")).unwrap();
                assert_eq!(
                    publication.observed.resources[&id].generation,
                    initial.observed.resources[&id].generation
                );
            }
            continue;
        }
        let record = runtime.record(handle.id(), kind).await.unwrap();
        assert_eq!(record.node, *handle.id());
        let accepted = core.desired_configuration().unwrap();
        let definition = accepted
            .desired
            .ordinary
            .iter()
            .find(|definition| &definition.id == handle.id())
            .unwrap();
        let crate::management::OrdinaryComponent::ProcessLocal { binding, .. } =
            &definition.component
        else {
            panic!("ordinary fixture must retain its actual resource binding");
        };
        assert_eq!(&record.resource, binding);
        let snapshot = runtime.inspector().unwrap().snapshot();
        let spec = &snapshot.desired.specifications[handle.id()];
        assert_eq!(
            spec.dependencies.get("instance"),
            Some(&vec![record.resource.clone()])
        );
        assert_eq!(
            spec.dependencies.len(),
            services.len() + 1 + usize::from(kind == "source")
        );
        if let Value::Source(source) = &record.value {
            let host =
                snapshot.desired.specifications[handle.id()].dependencies["source_host"][0].clone();
            let published = runtime
                .control()
                .unwrap()
                .registry_snapshot()
                .resource(&host)
                .unwrap()
                .get::<SourcePluginHost>()
                .unwrap();
            assert!(Arc::ptr_eq(&published, &source.borrowed));
        }
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
        .set(
            &drasi_core_types::StorePartition::component(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000369")
                    .expect("fixture UUID"),
            ),
            "retained",
            b"shared-state".to_vec(),
        )
        .await
        .unwrap();
    core.remove_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
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
        state
            .get(
                &drasi_core_types::StorePartition::component(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000369")
                        .expect("fixture UUID")
                ),
                "retained"
            )
            .await
            .unwrap(),
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
        .with_allow_external_component_ids(true)
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
            .filter(|resource| resource.role == ResourceRole::IndexBackend
                && resource.id.as_str().starts_with("instance.index/"))
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
        descriptor: ComponentDescriptor::try_new(crate::test_helpers::component_id(779), vec![])
            .unwrap(),
        role: ComponentRole::Service,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::from([(Arc::from("indexes"), resources)]),
    };
    core.add_computation_component(
        ComponentAddition::from_specification(
            ComponentName::try_new("inventory-probe").unwrap(),
            spec,
            factory,
        )
        .auto_start(false),
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
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .build()
        .await
        .unwrap();
    let query = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap();
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
            .filter(|resource| resource.role == ResourceRole::IndexBackend
                && resource.id.as_str().starts_with("instance.index/"))
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
        publication.desired.specifications[query.id()].dependencies["backend"],
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
    let mut replacement = config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    );
    replacement.storage_backend =
        Some(crate::indexes::StorageBackendRef::Named("secondary".into()));
    let error = core
        .update_query_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            replacement.clone(),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("query factory does not support configuration updates"));
    assert_eq!(
        control.desired_snapshot().specifications[query.id()].dependencies["backend"],
        vec![primary_id.clone()]
    );
    core.computation_runtime
        .remove_owned(query.id(), "query", false)
        .await
        .unwrap();
    let replacement = core.add_query_with_handle(replacement).await.unwrap();
    replacement.wait_created().await.unwrap();
    assert_ne!(replacement.id(), query.id());
    let desired = control.desired_snapshot();
    assert_eq!(
        desired.specifications[replacement.id()].dependencies["backend"],
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
    let independent_query_id = crate::test_helpers::component_id(46);
    let extra_id = "lib-query/7175657279";
    let extra = ComponentBatch::builder()
        .service(
            ComponentName::try_new("query").unwrap(),
            Box::new(NativeService {
                descriptor: ComponentDescriptor::try_new(independent_query_id.clone(), vec![])
                    .unwrap(),
                starts: Arc::new(AtomicUsize::new(0)),
                stops: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .build()
        .unwrap();
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let plugin = PluginIdentity {
        id: "known-source-plugin".into(),
        version: "1.2.3".into(),
    };
    let independent_core = builder()
        .with_allow_external_component_ids(true)
        .with_id(extra_id)
        .with_components(extra.auto_start(false))
        .build()
        .await
        .unwrap();
    independent_core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let core = builder()
        .with_source_fixture_metadata(
            source,
            HashMap::from([
                ("pluginId".into(), plugin.id.to_string()),
                ("pluginVersion".into(), plugin.version.to_string()),
            ]),
        )
        .with_default_index_provider("primary", Arc::new(DeferredPersistentProvider))
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_query(config(
            drasi_core_types::ComponentName::try_new("unresolved").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("missing")
                    .expect("fixture component name"),
            ),
        ))
        .build()
        .await
        .unwrap();
    let handle = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap();
    handle.wait_created().await.unwrap();
    let query_id = handle.id().clone();
    assert_waiting_for(
        &core,
        &core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("unresolved")
                    .expect("fixture component name"),
            )
            .unwrap(),
        &drasi_core_types::ComponentName::try_new("missing").expect("fixture component name"),
    )
    .await;
    let inventory = core.inspect_computation_inventory().await.unwrap();
    let root = ComputationScope::root("__drasi_lib_runtime__");
    let independent = independent_core
        .inspect_computation_inventory()
        .await
        .unwrap();
    assert_ne!(inventory.instance_id, independent.instance_id);
    assert_eq!(inventory.scopes.len(), 1);
    assert_eq!(independent.scopes.len(), 1);
    assert_eq!(
        inventory.scopes[&root].topology.graph_id.as_ref(),
        "__drasi_lib_runtime__"
    );
    assert!(independent.scopes[&root].owner.is_none());
    assert!(independent.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(independent_query_id.clone())));
    assert!(inventory.scopes[&root].owner.is_none());
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
        )
        .unwrap()
        .generation(),
        handle.generation()
    );
    assert!(inventory.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(query_id.clone())));
    assert!(inventory
        .entities()
        .any(|(id, entity)| id.scope == root && matches!(entity, GraphEntity::Pipe(_))));
    assert!(
        inventory
            .entities()
            .any(|(id, entity)| id.scope == root
                && matches!(entity, GraphEntity::SubscriptionPipe(_)))
    );
    assert!(inventory.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(component_id_by_name(
            &core,
            "unresolved"
        ))));
    assert_eq!(
        inventory.plugin_dependents(&plugin),
        BTreeSet::from([root.entity(GraphEntityId::Component(component_id_by_name(
            &core, "source"
        )))])
    );
    let provider = root.entity(GraphEntityId::Resource(
        index_resource_id("primary").unwrap(),
    ));
    let nested_provider = root.entity(GraphEntityId::Resource(
        ComputationPipelineBuilder::index_resource_id(&query_id).unwrap(),
    ));
    assert!(inventory
        .dependencies(&nested_provider)
        .any(|link| link.to == provider));
    let nested_source = root.entity(GraphEntityId::Resource(
        ComputationPipelineBuilder::source_resource_id(&component_id_by_name(&core, "source"))
            .unwrap(),
    ));
    let source = root.entity(GraphEntityId::Component(component_id_by_name(
        &core, "source",
    )));
    assert!(inventory
        .dependencies(&nested_source)
        .any(|link| link.to == source));
    let state = ResourceId::try_new("instance.state").unwrap();
    assert!(inventory.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Resource(state)));
    assert!(inventory.links.iter().all(|link| link.from != link.to));
    // This inventory fixture retains processing state when withdrawing its definition.
    core.computation_runtime
        .remove_owned(&query_id, "query", false)
        .await
        .unwrap();
    let after = core.inspect_computation_inventory().await.unwrap();
    assert_eq!(after.scopes.len(), 1);
    assert!(!after.scopes[&root]
        .topology
        .nodes
        .contains_key(&GraphEntityId::Component(query_id.clone())));
    assert!(independent_core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
        )
        .unwrap()
        .observed()
        .is_ok());
    assert!(!after
        .links
        .iter()
        .any(|link| link.from == nested_provider || link.to == nested_provider));
    assert!(
        inventory.scopes[&root]
            .topology
            .nodes
            .contains_key(&GraphEntityId::Component(query_id)),
        "earlier snapshots remain immutable"
    );
    core.shutdown().await.unwrap();
    independent_core.shutdown().await.unwrap();
}

#[tokio::test]
async fn instance_root_identity_is_rejected_by_native_admission_with_cleanup_ownership() {
    let id = ComponentId::generate();
    // Explicit object injection isolates the independent instance-identity reservation.
    let core = builder()
        .with_id(id.to_string())
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let (source, source_control) = ControlledSource::new(id.clone());
    let error = core
        .add_source_with_handle(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("instance identity is reserved"),
        "{error:#}"
    );
    assert_eq!(source_control.initializations.load(Ordering::Acquire), 0);
    assert!(core.get_source_status_by_id(&id).await.is_err());
    assert!(core
        .computation_control()
        .unwrap()
        .inspector()
        .snapshot()
        .observed
        .components
        .values()
        .all(|component| component.failure.is_none()));
    core.shutdown().await.unwrap();
    assert_eq!(source_control.drops.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn runtime_factory_validates_captured_services_without_requiring_absent_providers() {
    let core = builder().build().await.unwrap();
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    handle.wait_created().await.unwrap();
    let runtime = core.computation_runtime.as_ref();
    let record = runtime
        .record(&component_id_by_name(&core, "source"), "source")
        .await
        .unwrap();
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
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.auto_start.store(true, Ordering::Release);
    let source = core.add_source_fixture_with_handle(source).await.unwrap();
    source.wait_created().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 0);
    let runtime = core.computation_runtime.as_ref();
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Added
    );
    let snapshot = runtime.inspector().unwrap().snapshot();
    assert!(snapshot.desired.lifecycle_policies[source.id()].auto_start);
    let Some(ConfigurationValue::Literal(definition)) = snapshot.desired.specifications
        [source.id()]
    .configuration
    .get("definition") else {
        panic!("source definition must remain literal configuration");
    };
    assert_eq!(definition["auto_start"], true);
    core.start().await.unwrap();
    source.wait_started().await.unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 1);
    let (bad, bad_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("bad").expect("fixture component name"),
    );
    bad_control.auto_start.store(true, Ordering::Release);
    bad_control.fail_starts.store(1, Ordering::Release);
    core.add_source_fixture(bad).await.unwrap();
    let bad = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("bad").expect("fixture component name"),
        "source",
    )
    .await;
    bad.wait_created().await.unwrap();
    assert!(bad.wait_started().await.is_err());
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("bad").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_start_hooks_do_not_complete_handle_waits_before_plugin_running() {
    let core = builder().build().await.unwrap();
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    source_control.starting_only.store(true, Ordering::Release);
    let source_status = source_control.clone();
    let source = core.add_source_fixture_with_handle(source).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        core.start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        ),
    )
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
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .await
        .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    query.wait_started().await.unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    reaction_control
        .starting_only
        .store(true, Ordering::Release);
    let reaction_status = reaction_control.clone();
    let reaction = core
        .add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        core.start_reaction_by_name(
            &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        ),
    )
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
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.starting_only.store(true, Ordering::Release);
    let status = control.clone();
    let handle = core.add_source_fixture_with_handle(source).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
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
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

async fn verify_query_subscription_count_tracks_running_helpers() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    source_control.auto_start.store(true, Ordering::Release);
    source_control
        .pending_subscription
        .store(true, Ordering::Release);
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .unwrap();
    let handle = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        "query",
    )
    .await;
    let mut start = Box::pin(query.start());
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            result = &mut start => panic!("start finished before source subscription: {result:?}"),
            _ = source_control.subscription_entered.notified() => {},
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        query.status().await,
        ComponentStatus::Starting | ComponentStatus::Running
    ));
    assert_eq!(
        query.subscription_count().await,
        0,
        "a configured source is not an active subscription"
    );
    source_control.subscription_release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), &mut start)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(query.subscription_count().await, 1);
    query.stop().await.unwrap();
    assert_eq!(query.subscription_count().await, 0);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn query_subscription_count_uses_actual_helpers_current_thread() {
    verify_query_subscription_count_tracks_running_helpers().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_subscription_count_uses_actual_helpers_multi_thread() {
    verify_query_subscription_count_tracks_running_helpers().await;
}

#[tokio::test]
async fn query_start_does_not_wait_for_the_instance_subscription_fence() {
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.auto_start.store(true, Ordering::Release);
    control
        .release_bootstrap_at_fence
        .store(true, Ordering::Release);
    let bootstrap = control.clone();
    let query = crate::Query::cypher(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .query("MATCH (n:Person) RETURN n")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .enable_bootstrap(true)
    .auto_start(true)
    .build()
    .unwrap();
    let core = builder()
        .with_source_factory_async(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            None,
            true,
            move |id| async move {
                let source = source.construct(id)?;
                source
                    .inner
                    .set_bootstrap_provider(GatedBootstrap(bootstrap))
                    .await;
                Ok((source, HashMap::new()))
            },
        )
        .with_query(query)
        .build()
        .await
        .unwrap();
    let query = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        "query",
    )
    .await;
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

async fn late_subscriptions_release_the_fence_once_when_actually_bound() {
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.auto_start.store(true, Ordering::Release);
    let mut fast = config(
        drasi_core_types::ComponentName::try_new("fast").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    );
    fast.auto_start = true;
    let core = Arc::new(
        builder()
            .with_source_fixture(source)
            .with_query(fast)
            .build()
            .await
            .unwrap(),
    );
    core.start().await.unwrap();
    assert_eq!(control.subscription_fences.load(Ordering::Acquire), 1);
    control.pending_subscription.store(true, Ordering::Release);
    let mut slow = config(
        drasi_core_types::ComponentName::try_new("slow").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    );
    slow.auto_start = true;
    let adding = tokio::spawn({
        let core = core.clone();
        async move { core.add_query(slow).await }
    });
    tokio::time::timeout(
        Duration::from_secs(3),
        control.subscription_entered.notified(),
    )
    .await
    .unwrap();
    core.source_manager()
        .subscriptions_complete()
        .await
        .unwrap();
    assert_eq!(control.subscription_fences.load(Ordering::Acquire), 1);
    control.pending_subscription.store(false, Ordering::Release);
    control.subscription_release.notify_one();
    adding.await.unwrap().unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("slow").expect("fixture component name"),
        )
        .unwrap()
        .wait_started(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(control.subscription_fences.load(Ordering::Acquire), 2);
    core.source_manager()
        .subscriptions_complete()
        .await
        .unwrap();
    assert_eq!(control.subscription_fences.load(Ordering::Acquire), 2);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn late_subscription_fence_current_thread() {
    late_subscriptions_release_the_fence_once_when_actually_bound().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_subscription_fence_multiple_threads() {
    late_subscriptions_release_the_fence_once_when_actually_bound().await;
}

#[tokio::test]
async fn query_handle_waits_for_bootstrap_completion_and_reports_bootstrap_failure() {
    for fail in [false, true] {
        let (source, control) = ControlledSource::fixture(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        );
        control.auto_start.store(true, Ordering::Release);
        control.fail_bootstrap.store(fail, Ordering::Release);
        let bootstrap = control.clone();
        let query = crate::Query::cypher(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .query("MATCH (n:Person) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        )
        .enable_bootstrap(true)
        .auto_start(true)
        .build()
        .unwrap();
        let core = builder()
            .with_source_factory_async(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
                None,
                true,
                move |id| async move {
                    let source = source.construct(id)?;
                    source
                        .inner
                        .set_bootstrap_provider(GatedBootstrap(bootstrap))
                        .await;
                    Ok((source, HashMap::new()))
                },
            )
            .with_query(query)
            .build()
            .await
            .unwrap();
        let query = component_handle(
            &core,
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            "query",
        )
        .await;
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
            let instance = core.computation_runtime.query(query.id()).await.unwrap();
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
    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let mut counts = Vec::new();
    for (id, component_id, auto_start) in [
        ("automatic", crate::test_helpers::component_id(1), true),
        ("manual", crate::test_helpers::component_id(2), false),
    ] {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let handle = core
            .add_computation_component(
                ComponentAddition::new(
                    ComponentName::try_new(id).unwrap(),
                    ConstructedComponent::service(Box::new(NativeService {
                        descriptor: ComponentDescriptor::try_new(component_id, vec![]).unwrap(),
                        starts: starts.clone(),
                        stops: stops.clone(),
                    })),
                )
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
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name"),
    );
    let core = builder()
        .with_allow_external_component_ids(true)
        .with_source_fixture(source)
        .build()
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref();
    let record = runtime
        .record(&component_id_by_name(&core, "ordinary"), "source")
        .await
        .unwrap();
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
    for (id, component_id, fail) in [
        ("native-ready", crate::test_helpers::component_id(1), false),
        ("native-failed", crate::test_helpers::component_id(2), true),
    ] {
        let spec = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(component_id, vec![]).unwrap(),
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
            ComponentAddition::from_specification(
                ComponentName::try_new(id).unwrap(),
                spec,
                factory.clone(),
            )
            .auto_start(true),
        )
        .await
        .unwrap();
    }
    let blocked = core
        .add_computation_component(
            ComponentAddition::new(
                ComponentName::try_new("native-blocked").unwrap(),
                ConstructedComponent::source(Box::new(UnboundNativeSource {
                    descriptor: ComponentDescriptor::try_new(
                        crate::test_helpers::component_id(780),
                        vec![PortDescriptor::new(
                            PortId::try_new("out").unwrap(),
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                })),
            )
            .auto_start(true),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime
            .current_records()
            .await
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![record.node.clone()]
    );
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        runtime.start_native_components(false),
    )
    .await
    .expect("blocked and failed creation must not leave startup waiting")
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(!message.contains("native-blocked"), "{message}");
    assert!(
        message.contains("injected native creation failure"),
        "{message}"
    );
    let failures = error
        .downcast_ref::<crate::error::OperationFailures>()
        .unwrap();
    assert_eq!(failures.failures().len(), 1);
    assert!(failures.failures().iter().any(|error| error
        .chain()
        .any(|cause| { cause.downcast_ref::<NativeCreationFailure>().is_some() })));
    assert_eq!(
        blocked.observed().unwrap().realization,
        RealizationState::Blocked
    );
    assert!(blocked
        .observed()
        .unwrap()
        .waiting_reason()
        .unwrap()
        .contains(&format!("{}:out", blocked.id())));
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert_eq!(source_control.starts.load(Ordering::Acquire), 0);
    assert_eq!(
        core.computation_control()
            .unwrap()
            .component_handle_by_id(&record.node)
            .unwrap()
            .generation(),
        runtime.handle_for(&record).unwrap().generation()
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn default_builder_is_native_and_preserves_public_result_signatures() {
    let core = DrasiLib::builder()
        .with_query_factory(
            crate::computation::v1::FactoryRegistry::standard()
                .get(
                    &crate::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .build()
        .await
        .unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("added-by-default")
            .expect("fixture component name"),
    );
    core.add_source_fixture_with_handle(source)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    assert_eq!(control.initializations.load(Ordering::Acquire), 1);
    let mut invalid = config(
        drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
        None,
    );
    invalid.query = "this is not a query".into();
    let result: crate::Result<()> = core.add_query(invalid).await;
    result.unwrap();
    assert!(core
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await
        .is_err());
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.add_query(config(
        drasi_core_types::ComponentName::try_new("missing").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    ))
    .await
    .unwrap();
    assert_waiting_for(
        &core,
        &core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("missing")
                    .expect("fixture component name"),
            )
            .unwrap(),
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await;
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("missing").expect("fixture component name"),
    )
    .await
    .unwrap();
    assert!(core.list_queries().await.unwrap().is_empty());
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let added: crate::Result<()> = core.add_source_fixture(source).await;
    added.unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    assert!(!source_control.resource_observers.lock().unwrap().is_empty());
    core.add_query(config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    ))
    .await
    .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let added: crate::Result<()> = core.add_reaction_fixture(reaction).await;
    added.unwrap();
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    assert!(!reaction_control
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
    let (plugin, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("plugin-source").expect("fixture component name"),
    );
    core.add_source_fixture_with_metadata(plugin, metadata.clone())
        .await
        .unwrap();
    let (custom, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("custom-source").expect("fixture component name"),
    );
    core.add_source_fixture(custom).await.unwrap();
    core.add_query(config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        None,
    ))
    .await
    .unwrap();
    let (plugin, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("plugin-reaction")
            .expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    core.add_reaction_fixture_with_metadata(plugin, metadata)
        .await
        .unwrap();
    for (id, kind, expected) in [
        ("plugin-source", "source", Some(&expected)),
        ("custom-source", "source", None),
        ("plugin-reaction", "reaction", Some(&expected)),
    ] {
        let name = crate::ComponentName::try_new(id).expect("fixture component name");
        let handle = component_handle(&core, &name, kind).await;
        handle.wait_created().await.unwrap();
        let mut observations = core.computation_control().unwrap().subscribe_registry();
        let snapshot = tokio::time::timeout(
            Duration::from_secs(3),
            observations.wait_for(|snapshot| {
                snapshot.desired.component_plugins.get(handle.id()) == expected
            }),
        )
        .await
        .expect("host-supplied provenance must be published")
        .unwrap();
        assert_eq!(
            snapshot.desired.component_plugins.get(handle.id()),
            expected,
        );
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn data_subscriptions_track_actual_nodes_and_changed_dependencies() {
    let (a, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("a").expect("fixture component name"),
    );
    let (b, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("b").expect("fixture component name"),
    );
    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("q1").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(a)
        .with_source_fixture(b)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("a").expect("fixture component name")),
        ))
        .with_query(config(
            drasi_core_types::ComponentName::try_new("q2").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("b").expect("fixture component name")),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    let runtime = core.computation_runtime.as_ref();
    let nodes = runtime.current_records().await.unwrap();
    for (id, record) in &nodes {
        assert_eq!(&record.node, id);
    }
    let a = component_id_by_name(&core, "a");
    let b = component_id_by_name(&core, "b");
    let q1 = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let q2 = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("q2").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let reaction = component_id_by_name(&core, "reaction");
    for id in [&a, &b, &q1, &q2, &reaction] {
        core.component_by_id(id)
            .unwrap()
            .wait_created()
            .await
            .unwrap();
    }
    let status = core.reconcile_desired_state().await.unwrap();
    assert!(status.applied && status.error.is_none(), "{status:?}");
    let control_only = vec![(b.clone(), reaction.clone())];
    runtime
        .control()
        .unwrap()
        .management_control()
        .set_control_connections(control_only.clone())
        .await
        .unwrap();
    let mut expected = vec![
        (a.clone(), q1.clone()),
        (
            a.clone(),
            ComputationPipelineBuilder::source_component_id(&q1, &a).unwrap(),
        ),
        (b.clone(), q2.clone()),
        (
            b.clone(),
            ComputationPipelineBuilder::source_component_id(&q2, &b).unwrap(),
        ),
        (q1.clone(), reaction.clone()),
    ];
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
    runtime
        .control()
        .unwrap()
        .management_control()
        .set_control_connections(Vec::new())
        .await
        .unwrap();
    core.remove_reaction_by_id(&reaction, false).await.unwrap();
    core.remove_query_by_id(&q1).await.unwrap();
    let new_q1 = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("b").expect("fixture component name")),
        ))
        .await
        .unwrap();
    new_q1.wait_created().await.unwrap();
    assert_ne!(new_q1.id(), &q1);
    let q1 = new_q1.id().clone();
    let (new_reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("q2").expect("fixture component name")],
    );
    let new_reaction = core
        .add_reaction_fixture_with_handle(new_reaction)
        .await
        .unwrap();
    new_reaction.wait_created().await.unwrap();
    assert_ne!(new_reaction.id(), &reaction);
    let reaction = new_reaction.id().clone();
    let mut expected = vec![
        (b.clone(), q1.clone()),
        (
            b.clone(),
            ComputationPipelineBuilder::source_component_id(&q1, &b).unwrap(),
        ),
        (b.clone(), q2.clone()),
        (
            b.clone(),
            ComputationPipelineBuilder::source_component_id(&q2, &b).unwrap(),
        ),
        (q2.clone(), reaction.clone()),
    ];
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
    assert!(runtime
        .control()
        .unwrap()
        .desired_snapshot()
        .control_connections
        .is_empty());
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("a").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.remove_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await
    .unwrap();
    let expected = BTreeSet::from([
        (b.clone(), q2.clone()),
        (
            b.clone(),
            ComputationPipelineBuilder::source_component_id(&q2, &b).unwrap(),
        ),
    ]);
    assert_eq!(
        runtime
            .parent()
            .unwrap()
            .control()
            .desired_snapshot()
            .subscriptions
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        expected
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_control_handlers_run_while_legacy_start_and_enqueue_are_blocked() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    source_control.pending_start.store(true, Ordering::Release);
    let (reaction, reaction_control, mut output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = Arc::new(builder().build().await.unwrap());
    let source = core.add_source_fixture_with_handle(source).await.unwrap();
    let query = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .await
        .unwrap();
    let reaction = core
        .add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap();
    reaction.wait_created().await.unwrap();
    let source_messages = Arc::new(PeerControl::default());
    let reaction_messages = Arc::new(PeerControl::default());
    assert!(source
        .set_control_handler(source_messages.clone())
        .await
        .is_err());
    let owner = core.computation_control().unwrap().management_control();
    owner
        .component_handle_by_id(source.id())
        .unwrap()
        .set_control_handler(source_messages.clone())
        .await
        .unwrap();
    owner
        .component_handle_by_id(reaction.id())
        .unwrap()
        .set_control_handler(reaction_messages.clone())
        .await
        .unwrap();
    let start = tokio::spawn({
        let core = core.clone();
        async move {
            core.start_source_by_name(
                &drasi_core_types::ComponentName::try_new("source")
                    .expect("fixture component name"),
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), source_control.entered.notified())
        .await
        .unwrap();
    let sender = query.control().unwrap();
    assert_eq!(sender.id(), query.id());
    sender.ready().unwrap();
    let ready = source_messages
        .wait_for(|message| {
            message.from == *query.id() && message.notification == ControlNotification::Ready
        })
        .await;
    assert_eq!(ready.direction, ControlDirection::Upstream);
    core.stop_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    assert!(start.await.unwrap().is_err());
    source_control.pending_start.store(false, Ordering::Release);
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    reaction_control
        .pending_enqueue
        .store(true, Ordering::Release);
    insert(&core, &component_id_by_name(&core, "source")).await;
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
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    source_control.fail_starts.store(1, Ordering::Release);
    core.add_source_fixture(source).await.unwrap();
    let query = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .await
        .unwrap();
    let (reaction, reaction_control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    core.add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let messages = Arc::new(PeerControl::default());
    assert!(query.set_control_handler(messages.clone()).await.is_err());
    core.computation_control()
        .unwrap()
        .management_control()
        .component_handle_by_id(query.id())
        .unwrap()
        .set_control_handler(messages.clone())
        .await
        .unwrap();
    let source_id = component_id_by_name(&core, "source");
    let reaction_id = component_id_by_name(&core, "reaction");

    assert!(core
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .is_err());
    let failure = messages
        .wait_for(|message| {
            message.from == source_id
                && matches!(&message.notification, ControlNotification::Unavailable { reason }
                    if reason.contains("injected start failure"))
        })
        .await;
    assert_eq!(failure.direction, ControlDirection::Downstream);

    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    let updates = reaction_control.updates.lock().unwrap().clone().unwrap();
    updates
        .send(ComponentUpdate::Status {
            component_id: reaction_id.clone(),
            status: ComponentStatus::Error,
            message: Some("legacy reaction background failure".into()),
        })
        .await
        .unwrap();
    let failure = messages
        .wait_for(|message| {
            message.from == reaction_id
                && matches!(&message.notification, ControlNotification::Unavailable { reason }
                    if reason.contains("legacy reaction background failure"))
        })
        .await;
    assert_eq!(failure.direction, ControlDirection::Upstream);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn transient_start_failure_can_be_retried_without_replacing_the_source() {
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.fail_starts.store(1, Ordering::Release);
    let core = builder().with_source_fixture(source).build().await.unwrap();
    assert!(core
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .is_err());
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    assert_eq!(control.starts.load(Ordering::Acquire), 2);
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_interrupts_a_pending_component_start() {
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.pending_start.store(true, Ordering::Release);
    let core = Arc::new(builder().with_source_fixture(source).build().await.unwrap());
    let start = tokio::spawn({
        let core = core.clone();
        async move {
            core.start_source_by_name(
                &drasi_core_types::ComponentName::try_new("source")
                    .expect("fixture component name"),
            )
            .await
        }
    });
    control.entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(2),
        core.stop_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        ),
    )
    .await
    .expect("stop must reach the controller while start is pending")
    .unwrap();
    assert!(start.await.unwrap().is_err());
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Stopped
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_component_stops_queue_behind_an_in_flight_stop() {
    let (first, first_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("first").expect("fixture component name"),
    );
    let (second, second_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("second").expect("fixture component name"),
    );
    let (third, third_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("third").expect("fixture component name"),
    );
    let core = Arc::new(
        builder()
            .with_source_fixture(first)
            .with_source_fixture(second)
            .with_source_fixture(third)
            .build()
            .await
            .unwrap(),
    );
    for id in ["first", "second", "third"] {
        core.start_source_by_name(
            &crate::ComponentName::try_new(id).expect("fixture component name"),
        )
        .await
        .unwrap();
    }
    first_control.pending_stop.store(true, Ordering::Release);
    let first_stop = tokio::spawn({
        let core = core.clone();
        async move {
            core.stop_source_by_name(
                &drasi_core_types::ComponentName::try_new("first").expect("fixture component name"),
            )
            .await
        }
    });
    first_control.stop_entered.notified().await;

    // Only overlapping requests queue; unrelated components remain serviceable.
    let first_handle = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("first").expect("fixture component name"),
        )
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first_handle.stop())
            .await
            .is_err(),
        "a same-component stop waits for the existing cleanup owner"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first_handle.start())
            .await
            .is_err(),
        "an abandoned queued start must not revive the stopped component later"
    );
    let second_stop = tokio::spawn({
        let first_handle = first_handle.clone();
        async move { first_handle.stop().await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !second_stop.is_finished(),
        "the second stop must queue, not fail"
    );
    assert_eq!(second_control.stops.load(Ordering::Acquire), 0);
    assert_eq!(first_control.stops.load(Ordering::Acquire), 1);
    tokio::time::timeout(
        Duration::from_secs(2),
        core.stop_source_by_name(
            &drasi_core_types::ComponentName::try_new("second").expect("fixture component name"),
        ),
    )
    .await
    .expect("an unrelated component must stop while the first cleanup is pending")
    .unwrap();

    first_control.stop_release.notify_one();
    first_stop.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), second_stop)
        .await
        .expect("the queued stop runs once the in-flight stop completes")
        .unwrap()
        .unwrap();
    for (id, status) in [
        ("first", ComponentStatus::Stopped),
        ("second", ComponentStatus::Stopped),
        ("third", ComponentStatus::Running),
    ] {
        assert_eq!(
            core.get_source_status_by_name(
                &crate::ComponentName::try_new(id).expect("fixture component name"),
            )
            .await
            .unwrap(),
            status,
            "{id}"
        );
    }
    assert_eq!(first_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(first_control.stops.load(Ordering::Acquire), 1);
    assert_eq!(second_control.stops.load(Ordering::Acquire), 1);
    assert_eq!(third_control.stops.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn component_starts_queue_and_a_later_stop_supersedes_a_queued_start() {
    let (hung, hung_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("hung").expect("fixture component name"),
    );
    let (superseded, superseded_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("superseded").expect("fixture component name"),
    );
    let (queued, queued_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("queued").expect("fixture component name"),
    );
    let core = Arc::new(
        builder()
            .with_source_fixture(hung)
            .with_source_fixture(superseded)
            .with_source_fixture(queued)
            .build()
            .await
            .unwrap(),
    );
    hung_control.pending_start.store(true, Ordering::Release);
    let start = |id: &'static str| {
        let core = core.clone();
        tokio::spawn(async move {
            core.start_source_by_name(
                &crate::ComponentName::try_new(id).expect("fixture component name"),
            )
            .await
        })
    };
    let hung_start = start("hung");
    hung_control.entered.notified().await;
    let hung_handle = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("hung").expect("fixture component name"),
        )
        .unwrap();
    let superseded_start = tokio::spawn({
        let hung_handle = hung_handle.clone();
        async move { hung_handle.start().await }
    });
    let queued_start = start("queued");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !superseded_start.is_finished(),
        "a same-component start must queue, not fail"
    );
    tokio::time::timeout(Duration::from_secs(2), queued_start)
        .await
        .expect("an unrelated start must progress independently")
        .unwrap()
        .unwrap();

    // An untouched component still retains the ordinary API's stopped check.
    let error = core
        .stop_source_by_name(
            &drasi_core_types::ComponentName::try_new("superseded")
                .expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("already stopped"),
        "{error:#}"
    );
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("superseded").expect("fixture component name"),
    )
    .unwrap()
    .stop()
    .await
    .unwrap();
    // A stop that reaches the controller supersedes the queued start without
    // waiting for the hung one.
    tokio::time::timeout(Duration::from_secs(2), hung_handle.stop())
        .await
        .expect("a stop is not queued behind an in-flight start")
        .unwrap();
    let error = superseded_start.await.unwrap().unwrap_err();
    assert!(
        format!("{error:#}").contains("superseded by a later stop"),
        "{error:#}"
    );
    assert_eq!(superseded_control.starts.load(Ordering::Acquire), 0);
    assert_eq!(hung_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(hung_control.stops.load(Ordering::Acquire), 1);
    assert!(hung_start.await.unwrap().is_err());
    for (id, status) in [
        ("hung", ComponentStatus::Stopped),
        ("superseded", ComponentStatus::Stopped),
        ("queued", ComponentStatus::Running),
    ] {
        assert_eq!(
            core.get_source_status_by_name(
                &crate::ComponentName::try_new(id).expect("fixture component name"),
            )
            .await
            .unwrap(),
            status,
            "{id}"
        );
    }
    assert_eq!(queued_control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_retirement_rejects_bound_queries_without_retargeting_by_name() {
    let (old, old_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (new, new_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, _, mut output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(old)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    let source_id = component_id_by_name(&core, "source");
    let before = core.desired_configuration().unwrap();
    let error = core
        .replace_source_fixture(
            crate::config::ComponentReference::by_name(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
            new,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("Depended on by:"));
    assert_eq!(core.desired_configuration().unwrap(), before);
    assert_eq!(component_id_by_name(&core, "source"), source_id);
    assert_eq!(
        core.get_query_config_by_name(&ComponentName::try_new("query").unwrap())
            .await
            .unwrap()
            .inputs[0]
            .component
            .as_id(),
        Some(&source_id)
    );
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    insert(&core, &component_id_by_name(&core, "source")).await;
    assert_eq!(next(&mut output).await.results.len(), 1);
    assert_eq!(old_control.subscriptions.load(Ordering::Acquire), 1);
    assert_eq!(new_control.subscriptions.load(Ordering::Acquire), 0);
    assert_eq!(new_control.initializations.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn identical_query_configuration_preserves_unstarted_reaction_bindings() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, _, mut output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    let query_id = component_id_by_name(&core, "query");
    let handle = core.component_by_id(&query_id).unwrap();
    handle.wait_created().await.unwrap();
    let unchanged = core
        .get_query_config_by_id(&query_id)
        .await
        .unwrap()
        .into_authoring();
    core.update_query_by_id(&query_id, unchanged).await.unwrap();
    assert_eq!(
        core.component_by_id(&query_id).unwrap().generation(),
        handle.generation()
    );
    assert_eq!(
        core.get_reaction_info_by_name(&ComponentName::try_new("reaction").unwrap())
            .await
            .unwrap()
            .queries,
        vec![crate::config::ComponentReference::by_id(query_id)]
    );
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    insert(&core, &component_id_by_name(&core, "source")).await;
    assert_eq!(next(&mut output).await.results.len(), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn recreated_consumers_release_the_retired_source_and_query_dependencies() {
    let (a, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("a").expect("fixture component name"),
    );
    let (b, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("b").expect("fixture component name"),
    );
    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("q1").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(a)
        .with_source_fixture(b)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("a").expect("fixture component name")),
        ))
        .with_query(config(
            drasi_core_types::ComponentName::try_new("q2").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("b").expect("fixture component name")),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    let old_reaction = component_id_by_name(&core, "reaction");
    let old_q1 = component_id_by_name(&core, "q1");
    core.remove_reaction_by_id(&old_reaction, false)
        .await
        .unwrap();
    core.remove_query_by_id(&old_q1).await.unwrap();
    let q1 = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            Some(drasi_core_types::ComponentName::try_new("b").expect("fixture component name")),
        ))
        .await
        .unwrap();
    q1.wait_created().await.unwrap();
    assert_ne!(q1.id(), &old_q1);
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("a").expect("fixture component name"),
        false,
    )
    .await
    .unwrap();
    let (reaction, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("q2").expect("fixture component name")],
    );
    let reaction = core
        .add_reaction_fixture_with_handle(reaction)
        .await
        .unwrap();
    reaction.wait_created().await.unwrap();
    assert_ne!(reaction.id(), &old_reaction);
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await
    .unwrap();
    let (a, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("a").expect("fixture component name"),
    );
    core.add_source_fixture(a).await.unwrap();
    core.add_query(config(
        drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("a").expect("fixture component name")),
    ))
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn duplicate_reaction_inputs_reject_before_construction_and_do_not_reserve_name() {
    let core = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .build()
        .await
        .unwrap();
    let before = core.desired_configuration().unwrap();
    let (bad, control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        ],
    );
    assert!(core.add_reaction_fixture(bad).await.is_err());
    assert_eq!(core.desired_configuration().unwrap(), before);
    assert_eq!(control.initializations.load(Ordering::Acquire), 0);
    assert!(core.list_reactions().await.unwrap().is_empty());
    let (good, _, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let replacement = core.add_reaction_fixture_with_handle(good).await.unwrap();
    replacement.wait_created().await.unwrap();
    core.remove_reaction_by_id(replacement.id(), false)
        .await
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn committed_creation_failure_remains_addressable_for_awaited_cleanup() {
    let core = builder().build().await.unwrap();
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    control.fail_initialize.store(true, Ordering::Release);
    core.add_source_fixture(source).await.unwrap();
    let handle = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        "source",
    )
    .await;
    assert_creation_failed(&core, &handle, "injected initialization failure").await;
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Error
    );
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
    assert_eq!(control.stops.load(Ordering::Acquire), 0);
    assert_eq!(control.deprovisions.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_retirement_retains_the_actual_owner_for_deprovisioning() {
    let (old, old_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    old_control.fail_starts.store(1, Ordering::Release);
    old_control.fail_stop.store(true, Ordering::Release);
    let core = builder().with_source_fixture(old).build().await.unwrap();
    assert!(core
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .is_err());
    let old_id = component_id_by_name(&core, "source");
    let error = core.remove_source_by_id(&old_id, true).await.unwrap_err();
    assert!(format!("{error:#}").contains("injected stop failure"));
    assert_eq!(old_control.deprovisions.load(Ordering::Acquire), 0);
    let (new, new_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    assert!(core.add_source_fixture(new).await.is_err());
    assert_eq!(new_control.initializations.load(Ordering::Acquire), 0);
    old_control.fail_stop.store(false, Ordering::Release);
    let status = core.reconcile_desired_state().await.unwrap();
    assert!(status.applied && status.error.is_none(), "{status:?}");
    assert_eq!(old_control.deprovisions.load(Ordering::Acquire), 1);
    assert_eq!(new_control.deprovisions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_reaction_observes_plugin_failure_and_awaits_stop() {
    let (reaction, control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    let mut events = core.subscribe_all_component_events();
    let updates = control.updates.lock().unwrap().clone().unwrap();
    updates
        .send(ComponentUpdate::Status {
            component_id: component_id_by_name(&core, "reaction"),
            status: ComponentStatus::Error,
            message: Some("background worker failed".into()),
        })
        .await
        .unwrap();
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("reaction").expect("fixture component name"),
        ComponentStatus::Error,
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

fn flood_lifecycle_callback(
    sender: &crate::channels::ComponentUpdateSender,
    id: &crate::ComponentId,
) {
    for _ in 0..10_000 {
        sender
            .try_send(ComponentUpdate::Status {
                component_id: id.clone(),
                status: ComponentStatus::Starting,
                message: None,
            })
            .unwrap();
    }
    for status in [ComponentStatus::Running, ComponentStatus::Error, ComponentStatus::Stopped] {
        sender
            .try_send(ComponentUpdate::Status {
                component_id: id.clone(),
                status,
                message: Some("failure after full callback queue".into()),
            })
            .unwrap();
    }
}

async fn source_callback_flood() {
    let (source, control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let local_status = control.clone();
    let core = builder().with_source_fixture(source).build().await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    let mut events = core.subscribe_all_component_events();
    let updates = control.updates.lock().unwrap().clone().unwrap();
    flood_lifecycle_callback(&updates, &component_id_by_name(&core, "source"));
    local_status
        .set_status(ComponentStatus::Stopped, None)
        .await;
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("source").expect("fixture component name"),
        ComponentStatus::Error,
        Duration::from_secs(3),
    )
    .await;
    core.shutdown().await.unwrap();
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn source_callback_flood_current_thread() {
    source_callback_flood().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_callback_flood_multi_thread() {
    source_callback_flood().await;
}

async fn reaction_callback_flood() {
    let (reaction, control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    let mut events = core.subscribe_all_component_events();
    let updates = control.updates.lock().unwrap().clone().unwrap();
    flood_lifecycle_callback(&updates, &component_id_by_name(&core, "reaction"));
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("reaction").expect("fixture component name"),
        ComponentStatus::Error,
        Duration::from_secs(3),
    )
    .await;
    core.shutdown().await.unwrap();
    assert_eq!(control.stops.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn reaction_callback_flood_current_thread() {
    reaction_callback_flood().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaction_callback_flood_multi_thread() {
    reaction_callback_flood().await;
}

#[tokio::test]
async fn cancelled_replacement_caller_still_selects_the_committed_instance() {
    let (old, old_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let core = Arc::new(builder().with_source_fixture(old).build().await.unwrap());
    let old_handle = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        "source",
    )
    .await;
    let old_record = core
        .computation_runtime
        .record(old_handle.id(), "source")
        .await
        .unwrap();
    let (new, new_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    new_control
        .pending_initialize
        .store(true, Ordering::Release);
    let update = tokio::spawn({
        let core = core.clone();
        async move {
            core.replace_source_fixture(
                crate::config::ComponentReference::by_name(
                    drasi_core_types::ComponentName::try_new("source")
                        .expect("fixture component name"),
                ),
                new,
            )
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), new_control.entered.notified())
        .await
        .unwrap();
    let replacement = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        "source",
    )
    .await;
    assert_ne!(replacement.generation(), old_handle.generation());
    update.abort();
    if let Err(error) = update.await {
        assert!(error.is_cancelled());
    }
    new_control.release.notify_one();
    let runtime = core.computation_runtime.as_ref();
    assert!(matches!(
        runtime.control_for(&old_record),
        Err(error) if matches!(error.downcast_ref::<GraphError>(), Some(GraphError::StaleGeneration))
    ));
    replacement.wait_created().await.unwrap();
    let current = core.source_instance(replacement.id()).await.unwrap();
    assert!(Arc::ptr_eq(
        &current
            .as_any()
            .downcast_ref::<ControlledSource>()
            .unwrap()
            .control,
        &new_control,
    ));
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        true,
    )
    .await
    .unwrap();
    assert_eq!(new_control.deprovisions.load(Ordering::Acquire), 1);
    assert_eq!(old_control.deprovisions.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn retired_query_handles_cannot_operate_on_the_replacement_and_unbound_queries_stay_live() {
    let core = builder()
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .build()
        .await
        .unwrap();
    let old = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .unwrap();
    let old_id = component_id_by_name(&core, "query");
    core.remove_query_by_id(&old_id).await.unwrap();
    let replacement = core
        .add_query_with_handle(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            None,
        ))
        .await
        .unwrap();
    replacement.wait_created().await.unwrap();
    assert_ne!(replacement.id(), &old_id);
    assert!(old.start().await.is_err());
    let mut events = core.subscribe_all_component_events();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    crate::test_helpers::wait_for_component_status_by_name(
        &mut events,
        &core.component_graph(),
        &crate::ComponentName::try_new("query").expect("fixture component name"),
        ComponentStatus::Running,
        Duration::from_secs(2),
    )
    .await;
    assert!(old.stop().await.is_err());
    let rows = tokio::time::timeout(
        Duration::from_secs(2),
        core.get_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(rows.is_empty());
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Running
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn declared_references_validate_roles_before_acceptance() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let core = builder().with_source_fixture(source).build().await.unwrap();
    let runtime = core.computation_runtime.as_ref();
    let revision = runtime.inspector().unwrap().snapshot().desired.revision;
    let (reaction, control, _) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("source").expect("fixture component name")],
    );
    let error = core.add_reaction_fixture(reaction).await.unwrap_err();
    assert!(error.to_string().contains("not a query"));
    assert_eq!(
        runtime.inspector().unwrap().snapshot().desired.revision,
        revision
    );
    assert!(core.list_reactions().await.unwrap().is_empty());
    assert_eq!(control.initializations.load(Ordering::Acquire), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_service_logs_use_the_ordinary_instance_and_component_keys() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let core = builder()
        .with_id("native-log-parity")
        .with_source_fixture(source)
        .build()
        .await
        .unwrap();
    let (_, mut logs) = core
        .subscribe_source_logs_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
        )
        .await
        .unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
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
        _: &drasi_core_types::ComponentId,
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
    async fn create_scoped_indexes(
        &self,
        _scope: &crate::StorageScope,
        component_id: &crate::ComponentId,
    ) -> std::result::Result<drasi_core::interface::CreatedIndexes, drasi_core::interface::IndexError>
    {
        self.create_indexes(component_id).await
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn early_query_activation_failure_is_visible_and_bulk_stop_allows_another_attempt() {
    let (source, _) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let mut query_config = config(
        drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        Some(drasi_core_types::ComponentName::try_new("source").expect("fixture component name")),
    );
    query_config.auto_start = true;
    let core = builder()
        .with_source_fixture(source)
        .with_query(query_config)
        .with_default_index_provider("persistent", Arc::new(DeferredPersistentProvider))
        .build()
        .await
        .unwrap();
    let error = core
        .start()
        .await
        .expect_err("failed query activation is reported");
    assert!(error
        .downcast_ref::<crate::error::PartialStartError>()
        .is_some());
    assert!(core.is_running().await);
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .unwrap();
    assert_eq!(query.status().await, ComponentStatus::Error);
    assert!(matches!(
        query.fetch_snapshot().await,
        Err(crate::queries::FetchError::NotRunning {
            status: ComponentStatus::Error
        })
    ));
    let _runtime = core.computation_runtime.as_ref();
    let handle = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap();
    let first = handle.observed().unwrap();
    assert!(format!("{:#}", first.failure.as_ref().unwrap().cause).contains("IncompatibleSource"));
    core.stop().await.unwrap();
    let error = core
        .start()
        .await
        .expect_err("retry still reports the incompatible source");
    assert!(error
        .downcast_ref::<crate::error::PartialStartError>()
        .is_some());
    let second = handle.observed().unwrap();
    assert!(second.operation.0 > first.operation.0);
    assert!(format!("{:#}", second.failure.as_ref().unwrap().cause).contains("IncompatibleSource"));
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_running_query_update_preserves_consumers_and_unaffected_components() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, reaction_control, mut output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    reaction_control.snapshot.store(true, Ordering::Release);
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_query(config(
            drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    let runtime = core.computation_runtime.as_ref();
    let source_token = runtime
        .record(&component_id_by_name(&core, "source"), "source")
        .await
        .unwrap()
        .token;
    let unrelated_token = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name"),
        )
        .unwrap()
        .generation();
    insert(&core, &component_id_by_name(&core, "source")).await;
    assert_eq!(next(&mut output).await.results.len(), 1);

    let query_id = component_id_by_name(&core, "query");
    let before = core.desired_configuration().unwrap();
    let query_generation = core.component_by_id(&query_id).unwrap().generation();
    let mut updated = core
        .get_query_config_by_id(&query_id)
        .await
        .unwrap()
        .into_authoring();
    updated.query = "MATCH (n:Person) RETURN n.name AS name, 'new' AS version".into();
    let error = core
        .update_query_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            updated,
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("query factory does not support configuration updates"));
    assert_eq!(core.desired_configuration().unwrap(), before);
    assert_eq!(
        core.component_by_id(&query_id).unwrap().generation(),
        query_generation
    );
    insert_person(&core, &component_id_by_name(&core, "source"), "two", "Bob").await;
    let result = match tokio::time::timeout(Duration::from_secs(2), output.recv()).await {
        Ok(Some(result)) => result,
        other => panic!(
            "retained query result missing: {other:?}; query rows: {:?}; graph: {:?}; reaction starts: {}",
            core.query_manager().get_query_results_by_name(&drasi_core_types::ComponentName::try_new("query").expect("fixture component name")).await,
            core.computation_control().unwrap().observed().components,
            reaction_control.starts.load(Ordering::Acquire)
        ),
    };
    assert!(matches!(&result.results[..],
        [crate::channels::ResultDiff::Add { data, .. }]
        if data == &serde_json::json!({"name":"Bob"})));
    assert_eq!(reaction_control.starts.load(Ordering::Acquire), 1);
    assert_eq!(
        runtime
            .record(&component_id_by_name(&core, "source"), "source")
            .await
            .unwrap()
            .token,
        source_token
    );
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name")
        )
        .unwrap()
        .generation(),
        unrelated_token
    );
    let query_token = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .unwrap()
        .generation();

    let (replacement, replacement_control, _replacement_output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    replacement_control.snapshot.store(true, Ordering::Release);
    core.replace_reaction_fixture(
        crate::config::ComponentReference::by_name(
            drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        ),
        replacement,
    )
    .await
    .unwrap();
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name")
        )
        .unwrap()
        .generation(),
        query_token
    );
    assert_eq!(
        runtime
            .record(&component_id_by_name(&core, "source"), "source")
            .await
            .unwrap()
            .token,
        source_token
    );
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("unrelated").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("source").expect("fixture component name")
        )
        .await
        .unwrap(),
        ComponentStatus::Running
    );
    assert_eq!(source_control.starts.load(Ordering::Acquire), 1);
    core.shutdown().await.unwrap();
}

async fn quiescing_ordinary_query_pauses_its_independently_driven_graph() {
    let (source, source_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    );
    let (reaction, reaction_control, mut output) = ControlledReaction::fixture(
        drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
        &[drasi_core_types::ComponentName::try_new("query").expect("fixture component name")],
    );
    let core = builder()
        .with_source_fixture(source)
        .with_query(config(
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            Some(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            ),
        ))
        .with_reaction_fixture(reaction)
        .build()
        .await
        .unwrap();
    core.start().await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await
    .unwrap();
    insert(&core, &component_id_by_name(&core, "source")).await;
    assert_eq!(next(&mut output).await.results.len(), 1);

    let runtime = core.computation_runtime.as_ref();
    let control = runtime.control().unwrap();
    let query = component_handle(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        "query",
    )
    .await;
    let selected = GraphSelection::Exact(vec![query.id().clone()]);
    control
        .quiesce_components(control.desired_snapshot().revision, selected)
        .await
        .unwrap();
    assert_eq!(
        query.observed().unwrap().lifecycle,
        ComponentLifecycle::Quiesced
    );
    insert_person(&core, &component_id_by_name(&core, "source"), "two", "Bob").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), output.recv())
            .await
            .is_err(),
        "a quiesced ordinary query must not keep evaluating on its nested driver"
    );
    query.start().await.unwrap();
    let instance = core.computation_runtime.query(query.id()).await.unwrap();
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
    tokio::time::timeout(
        Duration::from_secs(10),
        quiescing_ordinary_query_pauses_its_independently_driven_graph(),
    )
    .await
    .expect("query quiescence and resume must complete on current_thread");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_query_quiescence_multi_thread() {
    tokio::time::timeout(
        Duration::from_secs(10),
        quiescing_ordinary_query_pauses_its_independently_driven_graph(),
    )
    .await
    .expect("query quiescence and resume must complete on multi_thread");
}
