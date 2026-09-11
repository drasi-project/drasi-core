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

use std::{collections::HashMap, sync::Arc, time::Instant};

use anyhow::{ensure, Context, Result};
use drasi_core::interface::{
    CheckpointStore, CommitReceipt, LiveResultsWriter, OutboxWriter, RootId, RowMutation,
    SessionGuard,
};
use tokio::sync::RwLock;

use crate::{
    channels::{ChangeDispatcher, QueryResult, ResultDiff},
    metrics::QueryOutputMetrics,
    profiling::ProfilingMetadata,
};

use super::QueryOutputState;

pub(super) struct OutputDelivery {
    pub query_id: String,
    pub state: Arc<RwLock<QueryOutputState>>,
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    pub outbox_writer: Option<Arc<dyn OutboxWriter>>,
    pub live_results_writer: Option<Arc<dyn LiveResultsWriter>>,
    pub checkpoint_store: Arc<dyn CheckpointStore>,
    pub metrics: Arc<QueryOutputMetrics>,
}

pub(super) struct StagedOutput {
    root_id: RootId,
    result: QueryResult,
    transaction_started: Instant,
}

pub(super) struct StagedBootstrapOutput {
    root_id: RootId,
    diffs: Vec<ResultDiff>,
}

fn serialize_rows(diffs: &[ResultDiff]) -> Result<Vec<(u64, Option<Vec<u8>>)>> {
    let mut rows = Vec::new();
    for diff in diffs {
        let (signature, value) = match diff {
            ResultDiff::Add {
                data,
                row_signature,
            } => (*row_signature, Some(data)),
            ResultDiff::Update {
                after,
                row_signature,
                ..
            }
            | ResultDiff::Aggregation {
                after,
                row_signature,
                ..
            } => (*row_signature, Some(after)),
            ResultDiff::Delete { row_signature, .. } => (*row_signature, None),
            ResultDiff::Noop => continue,
        };
        rows.push((
            signature,
            value
                .map(rmp_serde::to_vec)
                .transpose()
                .context("Failed to serialize a live result row")?,
        ));
    }
    Ok(rows)
}

impl OutputDelivery {
    pub async fn stage(
        &self,
        mut diffs: Vec<ResultDiff>,
        source_id: &str,
        profiling: ProfilingMetadata,
        root: &SessionGuard,
        transaction_started: Instant,
    ) -> Result<Option<StagedOutput>> {
        diffs.retain(|diff| !matches!(diff, ResultDiff::Noop));
        if diffs.is_empty() {
            return Ok(None);
        }
        let sequence = self
            .state
            .read()
            .await
            .as_of_sequence()
            .checked_add(1)
            .context("Query result sequence is exhausted")?;
        let metadata = HashMap::from([
            ("source_id".into(), serde_json::json!(source_id)),
            ("processed_by".into(), serde_json::json!("drasi-core")),
            ("result_count".into(), serde_json::json!(diffs.len())),
        ]);
        let result = QueryResult::with_profiling(
            self.query_id.clone(),
            sequence,
            chrono::Utc::now(),
            diffs,
            metadata,
            profiling,
        );

        let outbox_data = self
            .outbox_writer
            .as_ref()
            .map(|_| rmp_serde::to_vec(&result))
            .transpose()
            .context("Failed to serialize query output")?;
        let rows = if self.live_results_writer.is_some() {
            serialize_rows(&result.results)?
        } else {
            Vec::new()
        };

        root.mark_dirty()
            .context("Query output root is not active")?;
        if let (Some(writer), Some(data)) = (&self.outbox_writer, outbox_data) {
            writer
                .append(&self.query_id, sequence, &data)
                .await
                .context("Failed to stage query outbox output")?;
        }
        self.write_rows(&rows).await?;
        self.checkpoint_store
            .write_result_sequence(&self.query_id, sequence)
            .await
            .context("Failed to stage the query result sequence")?;
        Ok(Some(StagedOutput {
            root_id: root.root().id(),
            result,
            transaction_started,
        }))
    }

    async fn write_rows(&self, rows: &[(u64, Option<Vec<u8>>)]) -> Result<()> {
        if let Some(writer) = &self.live_results_writer {
            let mutations: Vec<_> = rows
                .iter()
                .map(|(row_signature, data)| RowMutation {
                    row_signature: *row_signature,
                    data: data.as_deref(),
                })
                .collect();
            writer
                .apply_mutations(&self.query_id, &mutations)
                .await
                .context("Failed to stage live query results")?;
        }
        Ok(())
    }

