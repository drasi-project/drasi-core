// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    channels::{
        BootstrapEvent, ChangeDispatcher, ChannelChangeDispatcher, ComponentStatusHandle,
        QueryResult, SourceEvent, SourceEventWrapper,
    },
    reactions::ReactionCheckpoint,
    ComponentStatus, DrasiLib, Query, Reaction, ReactionBase, ReactionBaseParams,
    ReactionRuntimeContext, Source, SourceRuntimeContext, SourceSubscriptionSettings,
    SubscriptionResponse,
};
use tokio::sync::{mpsc, Mutex};

#[derive(Clone)]
struct OptionalSnapshotSource {
    status: ComponentStatusHandle,
    dispatchers: Arc<Mutex<Vec<ChannelChangeDispatcher<SourceEventWrapper>>>>,
    sequence: Arc<AtomicU64>,
    snapshot: Option<Arc<AtomicU64>>,
}

impl OptionalSnapshotSource {
    fn new(snapshot: Option<Arc<AtomicU64>>) -> Self {
        Self {
            status: ComponentStatusHandle::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
            ),
            dispatchers: Arc::new(Mutex::new(Vec::new())),
            sequence: Arc::new(AtomicU64::new(1)),
            snapshot,
        }
    }

    async fn emit(&self, value: u64) -> Result<()> {
        anyhow::ensure!(self.status.get_status().await == ComponentStatus::Running);
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let event = Arc::new(SourceEventWrapper::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            SourceEvent::Change(person(value, sequence)),
            chrono::DateTime::from_timestamp_millis(sequence as i64).expect("time"),
            sequence,
        ));
        for dispatcher in self.dispatchers.lock().await.iter() {
            dispatcher.dispatch_change(event.clone()).await?;
        }
        Ok(())
    }
}

fn person(value: u64, sequence: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &value.to_string()),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: sequence,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"value":value})),
        },
    }
}

#[async_trait]
impl Source for OptionalSnapshotSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
        static ID: std::sync::LazyLock<drasi_core_types::ComponentId> =
            std::sync::LazyLock::new(|| {
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID")
            });
        &ID
    }
    fn type_name(&self) -> &str {
        "optional-snapshot"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn supports_replay(&self) -> bool {
        false
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.status.wire(context.update_tx).await;
    }
    async fn start(&self) -> Result<()> {
        self.status.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.dispatchers.lock().await.clear();
        self.status.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let dispatcher = ChannelChangeDispatcher::new(16);
        let receiver = dispatcher.create_receiver().await?;
        self.dispatchers.lock().await.push(dispatcher);
        let bootstrap_receiver =
            if let Some(value) = self.snapshot.as_ref().filter(|_| settings.enable_bootstrap) {
                let (sender, receiver) = mpsc::channel(1);
                sender
                    .send(BootstrapEvent {
                        source_id: drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d",
                        )
                        .expect("fixture UUID"),
                        change: person(value.load(Ordering::Acquire), 0),
                        timestamp: chrono::Utc::now(),
                        sequence: 0,
                    })
                    .await?;
                Some(receiver)
            } else {
                None
            };
        Ok(SubscriptionResponse {
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000004d",
            )
            .expect("fixture UUID"),
            query_id: settings.query_id,
            receiver,
            bootstrap_receiver,
            bootstrap_result_receiver: None,
            position_handle: None,
        })
    }
}

struct RecordingReaction {
    base: ReactionBase,
    results: mpsc::UnboundedSender<QueryResult>,
}

#[async_trait]
impl Reaction for RecordingReaction {
    fn id(&self) -> &drasi_core_types::ComponentId {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "recording"
    }
    fn query_inputs(&self) -> Vec<drasi_computation_contracts::ComponentReference> {
        self.base.query_inputs().to_vec()
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }
    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        let checkpoint = self
            .base
            .read_checkpoint(&result.query_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("reaction startup did not seed progress"))?;
        anyhow::ensure!(
            result.sequence > checkpoint.sequence,
            "already handled output replayed"
        );
        self.base
            .write_checkpoint(
                &result.query_id,
                &ReactionCheckpoint {
                    sequence: result.sequence,
                    config_hash: checkpoint.config_hash,
                },
            )
            .await?;
        self.results.send(result)?;
        Ok(())
    }
}

async fn ready(core: &DrasiLib, component: &drasi_core_types::ComponentName) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        core.component_by_name(component)?.wait_started(),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn no_snapshot_source_keeps_committed_query_state_and_strict_reaction_progress_on_restart(
) -> Result<()> {
    let source = OptionalSnapshotSource::new(None);
    let (sender, mut received) = mpsc::unbounded_channel();
    let state = Arc::new(drasi_lib::MemoryStateStoreProvider::new());
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("no-snapshot-restart")
        .with_source(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            source.clone(),
        )
        .with_query(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )
            .query("MATCH (p:Person) RETURN p.value AS value")
            .from_input_by_name(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            )
            .build()?,
        )
        .with_reaction(
            drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
            RecordingReaction {
                base: ReactionBase::new(ReactionBaseParams::new(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000000ee")
                        .expect("fixture UUID"),
                    vec![drasi_computation_contracts::ComponentReference::by_name(
                        drasi_core_types::ComponentName::try_new("query")
                            .expect("fixture component name"),
                    )],
                )),
                results: sender,
            },
        )
        .with_state_store_provider(state)
        .build()
        .await?;
    core.start().await?;
    ready(
        &core,
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await?;
    source.emit(1).await?;
    let first = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("reaction stopped"))?;
    assert_eq!(first.sequence, 1);
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let before = query.fetch_snapshot().await?;
    assert_eq!(before.to_vec(), [serde_json::json!({"value":1})]);
    core.stop().await?;
    core.start().await?;
    ready(
        &core,
        &drasi_core_types::ComponentName::try_new("reaction").expect("fixture component name"),
    )
    .await?;
    let restored = query.fetch_snapshot().await?;
    assert_eq!(restored.output_generation, before.output_generation);
    assert_eq!(restored.as_of_sequence, before.as_of_sequence);
    assert_eq!(restored.to_vec(), before.to_vec());
    assert!(
        received.try_recv().is_err(),
        "acknowledged result must not be replayed"
    );
    source.emit(2).await?;
    let second = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("reaction stopped"))?;
    assert_eq!(second.sequence, 2);
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(snapshot.to_vec().len(), 2);
    assert_eq!(snapshot.output_generation, before.output_generation);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn real_volatile_snapshot_still_refreshes_state_and_advances_reset_generation() -> Result<()>
{
    let snapshot_value = Arc::new(AtomicU64::new(1));
    let source = OptionalSnapshotSource::new(Some(snapshot_value.clone()));
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("actual-snapshot-restart")
        .with_source(
            drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .with_query(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )
            .query("MATCH (p:Person) RETURN p.value AS value")
            .from_input_by_name(
                drasi_core_types::ComponentName::try_new("source").expect("fixture component name"),
            )
            .build()?,
        )
        .build()
        .await?;
    core.start().await?;
    ready(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let before = query.fetch_snapshot().await?;
    assert_eq!(before.to_vec(), [serde_json::json!({"value":1})]);
    core.stop().await?;
    snapshot_value.store(2, Ordering::Release);
    core.start().await?;
    ready(
        &core,
        &drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
    )
    .await?;
    let after = query.fetch_snapshot().await?;
    assert_eq!(after.to_vec(), [serde_json::json!({"value":2})]);
    assert!(after.output_generation > before.output_generation);
    core.shutdown().await?;
    Ok(())
}
