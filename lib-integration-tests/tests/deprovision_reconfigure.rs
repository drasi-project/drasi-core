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

//! Integration tests for the deprovision and reconfigure features.
//!
//! These tests exercise the full DrasiLib public API for:
//! - `remove_source(id, cleanup: bool)` / `remove_reaction(id, cleanup: bool)`
//! - registered source/reaction definition updates through the configuration owner
//! - Lifecycle event streaming during deprovision and reconfigure operations
//! - Log history preservation across reconfigure cycles

use async_trait::async_trait;
use drasi_lib::{
    channels::QueryResult,
    computation::v1::{
        ComponentId, ComponentName, ComponentUpdateData, ComponentUpdateDirective,
        ComponentUpdateMode, ComponentUpdatePlan,
    },
    config::ComponentReference,
    management::{
        OrdinaryComponent, OrdinaryComponentDefinition, OrdinaryComponentRecord,
        OrdinaryComponentResolver,
    },
    ComponentEvent, ComponentStatus, DrasiLib, MemoryStateStoreProvider, Query, Reaction,
    ReactionRuntimeContext, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, StateStoreProvider, SubscriptionResponse,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use tracing::Instrument;

// ============================================================================
// Helpers
// ============================================================================

/// Wait for a source to reach the expected status via event notification.
async fn wait_for_source_status(
    drasi: &DrasiLib,
    name: &drasi_lib::ComponentName,
    expected: ComponentStatus,
) {
    // Subscribe BEFORE checking to avoid missing events
    let mut rx = drasi.subscribe_all_component_events();
    let id = drasi
        .component_by_name(name)
        .expect("declared source")
        .id()
        .clone();
    if drasi
        .get_source_status_by_id(&id)
        .await
        .expect("source status")
        == expected
    {
        return;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.component_id == id && event.status == expected => return,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Event channel closed while waiting for source '{id}'");
                }
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("Timed out waiting for source '{id}' to reach {expected:?}");
    });
}

/// Wait for a reaction to reach the expected status via event notification.
async fn wait_for_reaction_status(
    drasi: &DrasiLib,
    name: &drasi_lib::ComponentName,
    expected: ComponentStatus,
) {
    // Subscribe BEFORE checking to avoid missing events
    let mut rx = drasi.subscribe_all_component_events();
    let id = drasi
        .component_by_name(name)
        .expect("declared reaction")
        .id()
        .clone();
    if drasi
        .get_reaction_status_by_id(&id)
        .await
        .expect("reaction status")
        == expected
    {
        return;
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(event) if event.component_id == id && event.status == expected => return,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("Event channel closed while waiting for reaction '{id}'");
                }
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("Timed out waiting for reaction '{id}' to reach {expected:?}");
    });
}

#[derive(Clone, Default)]
struct LifecycleProbe {
    delete_owned_resources_called: Arc<AtomicBool>,
    start_count: Arc<AtomicU32>,
    stop_count: Arc<AtomicU32>,
}

// ============================================================================
// Test Source with deprovision/reconfigure support
// ============================================================================

struct TestSource {
    base: SourceBase,
    probe: LifecycleProbe,
}

impl TestSource {
    fn new(id: drasi_core::ComponentId) -> anyhow::Result<Self> {
        Self::with_probe(id, LifecycleProbe::default())
    }

    fn with_probe(id: drasi_core::ComponentId, probe: LifecycleProbe) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            probe,
        })
    }
}

#[async_trait]
impl Source for TestSource {
    fn id(&self) -> &drasi_core::ComponentId {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "deprovision-test-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.auto_start
    }

    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.probe.start_count.fetch_add(1, Ordering::SeqCst);
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_source_start",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("TestSource starting");
        }
        .instrument(span)
        .await;

        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.probe.stop_count.fetch_add(1, Ordering::SeqCst);
        let source_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_source_stop",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        async {
            tracing::info!("TestSource stopping");
        }
        .instrument(span)
        .await;

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
            .subscribe_with_bootstrap(&settings, "test-source")
            .await
    }

    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.probe
            .delete_owned_resources_called
            .store(true, Ordering::SeqCst);
        self.base.delete_owned_resources_common().await?;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Test Reaction with deprovision/reconfigure support
