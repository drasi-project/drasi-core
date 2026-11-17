// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::Routine,
    store::{Run, Store},
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::{
    channels::{ComponentStatus, QueryResult, ResultDiff},
    context::ReactionRuntimeContext,
    reactions::{
        common::base::{ReactionBase, ReactionBaseParams},
        SnapshotFetcher,
    },
    Reaction,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock};

#[derive(Default)]
pub struct Gate {
    pub enabled: bool,
    pub cutoff: u64,
}

#[derive(Clone)]
pub struct Handle {
    pub gate: Arc<Mutex<Gate>>,
    pub fetcher: Arc<RwLock<Option<Arc<dyn SnapshotFetcher>>>>,
    pub query_id: String,
    pub epoch: String,
    pub changes: Arc<Mutex<Vec<Value>>>,
}

impl Handle {
    pub async fn snapshot(&self) -> Result<Value> {
        let fetcher = self
            .fetcher
            .read()
            .await
            .clone()
            .context("Query snapshot service is unavailable")?;
        let snapshot = fetcher.fetch_snapshot(&self.query_id).await?;
        let sequence = snapshot.as_of_sequence.to_string();
        let rows = snapshot.collect_vec().await;
        Ok(
            json!({"epoch":self.epoch,"sequence":sequence,"rows":rows,"changes":self.changes.lock().await.clone()}),
        )
    }
}

pub struct FoundryReaction {
    base: ReactionBase,
    routine: Routine,
    store: Arc<Store>,
    pub handle: Handle,
}

impl FoundryReaction {
    pub fn new(
        id: String,
        query_id: String,
        routine: Routine,
        store: Arc<Store>,
        enabled: bool,
    ) -> Self {
        Self {
            base: ReactionBase::new(
                ReactionBaseParams::new(id, vec![query_id.clone()]).with_auto_start(false),
            ),
            routine,
            store,
            handle: Handle {
                gate: Arc::new(Mutex::new(Gate { enabled, cutoff: 0 })),
                fetcher: Arc::new(RwLock::new(None)),
                query_id,
                epoch: uuid::Uuid::new_v4().to_string(),
                changes: Arc::new(Mutex::new(Vec::new())),
            },
        }
    }
}

pub async fn admit(
    store: &Store,
    routine: &Routine,
    handle: &Handle,
    event: &QueryResult,
) -> Result<()> {
    let gate = handle.gate.lock().await;
    if event.sequence <= gate.cutoff {
        return Ok(());
    }
    let mut changes = Vec::new();
    let mut jobs = Vec::new();
    for (index, diff) in event.results.iter().enumerate() {
        let (operation, before, after, signature) = match diff {
            ResultDiff::Add {
                data,
                row_signature,
            } => ("added", Value::Null, data.clone(), *row_signature),
            ResultDiff::Update {
                before,
                after,
                row_signature,
                ..
            } => ("updated", before.clone(), after.clone(), *row_signature),
            ResultDiff::Delete {
                data,
                row_signature,
            } => ("removed", data.clone(), Value::Null, *row_signature),
            ResultDiff::Aggregation {
                before,
                after,
                row_signature,
            } => (
                if before.is_some() { "updated" } else { "added" },
                before.clone().unwrap_or(Value::Null),
                after.clone(),
                *row_signature,
            ),
            ResultDiff::Noop => continue,
        };
        let row = if operation == "removed" {
            &before
        } else {
            &after
        };
        let key = routine.key(row);
        let change = json!({"routineId":routine.id,"routineRevision":routine.revision,"queryId":event.query_id,"queryEpoch":handle.epoch,"sequence":event.sequence.to_string(),"diffIndex":index,"operation":operation,"rowSignature":signature.to_string(),"resultKey":key.as_ref().ok(),"before":before,"after":after,"occurredAt":event.timestamp});
        changes.push(change.clone());
        if !gate.enabled || !routine.operations.iter().any(|op| op == operation) {
            continue;
        }
        let tuple = serde_json::to_vec(&json!([
            routine.id,
            routine.revision,
            handle.epoch,
            event.sequence.to_string(),
            index
        ]))?;
        let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, &tuple).to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let key_error = key.err().map(|e| e.to_string());
        jobs.push(Run {
            id,
            routine_id: routine.id.clone(),
            agent: routine.agent.clone(),
            input: json!({"schemaVersion":1,"instructions":routine.prompt,"event":change}),
            status: if key_error.is_some() {
                "Failed".into()
            } else {
                "Queued".into()
            },
            response: None,
            error: key_error,
            created_at: now.clone(),
            updated_at: now,
            attempts: 0,
        });
    }
    store
        .update(|records| {
            let pending = records
                .runs
                .values()
                .filter(|r| {
                    r.routine_id == routine.id
                        && ["Queued", "Submitting", "Running"].contains(&r.status.as_str())
                })
                .count();
            let new_jobs = jobs
                .iter()
                .filter(|r| !records.runs.contains_key(&r.id))
                .count();
            ensure!(
                pending + new_jobs <= 100,
                "Pending run limit reached; pause and reconcile before resuming"
            );
            for job in jobs {
                records.runs.entry(job.id.clone()).or_insert(job);
            }
            Ok(())
        })
        .await?;
    let mut timeline = handle.changes.lock().await;
    timeline.extend(changes);
    let excess = timeline.len().saturating_sub(100);
    timeline.drain(..excess);
    let _ = store.events.send(json!({"type":"refresh"}));
    Ok(())
}

#[async_trait]
impl Reaction for FoundryReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "foundry-demo"
    }
    fn properties(&self) -> HashMap<String, Value> {
        self.base.properties_or_serialize(&self.routine)
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        false
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        *self.handle.fetcher.write().await = context.snapshot_fetcher.clone();
        self.base.initialize(context).await;
    }
    async fn start(&self) -> Result<()> {
        let base = self.base.clone_shared();
        let store = self.store.clone();
        let routine = self.routine.clone();
        let handle = self.handle.clone();
        let shutdown = self.base.create_shutdown_channel().await;
        let task = tokio::spawn(async move {
            let policy = drasi_lib::recovery::ReactionRecoveryPolicy::Strict;
            let result = base
                .run_standard_loop(shutdown, HashMap::new(), policy, |event| {
                    let store = store.clone();
                    let routine = routine.clone();
                    let handle = handle.clone();
                    async move { admit(&store, &routine, &handle, &event).await }
                })
                .await;
            if let Err(error) = result {
                log::error!("Foundry reaction stopped: {error:#}");
                base.set_status(ComponentStatus::Error, Some(error.to_string()))
                    .await;
                if let Err(e) = store
                    .update(|r| {
                        if let Some(saved) = r.routines.get_mut(&routine.id) {
                            saved.status = "Error".into();
                            saved.error = Some(error.to_string());
                        }
                        Ok(())
                    })
                    .await
                {
                    log::error!("Cannot persist reaction failure: {e:#}");
                }
            }
        });
        self.base.set_processing_task(task).await;
        self.base.set_status(ComponentStatus::Running, None).await;
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
    // The query is volatile. Durability applies to admitted jobs, not source replay.
    fn is_durable(&self) -> bool {
        false
    }
}
