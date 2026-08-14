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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::reactions::{
    ManagerCheckpointOwnership, ReactionBase, ReactionBaseParams, ReactionCheckpoint,
};
use drasi_lib::{DrasiLib, Query, Reaction, ReactionRuntimeContext, StateStoreProvider};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use tokio::sync::mpsc;

const SOURCE_ID: &str = "restart-source";
const QUERY_ID: &str = "q1";
const REACTION_ID: &str = "durable-recorder";
const OUTPUT_SEQUENCE_STORE_ID: &str = "__drasi_query_output_sequences";

struct DurableRecordingReaction {
    base: Arc<ReactionBase>,
    delivered: mpsc::UnboundedSender<QueryResult>,
}

impl DurableRecordingReaction {
    fn new() -> (Self, mpsc::UnboundedReceiver<QueryResult>) {
        let (delivered, receiver) = mpsc::unbounded_channel();
        let base = ReactionBase::new(ReactionBaseParams::new(
            REACTION_ID,
            vec![QUERY_ID.to_string()],
        ));
        (
            Self {
                base: Arc::new(base),
                delivered,
            },
            receiver,
        )
    }
}

#[async_trait]
impl Reaction for DurableRecordingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "durable-recording"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        true
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        let mut shutdown = self.base.create_shutdown_channel().await;
        let base = self.base.clone();
        let delivered = self.delivered.clone();
        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    event = base.priority_queue.dequeue() => event,
                };
                let existing = base
                    .read_checkpoint(&event.query_id)
                    .await
                    .expect("read durable reaction checkpoint");
                if existing
                    .as_ref()
                    .is_some_and(|checkpoint| event.sequence <= checkpoint.sequence)
                {
                    continue;
                }

                delivered
                    .send(event.as_ref().clone())
                    .expect("record delivered result");
                base.write_checkpoint(
                    &event.query_id,
                    &ReactionCheckpoint {
                        sequence: event.sequence,
                        config_hash: existing.map_or(0, |checkpoint| checkpoint.config_hash),
                    },
                )
                .await
                .expect("persist acknowledged reaction checkpoint");
            }
        });
        self.base.set_processing_task(task).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn checkpoint_ownership(&self) -> ManagerCheckpointOwnership {
        ManagerCheckpointOwnership::Reaction
    }
}

fn query(cypher: &str) -> drasi_lib::QueryConfig {
    Query::cypher(QUERY_ID)
        .query(cypher)
        .from_source(SOURCE_ID)
        .with_outbox_capacity(32)
        .auto_start(true)
        .build()
}

async fn build_run(
    store: Arc<dyn StateStoreProvider>,
    cypher: &str,
) -> Result<(
    DrasiLib,
    ApplicationSourceHandle,
    mpsc::UnboundedReceiver<QueryResult>,
)> {
    let (source, source_handle) = ApplicationSource::new(
        SOURCE_ID,
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?;
    let (reaction, delivered) = DurableRecordingReaction::new();
    let core = DrasiLib::builder()
        .with_id("restart-sequence-redb")
        .with_source(source)
        .with_query(query(cypher))
        .with_reaction(reaction)
        .with_state_store_provider(store.clone())
        .build()
        .await?;
    Ok((core, source_handle, delivered))
}

async fn insert_person(source: &ApplicationSourceHandle, id: &str, name: &str) -> Result<()> {
    source
        .send_node_insert(
            id,
            vec!["Person"],
            PropertyMapBuilder::new().with_string("name", name).build(),
        )
        .await
}

async fn receive(delivered: &mut mpsc::UnboundedReceiver<QueryResult>) -> Result<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), delivered.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("recording reaction closed"))
}

async fn wait_for_checkpoint(
    store: &dyn StateStoreProvider,
    sequence: u64,
) -> Result<ReactionCheckpoint> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(bytes) = store
                .get(REACTION_ID, &format!("checkpoint:{QUERY_ID}"))
                .await
                .expect("read redb checkpoint")
            {
                let checkpoint: ReactionCheckpoint =
                    bincode::deserialize(&bytes).expect("decode reaction checkpoint");
                if checkpoint.sequence >= sequence {
                    return checkpoint;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(Into::into)
}

async fn wait_until_stopped(core: &DrasiLib) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if core
                .list_reactions()
                .await
                .expect("list reactions")
                .iter()
                .any(|(id, status)| id == REACTION_ID && *status == ComponentStatus::Stopped)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn durable_redb_restart_rebases_legacy_checkpoint_and_delivers_once() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let db_path = directory.path().join("reaction-state.redb");
    let store: Arc<dyn StateStoreProvider> = Arc::new(RedbStateStoreProvider::new(&db_path)?);
    let cypher = "MATCH (p:Person) RETURN p.name AS name";

    // Run 1 advances a real reaction-owned checkpoint. Remove only the new
    // query clock to leave the exact legacy shape written before this fix.
    {
        let (core, source, mut delivered) = build_run(store.clone(), cypher).await?;
        core.start().await?;
        for (id, name) in [("p1", "Alice"), ("p2", "Bob"), ("p3", "Charlie")] {
            insert_person(&source, id, name).await?;
            receive(&mut delivered).await?;
        }
        let checkpoint = wait_for_checkpoint(store.as_ref(), 3).await?;
        assert_eq!(checkpoint.sequence, 3);
        assert_ne!(checkpoint.config_hash, 0);
        store.delete(OUTPUT_SEQUENCE_STORE_ID, QUERY_ID).await?;
        core.shutdown().await?;
    }

    // A new DrasiLib reconstructs QueryOutputState at raw sequence zero. The
    // legacy reaction checkpoint seeds the shared query clock, so the first
    // genuinely new result is 4 rather than being discarded as raw sequence 1.
    {
        let (core, source, mut delivered) = build_run(store.clone(), cypher).await?;
        core.start().await?;
        insert_person(&source, "p4", "Diana").await?;
        let result = receive(&mut delivered).await?;
        assert_eq!(result.sequence, 4);
        wait_for_checkpoint(store.as_ref(), 4).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            delivered.try_recv().is_err(),
            "the run-2 diff must be delivered exactly once"
        );

        // Restarting the reaction in the same process replays its outbox
        // position, but the acknowledged sequence remains suppressed.
        core.stop_reaction(REACTION_ID).await?;
        wait_until_stopped(&core).await?;
        core.start_reaction(REACTION_ID).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            delivered.try_recv().is_err(),
            "acknowledged outbox replay must remain suppressed"
        );

        insert_person(&source, "p5", "Eve").await?;
        let next = receive(&mut delivered).await?;
        assert_eq!(next.sequence, 5);
        wait_for_checkpoint(store.as_ref(), 5).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(delivered.try_recv().is_err());
        core.shutdown().await?;
    }

    // The sequence migration is independent of config validation: Strict still
    // rejects a changed query, and no stale result is delivered as a side effect.
    {
        let changed = "MATCH (p:Person) WHERE p.name IS NOT NULL RETURN p.name AS name";
        let (core, _source, mut delivered) = build_run(store.clone(), changed).await?;
        let error = core
            .start()
            .await
            .expect_err("Strict recovery must reject a config-hash mismatch");
        assert!(error.to_string().contains("Strict recovery policy"));
        assert!(delivered.try_recv().is_err());
        assert_eq!(wait_for_checkpoint(store.as_ref(), 5).await?.sequence, 5);
        let _ = core.shutdown().await;
    }

    Ok(())
}