// ============================================================================

struct TestReaction {
    base: drasi_lib::ReactionBase,
    probe: LifecycleProbe,
}

impl TestReaction {
    fn with_probe(
        id: drasi_core::ComponentId,
        query_ids: Vec<drasi_lib::config::ComponentReference>,
        probe: LifecycleProbe,
    ) -> Self {
        let params = drasi_lib::ReactionBaseParams::new(id, query_ids);
        Self {
            base: drasi_lib::ReactionBase::new(params),
            probe,
        }
    }
}

#[async_trait]
impl Reaction for TestReaction {
    fn id(&self) -> &drasi_core::ComponentId {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "deprovision-test-reaction"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_inputs(&self) -> Vec<drasi_lib::config::ComponentReference> {
        self.base.query_inputs().to_vec()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.probe.start_count.fetch_add(1, Ordering::SeqCst);
        let reaction_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_reaction_start",
            instance_id = %instance_id,
            component_id = %reaction_id,
            component_type = "reaction"
        );
        async {
            tracing::info!("TestReaction starting");
        }
        .instrument(span)
        .await;

        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.probe.stop_count.fetch_add(1, Ordering::SeqCst);
        let reaction_id = self.base.get_id().to_string();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "test_reaction_stop",
            instance_id = %instance_id,
            component_id = %reaction_id,
            component_type = "reaction"
        );
        async {
            tracing::info!("TestReaction stopping");
        }
        .instrument(span)
        .await;

        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.probe
            .delete_owned_resources_called
            .store(true, Ordering::SeqCst);
        self.base.delete_owned_resources_common().await?;
        Ok(())
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

#[derive(Default)]
struct TestOrdinaryResolver {
    probes: Mutex<HashMap<String, LifecycleProbe>>,
}

impl TestOrdinaryResolver {
    fn register(&self, key: &str) -> LifecycleProbe {
        let probe = LifecycleProbe::default();
        assert!(
            self.probes
                .lock()
                .expect("lifecycle probe registry lock")
                .insert(key.to_string(), probe.clone())
                .is_none(),
            "duplicate lifecycle probe {key}"
        );
        probe
    }

    fn probe(&self, definition: &OrdinaryComponentRecord) -> anyhow::Result<LifecycleProbe> {
        let configuration = match &definition.component {
            OrdinaryComponent::Source { configuration, .. }
            | OrdinaryComponent::Reaction { configuration, .. } => configuration,
            _ => anyhow::bail!("lifecycle fixture requires a source or reaction definition"),
        };
        let key = configuration
            .get("probe")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("lifecycle fixture requires a probe key"))?;
        self.probes
            .lock()
            .expect("lifecycle probe registry lock")
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown lifecycle probe {key}"))
    }
}