    pub async fn stage_bootstrap(
        &self,
        diffs: Vec<ResultDiff>,
        root: &SessionGuard,
    ) -> Result<StagedBootstrapOutput> {
        if self.live_results_writer.is_some() {
            let rows = serialize_rows(&diffs)?;
            root.mark_dirty()
                .context("Bootstrap output root is not active")?;
            self.write_rows(&rows).await?;
        }
        Ok(StagedBootstrapOutput {
            root_id: root.root().id(),
            diffs,
        })
    }

    pub async fn publish_bootstrap(
        &self,
        staged: StagedBootstrapOutput,
        receipt: &CommitReceipt,
    ) -> Result<()> {
        ensure!(
            staged.root_id == receipt.root_id(),
            "Bootstrap output belongs to a different transaction root"
        );
        let mut state = self.state.write().await;
        state.apply_diffs(&staged.diffs);
        self.metrics.record_live_results_count(state.results_len());
        Ok(())
    }

    pub async fn publish(&self, staged: StagedOutput, receipt: &CommitReceipt) -> Result<()> {
        ensure!(
            staged.root_id == receipt.root_id(),
            "Query output belongs to a different transaction root"
        );
        let (result, capacity) = {
            let mut state = self.state.write().await;
            ensure!(
                state.as_of_sequence().checked_add(1) == Some(staged.result.sequence),
                "Query output sequence changed before publication"
            );
            state.apply_diffs(&staged.result.results);
            let result = state.advance_sequence_and_push(staged.result);
            self.metrics.record_transaction_duration_ns(
                u64::try_from(staged.transaction_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            );
            self.metrics.record_seq_advance();
            self.metrics.record_live_results_count(state.results_len());
            self.metrics.update_outbox(
                state.outbox_len(),
                state.outbox_earliest_seq().unwrap_or(0),
                state.as_of_sequence(),
            );
            (result, state.outbox_capacity())
        };
        for dispatcher in self.dispatchers.read().await.iter() {
            if let Err(error) = dispatcher.dispatch_change(result.clone()).await {
                log::debug!(
                    "Query '{}' could not dispatch committed output: {error}",
                    self.query_id
                );
            }
        }
        if let Some(writer) = &self.outbox_writer {
            if let Err(error) = writer.trim_to_capacity(&self.query_id, capacity).await {
                log::warn!(
                    "Query '{}' could not trim its committed outbox: {error}",
                    self.query_id
                );
            }
        }
        Ok(())
    }

    pub async fn restore(&self) -> Result<()> {
        let sequence = self
            .checkpoint_store
            .read_result_sequence(&self.query_id)
            .await
            .context("Failed to read the persisted result sequence")?
            .unwrap_or(0);
        let Some(writer) = &self.live_results_writer else {
            ensure!(
                sequence == self.state.read().await.as_of_sequence(),
                "The persisted result sequence has no recoverable snapshot"
            );
            return Ok(());
        };
        let rows = writer
            .read_snapshot(&self.query_id)
            .await
            .context("Failed to read the persisted query snapshot")?;
        let rows = rows
            .into_iter()
            .map(|(signature, data)| {
                rmp_serde::from_slice(&data)
                    .map(|value| (signature, value))
                    .context("Persisted query snapshot contains an invalid row")
            })
            .collect::<Result<Vec<_>>>()?;
        let capacity = self.state.read().await.outbox_capacity();
        let mut outbox = Vec::new();
        if let Some(writer) = &self.outbox_writer {
            let after = sequence.saturating_sub(u64::try_from(capacity).unwrap_or(u64::MAX));
            for (stored_sequence, bytes) in writer
                .read_from(&self.query_id, after)
                .await
                .context("Failed to read the persisted query outbox")?
            {
                let result: QueryResult = rmp_serde::from_slice(&bytes)
                    .context("Persisted query outbox contains an invalid result")?;
                ensure!(
                    result.query_id == self.query_id && result.sequence == stored_sequence,
                    "Persisted query outbox identity does not match its storage key"
                );
                outbox.push(result);
            }
            ensure!(
                sequence == 0 || outbox.last().map(|result| result.sequence) == Some(sequence),
                "Persisted query outbox does not reach its result sequence; rebuild or replay is required"
            );
        }
        let restored = QueryOutputState::from_persisted(rows, sequence, outbox, capacity)?;
        self.metrics
            .record_live_results_count(restored.results_len());
        self.metrics.update_outbox(
            restored.outbox_len(),
            restored.outbox_earliest_seq().unwrap_or(0),
            restored.as_of_sequence(),
        );
        *self.state.write().await = restored;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::{
        in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore,
        interface::{NoOpSessionControl, RootOutcome},
    };

    fn delivery() -> OutputDelivery {
        OutputDelivery {
            query_id: "query".into(),
            state: Arc::new(RwLock::new(QueryOutputState::new(10))),
            dispatchers: Arc::new(RwLock::new(Vec::new())),
            outbox_writer: None,
            live_results_writer: None,
            checkpoint_store: Arc::new(InMemoryCheckpointStore::new()),
            metrics: Arc::new(QueryOutputMetrics::new()),
        }
    }

    fn diffs() -> Vec<ResultDiff> {
        vec![ResultDiff::Add {
            row_signature: 7,
            data: serde_json::json!({"value": 3}),
        }]
    }

    #[tokio::test]
    async fn bootstrap_changes_snapshot_only_after_commit() {
        let output = delivery();
        let root = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let staged = output.stage_bootstrap(diffs(), &root).await.unwrap();
        assert_eq!(output.state.read().await.results_len(), 0);
        let receipt = root.commit_with_receipt().await.unwrap();
        output.publish_bootstrap(staged, &receipt).await.unwrap();
        let state = output.state.read().await;
        assert_eq!(state.results_len(), 1);
        assert_eq!(state.as_of_sequence(), 0);
        assert_eq!(state.outbox_len(), 0);
    }

    #[tokio::test]
    async fn output_is_invisible_until_its_root_commits() {
        let output = delivery();
        let root = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let staged = output
            .stage(
                diffs(),
                "source",
                ProfilingMetadata::new(),
                &root,
                Instant::now(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.state.read().await.as_of_sequence(), 0);
        assert_eq!(output.state.read().await.results_len(), 0);
        let receipt = root.commit_with_receipt().await.unwrap();
        output.publish(staged, &receipt).await.unwrap();
        assert_eq!(output.state.read().await.as_of_sequence(), 1);
        assert_eq!(output.state.read().await.results_len(), 1);
    }

    #[tokio::test]
    async fn another_root_cannot_publish_aborted_output() {
        let output = delivery();
        let root = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let handle = root.root();
        let staged = output
            .stage(
                diffs(),
                "source",
                ProfilingMetadata::new(),
                &root,
                Instant::now(),
            )
            .await
            .unwrap()
            .unwrap();
        drop(root);
        assert_eq!(handle.outcome(), RootOutcome::RequiresRebuild);
        let other = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let receipt = other.commit_with_receipt().await.unwrap();
        assert!(output.publish(staged, &receipt).await.is_err());
        assert_eq!(output.state.read().await.as_of_sequence(), 0);
        assert_eq!(output.state.read().await.results_len(), 0);
    }

    #[tokio::test]
    async fn restored_output_continues_its_existing_sequence() {
        let output = delivery();
        let old_result = QueryResult::new(
            "query".into(),
            12,
            chrono::Utc::now(),
            diffs(),
            HashMap::new(),
        );
        *output.state.write().await = QueryOutputState::from_persisted(
            vec![(7, serde_json::json!({"value": 3}))],
            12,
            vec![old_result],
            10,
        )
        .unwrap();
        let root = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        let staged = output
            .stage(
                diffs(),
                "source",
                ProfilingMetadata::new(),
                &root,
                Instant::now(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(staged.result.sequence, 13);
        let receipt = root.commit_with_receipt().await.unwrap();
        output.publish(staged, &receipt).await.unwrap();
        let state = output.state.read().await;
        assert_eq!(state.as_of_sequence(), 13);
        assert_eq!(state.outbox_earliest_seq(), Some(12));
    }

    #[test]
    fn persisted_outbox_gaps_are_not_silently_restored() {
        let results = [10, 12]
            .into_iter()
            .map(|sequence| {
                QueryResult::new(
                    "query".into(),
                    sequence,
                    chrono::Utc::now(),
                    diffs(),
                    HashMap::new(),
                )
            })
            .collect();
        assert!(QueryOutputState::from_persisted(Vec::new(), 12, results, 10).is_err());
    }
}