#[async_trait]
impl OrdinaryComponentResolver for TestOrdinaryResolver {
    fn plan_update(
        &self,
        _instance_id: &str,
        current: &OrdinaryComponentRecord,
        proposed: &OrdinaryComponentRecord,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(current.id == proposed.id, "component UUID changed");
        anyhow::ensure!(current.name == proposed.name, "component name changed");
        anyhow::ensure!(
            matches!(
                (&current.component, &proposed.component),
                (
                    OrdinaryComponent::Source { .. },
                    OrdinaryComponent::Source { .. }
                ) | (
                    OrdinaryComponent::Reaction { .. },
                    OrdinaryComponent::Reaction { .. }
                )
            ),
            "component role changed"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create_source(
        &self,
        _instance_id: &str,
        definition: &OrdinaryComponentRecord,
        _update: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Source>, HashMap<String, String>)> {
        Ok((
            Box::new(TestSource::with_probe(
                definition.id.clone(),
                self.probe(definition)?,
            )?),
            HashMap::new(),
        ))
    }

    async fn create_reaction(
        &self,
        _instance_id: &str,
        definition: &OrdinaryComponentRecord,
        inputs: &[ComponentId],
        _update: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Reaction>, HashMap<String, String>)> {
        Ok((
            Box::new(TestReaction::with_probe(
                definition.id.clone(),
                inputs
                    .iter()
                    .cloned()
                    .map(ComponentReference::by_id)
                    .collect(),
                self.probe(definition)?,
            )),
            HashMap::new(),
        ))
    }
}

// ============================================================================
// Helper
// ============================================================================

fn component_name(name: &str) -> ComponentName {
    ComponentName::try_new(name).expect("component name")
}

fn source_definition(
    id: ComponentId,
    name: &str,
    auto_start: bool,
    probe: &str,
) -> OrdinaryComponentRecord {
    OrdinaryComponentRecord {
        id,
        name: component_name(name),
        auto_start,
        component: OrdinaryComponent::Source {
            kind: "lifecycle-fixture".into(),
            configuration: serde_json::json!({"probe": probe}),
        },
    }
}

fn reaction_definition(
    id: ComponentId,
    name: &str,
    query_name: Option<&str>,
    auto_start: bool,
    probe: &str,
) -> OrdinaryComponentRecord {
    OrdinaryComponentRecord {
        id,
        name: component_name(name),
        auto_start,
        component: OrdinaryComponent::Reaction {
            kind: "lifecycle-fixture".into(),
            configuration: serde_json::json!({"probe": probe}),
            inputs: query_name
                .map(|name| {
                    vec![ComponentReference::by_name(
                        drasi_core::ComponentName::try_new(name).expect("fixture component name"),
                    )]
                })
                .unwrap_or_default(),
        },
    }
}

async fn settle_managed_configuration(drasi: &DrasiLib) {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = drasi
                .reconcile_desired_state()
                .await
                .expect("managed lifecycle reconciliation");
            assert!(status.error.is_none(), "{status:?}");
            if drasi
                .desired_configuration()
                .expect("managed lifecycle configuration")
                .pending_updates
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("managed configuration should settle");
}

async fn create_managed_source(
    drasi: &DrasiLib,
    resolver: &TestOrdinaryResolver,
    name: &str,
    auto_start: bool,
    probe_key: &str,
) -> (ComponentId, LifecycleProbe) {
    let probe = resolver.register(probe_key);
    let template = source_definition(ComponentId::generate(), name, auto_start, probe_key);
    let receipt = drasi
        .create_managed_ordinary_component(
            drasi
                .desired_configuration()
                .expect("managed source configuration")
                .revision,
            format!("create-source-{name}"),
            OrdinaryComponentDefinition {
                id: None,
                name: template.name,
                auto_start: template.auto_start,
                component: template.component,
            },
        )
        .await
        .expect("managed source acceptance");
    let id = receipt
        .creation
        .expect("source creation receipt")
        .components[0]
        .id
        .clone();
    settle_managed_configuration(drasi).await;
    timeout(
        Duration::from_secs(5),
        drasi
            .component_by_id(&id)
            .expect("managed source handle")
            .wait_created(),
    )
    .await
    .expect("source construction should finish")
    .expect("source construction should succeed");
    (id, probe)
}

async fn create_managed_reaction(
    drasi: &DrasiLib,
    resolver: &TestOrdinaryResolver,
    name: &str,
    query_name: &str,
    auto_start: bool,
    probe_key: &str,
) -> (ComponentId, LifecycleProbe) {
    let probe = resolver.register(probe_key);
    let template = reaction_definition(
        ComponentId::generate(),
        name,
        Some(query_name),
        auto_start,
        probe_key,
    );
    let receipt = drasi
        .create_managed_ordinary_component(
            drasi
                .desired_configuration()
                .expect("managed reaction configuration")
                .revision,
            format!("create-reaction-{name}"),
            OrdinaryComponentDefinition {
                id: None,
                name: template.name,
                auto_start: template.auto_start,
                component: template.component,
            },
        )
        .await
        .expect("managed reaction acceptance");
    let id = receipt
        .creation
        .expect("reaction creation receipt")
        .components[0]
        .id
        .clone();
    settle_managed_configuration(drasi).await;
    timeout(
        Duration::from_secs(5),
        drasi
            .component_by_id(&id)
            .expect("managed reaction handle")
            .wait_created(),
    )
    .await
    .expect("reaction construction should finish")
    .expect("reaction construction should succeed");
    (id, probe)
}

async fn collect_events(
    rx: &mut tokio::sync::broadcast::Receiver<ComponentEvent>,
    target_status: ComponentStatus,
    timeout_secs: u64,
) -> Vec<ComponentEvent> {
    let mut events = Vec::new();
    let _ = timeout(Duration::from_secs(timeout_secs), async {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let done = event.status == target_status;
                    events.push(event);
                    if done {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("Lagged {n} events");
                }
                Err(_) => break,
            }
        }
    })
    .await;
    events
}

// ============================================================================
// Source Deprovision Tests
// ============================================================================

/// Removing a source with cleanup=true calls deprovision.
#[tokio::test]
async fn test_source_remove_with_cleanup_calls_deprovision() {
    let probe = LifecycleProbe::default();
    let deprov_flag = probe.delete_owned_resources_called.clone();

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("deprov-src-1").expect("fixture component name"),
            true,
            move |id| TestSource::with_probe(id.clone(), probe),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-src-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("deprov-src-1").expect("fixture component name"),
            true,
        )
        .await
        .unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should have been called with cleanup=true"
    );

    // Source should no longer be listed
    let sources = drasi.list_sources().await.unwrap();
    assert!(
        !sources.iter().any(|source| {
            source.name
                == drasi_core::ComponentName::try_new("deprov-src-1")
                    .expect("fixture component name")
        }),
        "Source should be removed"
    );

    let _ = drasi.stop().await;
}

/// Removing a source with cleanup=false does NOT call deprovision.
#[tokio::test]
async fn test_source_remove_without_cleanup_skips_deprovision() {
    let probe = LifecycleProbe::default();
    let deprov_flag = probe.delete_owned_resources_called.clone();

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("deprov-src-2").expect("fixture component name"),
            true,
            move |id| TestSource::with_probe(id.clone(), probe),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-src-2").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("deprov-src-2").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();

    assert!(
        !deprov_flag.load(Ordering::SeqCst),
        "deprovision() should NOT have been called with cleanup=false"
    );

    let _ = drasi.stop().await;
}

/// Removing a stopped source with cleanup=true still calls deprovision.
#[tokio::test]
async fn test_source_remove_stopped_with_cleanup() {
    let probe = LifecycleProbe::default();
    let deprov_flag = probe.delete_owned_resources_called.clone();

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("deprov-src-3").expect("fixture component name"),
            true,
            move |id| TestSource::with_probe(id.clone(), probe),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-src-3").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Stop first, then remove
    drasi
        .stop_source_by_name(
            &drasi_core::ComponentName::try_new("deprov-src-3").expect("fixture component name"),
        )
        .await
        .unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-src-3").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;

    drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("deprov-src-3").expect("fixture component name"),
            true,
        )
        .await
        .unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should be called even on a stopped source"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Reaction Deprovision Tests
// ============================================================================

/// Removing a reaction with cleanup=true calls deprovision.
#[tokio::test]
async fn test_reaction_remove_with_cleanup_calls_deprovision() {
    let query = Query::cypher(
        drasi_core::ComponentName::try_new("deprov-rxn-q-1").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("deprov-rxn-src-1").expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .build()
    .expect("valid query configuration");
    let probe = LifecycleProbe::default();
    let deprov_flag = probe.delete_owned_resources_called.clone();

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("deprov-rxn-src-1").expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_query(query)
        .with_reaction_factory(
            drasi_core::ComponentName::try_new("deprov-rxn-1").expect("fixture component name"),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_core::ComponentName::try_new("deprov-rxn-q-1")
                    .expect("fixture component name"),
            )],
            true,
            move |id, inputs| {
                Ok(TestReaction::with_probe(
                    id,
                    inputs
                        .into_iter()
                        .map(drasi_lib::config::ComponentReference::by_id)
                        .collect(),
                    probe,
                ))
            },
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_reaction_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-rxn-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_reaction_by_name(
            &drasi_core::ComponentName::try_new("deprov-rxn-1").expect("fixture component name"),
            true,
        )
        .await
        .unwrap();

    assert!(
        deprov_flag.load(Ordering::SeqCst),
        "deprovision() should have been called with cleanup=true"
    );

    let _ = drasi.stop().await;
}

/// Removing a reaction with cleanup=false does NOT call deprovision.
#[tokio::test]
async fn test_reaction_remove_without_cleanup_skips_deprovision() {
    let query = Query::cypher(
        drasi_core::ComponentName::try_new("deprov-rxn-q-2").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("deprov-rxn-src-2").expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .build()
    .expect("valid query configuration");
    let probe = LifecycleProbe::default();
    let deprov_flag = probe.delete_owned_resources_called.clone();

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("deprov-rxn-src-2").expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_query(query)
        .with_reaction_factory(
            drasi_core::ComponentName::try_new("deprov-rxn-2").expect("fixture component name"),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_core::ComponentName::try_new("deprov-rxn-q-2")
                    .expect("fixture component name"),
            )],
            true,
            move |id, inputs| {
                Ok(TestReaction::with_probe(
                    id,
                    inputs
                        .into_iter()
                        .map(drasi_lib::config::ComponentReference::by_id)
                        .collect(),
                    probe,
                ))
            },
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_reaction_status(
        &drasi,
        &drasi_core::ComponentName::try_new("deprov-rxn-2").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_reaction_by_name(
            &drasi_core::ComponentName::try_new("deprov-rxn-2").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();

    assert!(
        !deprov_flag.load(Ordering::SeqCst),
        "deprovision() should NOT have been called with cleanup=false"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Source Reconfigure Tests
// ============================================================================

/// Updating a running source stops it, replaces the instance, and restarts it.
#[tokio::test]
async fn test_source_update_running_stops_and_restarts() {
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (source_id, initial_probe) =
        create_managed_source(&drasi, resolver.as_ref(), "reconfig-src-1", true, "initial").await;

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-src-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Should be running
    let status = drasi
        .get_source_status_by_name(
            &drasi_core::ComponentName::try_new("reconfig-src-1").expect("fixture component name"),
        )
        .await
        .unwrap();
    assert_eq!(status, ComponentStatus::Running);
    assert_eq!(initial_probe.start_count.load(Ordering::SeqCst), 1);

    let replacement_probe = resolver.register("replacement");

    drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-src-1").expect("fixture component name"),
            source_definition(source_id, "reconfig-src-1", true, "replacement"),
        )
        .await
        .unwrap();

    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-src-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Old instance was stopped
    assert_eq!(
        initial_probe.stop_count.load(Ordering::SeqCst),
        1,
        "should have stopped old instance"
    );
    // New instance was started
    assert_eq!(
        replacement_probe.start_count.load(Ordering::SeqCst),
        1,
        "should have started new instance"
    );

    // Should be running again
    let status = drasi
        .get_source_status_by_name(
            &drasi_core::ComponentName::try_new("reconfig-src-1").expect("fixture component name"),
        )
        .await
        .unwrap();
    assert_eq!(status, ComponentStatus::Running);

    let _ = drasi.stop().await;
}

/// Updating a stopped source replaces the instance without restart.
#[tokio::test]
async fn test_source_update_stopped_replaces_without_restart() {
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (source_id, _) =
        create_managed_source(&drasi, resolver.as_ref(), "reconfig-src-2", true, "initial").await;

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-src-2").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Stop the source
    drasi
        .stop_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-src-2").expect("fixture component name"),
        )
        .await
        .unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-src-2").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;

    let replacement_probe = resolver.register("replacement");

    drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-src-2").expect("fixture component name"),
            source_definition(source_id, "reconfig-src-2", true, "replacement"),
        )
        .await
        .unwrap();

    // New instance should NOT have been started (was stopped)
    assert_eq!(
        replacement_probe.start_count.load(Ordering::SeqCst),
        0,
        "should NOT restart a stopped source"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Reaction Reconfigure Tests
// ============================================================================

/// Updating a running reaction stops it, replaces the instance, and restarts it.
#[tokio::test]
async fn test_reaction_update_running_stops_and_restarts() {
    let query = Query::cypher(
        drasi_core::ComponentName::try_new("reconfig-rxn-q-1").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("reconfig-rxn-src-1").expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .build()
    .expect("valid query configuration");
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("reconfig-rxn-src-1")
                .expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_query(query)
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (reaction_id, initial_probe) = create_managed_reaction(
        &drasi,
        resolver.as_ref(),
        "reconfig-rxn-1",
        "reconfig-rxn-q-1",
        true,
        "initial",
    )
    .await;

    drasi.start().await.unwrap();
    wait_for_reaction_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-rxn-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let replacement_probe = resolver.register("replacement");

    drasi
        .update_reaction_by_name(
            &drasi_core::ComponentName::try_new("reconfig-rxn-1").expect("fixture component name"),
            reaction_definition(
                reaction_id,
                "reconfig-rxn-1",
                Some("reconfig-rxn-q-1"),
                true,
                "replacement",
            ),
        )
        .await
        .unwrap();

    wait_for_reaction_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-rxn-1").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    assert_eq!(
        initial_probe.stop_count.load(Ordering::SeqCst),
        1,
        "should have stopped old instance"
    );
    assert_eq!(
        replacement_probe.start_count.load(Ordering::SeqCst),
        1,
        "should have started new instance"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Lifecycle Event Streaming Tests
// ============================================================================

/// Verify that Reconfiguring event is emitted during source update.
#[tokio::test]
async fn test_source_update_emits_reconfiguring_event() {
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (source_id, _) = create_managed_source(
        &drasi,
        resolver.as_ref(),
        "reconfig-evt-src",
        true,
        "initial",
    )
    .await;

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-evt-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Subscribe to events before update
    let (_history, mut event_rx) = drasi
        .subscribe_source_events_by_name(
            &drasi_core::ComponentName::try_new("reconfig-evt-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

    // Drain any pending events from initial startup that may not have
    // been dispatched yet
    tokio::task::yield_now().await;
    while event_rx.try_recv().is_ok() {}

    resolver.register("replacement");

    drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-evt-src")
                .expect("fixture component name"),
            source_definition(source_id, "reconfig-evt-src", true, "replacement"),
        )
        .await
        .unwrap();

    // Collect events — expect Reconfiguring and Running
    let events = collect_events(&mut event_rx, ComponentStatus::Running, 3).await;
    let statuses: Vec<_> = events.iter().map(|e| &e.status).collect();
    println!("Source update events: {statuses:?}");

    assert!(
        events
            .iter()
            .any(|e| e.status == ComponentStatus::Reconfiguring),
        "Expected Reconfiguring event, got: {statuses:?}"
    );
    assert!(
        events.iter().any(|e| e.status == ComponentStatus::Running),
        "Expected Running event after reconfigure, got: {statuses:?}"
    );

    let _ = drasi.stop().await;
}

/// Verify that Reconfiguring event is emitted during reaction update.
#[tokio::test]
async fn test_reaction_update_emits_reconfiguring_event() {
    let query = Query::cypher(
        drasi_core::ComponentName::try_new("reconfig-evt-rxn-q").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("reconfig-evt-rxn-src").expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .build()
    .expect("valid query configuration");
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("reconfig-evt-rxn-src")
                .expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_query(query)
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (reaction_id, _) = create_managed_reaction(
        &drasi,
        resolver.as_ref(),
        "reconfig-evt-rxn",
        "reconfig-evt-rxn-q",
        true,
        "initial",
    )
    .await;

    drasi.start().await.unwrap();
    wait_for_reaction_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-evt-rxn").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let (_history, mut event_rx) = drasi
        .subscribe_reaction_events_by_name(
            &drasi_core::ComponentName::try_new("reconfig-evt-rxn")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

    // Drain any pending events from initial startup that may not have
    // been dispatched yet
    tokio::task::yield_now().await;
    while event_rx.try_recv().is_ok() {}

    resolver.register("replacement");

    drasi
        .update_reaction_by_name(
            &drasi_core::ComponentName::try_new("reconfig-evt-rxn")
                .expect("fixture component name"),
            reaction_definition(
                reaction_id,
                "reconfig-evt-rxn",
                Some("reconfig-evt-rxn-q"),
                true,
                "replacement",
            ),
        )
        .await
        .unwrap();

    let events = collect_events(&mut event_rx, ComponentStatus::Running, 3).await;
    let statuses: Vec<_> = events.iter().map(|e| &e.status).collect();
    println!("Reaction update events: {statuses:?}");

    assert!(
        events
            .iter()
            .any(|e| e.status == ComponentStatus::Reconfiguring),
        "Expected Reconfiguring event, got: {statuses:?}"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Log Preservation Tests
// ============================================================================

/// Source log history is preserved across a reconfigure cycle.
#[tokio::test]
async fn test_source_update_preserves_log_history() {
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (source_id, _) = create_managed_source(
        &drasi,
        resolver.as_ref(),
        "reconfig-log-src",
        true,
        "initial",
    )
    .await;

    // Subscribe to logs before starting to detect log arrival
    let (_, mut log_rx) = drasi
        .subscribe_source_logs_by_name(
            &drasi_core::ComponentName::try_new("reconfig-log-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-log-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Wait for at least one log to arrive (event-based instead of sleep)
    let _ = timeout(Duration::from_secs(5), log_rx.recv())
        .await
        .expect("Timed out waiting for source logs to arrive");

    // Capture pre-update log count
    let (pre_logs, _) = drasi
        .subscribe_source_logs_by_name(
            &drasi_core::ComponentName::try_new("reconfig-log-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
    let pre_count = pre_logs.len();
    assert!(pre_count > 0, "Expected logs before update");

    // Now update
    resolver.register("replacement");
    drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-log-src")
                .expect("fixture component name"),
            source_definition(source_id, "reconfig-log-src", true, "replacement"),
        )
        .await
        .unwrap();

    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("reconfig-log-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Post-update logs should include pre-update logs plus new ones
    let (post_logs, _) = drasi
        .subscribe_source_logs_by_name(
            &drasi_core::ComponentName::try_new("reconfig-log-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
    assert!(
        post_logs.len() >= pre_count,
        "Log history should be preserved: pre={pre_count}, post={}",
        post_logs.len()
    );

    let _ = drasi.stop().await;
}

/// Active log subscribers continue receiving logs through a reconfigure.
#[tokio::test]
async fn test_source_update_active_log_stream_continues() {
    let resolver = Arc::new(TestOrdinaryResolver::default());
    let drasi = lib_integration_tests::query_builder()
        .with_ordinary_component_resolver(resolver.clone())
        .build()
        .await
        .unwrap();
    let (source_id, _) = create_managed_source(
        &drasi,
        resolver.as_ref(),
        "reconfig-stream-src",
        true,
        "initial",
    )
    .await;

    // Subscribe to logs before start
    let (_initial, mut log_rx) = drasi
        .subscribe_source_logs_by_name(
            &drasi_core::ComponentName::try_new("reconfig-stream-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

    drasi.start().await.unwrap();

    // Collect initial startup logs
    let mut pre_logs = Vec::new();
    let _ = timeout(Duration::from_secs(1), async {
        while let Ok(log) = log_rx.recv().await {
            pre_logs.push(log);
            if pre_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;
    assert!(!pre_logs.is_empty(), "Should receive startup logs");

    // Now reconfigure
    resolver.register("replacement");
    drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("reconfig-stream-src")
                .expect("fixture component name"),
            source_definition(source_id, "reconfig-stream-src", true, "replacement"),
        )
        .await
        .unwrap();

    // The same subscriber should receive new logs from the restart
    let mut post_logs = Vec::new();
    let _ = timeout(Duration::from_secs(2), async {
        while let Ok(log) = log_rx.recv().await {
            post_logs.push(log);
            if post_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;

    assert!(
        !post_logs.is_empty(),
        "Active log subscriber should continue receiving logs after reconfigure"
    );

    let _ = drasi.stop().await;
}

// ============================================================================
// Edge Cases
// ============================================================================

/// Removing a non-existent source returns an error.
#[tokio::test]
async fn test_remove_nonexistent_source_fails() {
    let drasi = lib_integration_tests::query_builder()
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("does-not-exist").expect("fixture component name"),
            true,
        )
        .await;
    assert!(result.is_err());

    let _ = drasi.stop().await;
}

/// Updating a non-existent source returns an error.
#[tokio::test]
async fn test_update_nonexistent_source_fails() {
    let drasi = lib_integration_tests::query_builder()
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .update_source_by_name(
            &drasi_core::ComponentName::try_new("does-not-exist").expect("fixture component name"),
            source_definition(ComponentId::generate(), "does-not-exist", true, "missing"),
        )
        .await;
    assert!(result.is_err());

    let _ = drasi.stop().await;
}

/// Removing a non-existent reaction returns an error.
#[tokio::test]
async fn test_remove_nonexistent_reaction_fails() {
    let drasi = lib_integration_tests::query_builder()
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .remove_reaction_by_name(
            &drasi_core::ComponentName::try_new("does-not-exist").expect("fixture component name"),
            true,
        )
        .await;
    assert!(result.is_err());

    let _ = drasi.stop().await;
}

/// Updating a non-existent reaction returns an error.
#[tokio::test]
async fn test_update_nonexistent_reaction_fails() {
    let drasi = lib_integration_tests::query_builder()
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();

    let result = drasi
        .update_reaction_by_name(
            &drasi_core::ComponentName::try_new("does-not-exist").expect("fixture component name"),
            reaction_definition(
                ComponentId::generate(),
                "does-not-exist",
                None,
                true,
                "missing",
            ),
        )
        .await;
    assert!(result.is_err());

    let _ = drasi.stop().await;
}

// ============================================================================
// State Store Cleanup Tests
// ============================================================================

/// Removing a source with cleanup=true clears its state store partition.
#[tokio::test]
async fn test_source_remove_with_cleanup_clears_state_store() {
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("cleanup-src").expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_state_store_provider(state_store.clone() as Arc<dyn StateStoreProvider>)
        .build()
        .await
        .unwrap();
    let source_id = drasi
        .component_by_name(
            &drasi_core::ComponentName::try_new("cleanup-src").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let source_partition = drasi_core::interface::StorePartition::component(source_id);

    // Pre-populate state store with data for this source UUID.
    state_store
        .set(&source_partition, "key1", b"value1".to_vec())
        .await
        .unwrap();
    state_store
        .set(&source_partition, "key2", b"value2".to_vec())
        .await
        .unwrap();

    let keys = state_store.list_keys(&source_partition).await.unwrap();
    assert_eq!(
        keys.len(),
        2,
        "State store should have 2 keys before delete"
    );

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("cleanup-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("cleanup-src").expect("fixture component name"),
            true,
        )
        .await
        .unwrap();

    let keys = state_store.list_keys(&source_partition).await.unwrap();
    assert!(
        keys.is_empty(),
        "State store should be empty after delete with cleanup=true, got {} keys",
        keys.len()
    );

    let _ = drasi.stop().await;
}

/// Removing a source with cleanup=false preserves its state store partition.
#[tokio::test]
async fn test_source_remove_without_cleanup_preserves_state_store() {
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    let drasi = lib_integration_tests::query_builder()
        .with_source_factory(
            drasi_core::ComponentName::try_new("no-cleanup-src").expect("fixture component name"),
            true,
            |id| TestSource::new(id.clone()),
        )
        .with_state_store_provider(state_store.clone() as Arc<dyn StateStoreProvider>)
        .build()
        .await
        .unwrap();
    let source_id = drasi
        .component_by_name(
            &drasi_core::ComponentName::try_new("no-cleanup-src").expect("fixture component name"),
        )
        .unwrap()
        .id()
        .clone();
    let source_partition = drasi_core::interface::StorePartition::component(source_id);

    // Pre-populate state store with data for this source UUID.
    state_store
        .set(&source_partition, "key1", b"value1".to_vec())
        .await
        .unwrap();
    state_store
        .set(&source_partition, "key2", b"value2".to_vec())
        .await
        .unwrap();

    drasi.start().await.unwrap();
    wait_for_source_status(
        &drasi,
        &drasi_core::ComponentName::try_new("no-cleanup-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    drasi
        .remove_source_by_name(
            &drasi_core::ComponentName::try_new("no-cleanup-src").expect("fixture component name"),
            false,
        )
        .await
        .unwrap();

    let keys = state_store.list_keys(&source_partition).await.unwrap();
    assert_eq!(
        keys.len(),
        2,
        "State store should still have 2 keys after delete with cleanup=false"
    );

    let _ = drasi.stop().await;
}
