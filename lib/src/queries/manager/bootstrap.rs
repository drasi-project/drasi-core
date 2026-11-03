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

use super::*;
use crate::bootstrap::BootstrapResult;
use futures::{stream::FuturesUnordered, StreamExt};
use tokio::sync::{mpsc, oneshot};

pub(super) struct BootstrapInput {
    pub(super) source_id: String,
    events: mpsc::Receiver<BootstrapEvent>,
    result: Option<oneshot::Receiver<Result<BootstrapResult>>>,
    count: usize,
}

impl BootstrapInput {
    pub(super) fn new(
        source_id: String,
        events: mpsc::Receiver<BootstrapEvent>,
        result: Option<oneshot::Receiver<Result<BootstrapResult>>>,
    ) -> Self {
        Self {
            source_id,
            events,
            result,
            count: 0,
        }
    }

    async fn receive(mut self) -> (Self, Result<BootstrapStep>) {
        let step = if let Some(event) = self.events.recv().await {
            Ok(BootstrapStep::Change(event))
        } else if let Some(result) = self.result.take() {
            match result.await {
                Ok(result) => result
                    .with_context(|| format!("Bootstrap source '{}' failed", self.source_id))
                    .map(|result| BootstrapStep::Complete(Some(result))),
                Err(error) => Err(anyhow::anyhow!(
                    "Bootstrap source '{}' dropped its completion channel: {error}",
                    self.source_id
                )),
            }
        } else {
            Ok(BootstrapStep::Complete(None))
        };
        (self, step)
    }
}

enum BootstrapStep {
    Change(BootstrapEvent),
    Complete(Option<BootstrapResult>),
}

pub(super) enum BootstrapOutcome {
    Complete,
    Stopped,
}

pub(super) struct BootstrapPersistence {
    pub(super) stores: DurableOutputStores,
    pub(super) session: Arc<dyn SessionControl>,
    pub(super) config_hash: u64,
}

pub(super) struct BootstrapRun {
    pub(super) query_id: String,
    pub(super) require_complete: bool,
    pub(super) inputs: Vec<BootstrapInput>,
    pub(super) source_ids: Vec<String>,
    pub(super) query: Arc<ContinuousQuery>,
    pub(super) output: Arc<RwLock<QueryOutputState>>,
    pub(super) state: Arc<RwLock<HashMap<String, BootstrapPhase>>>,
    pub(super) persistence: Option<BootstrapPersistence>,
}

pub(super) fn stopping(shutdown: &mut oneshot::Receiver<()>) -> bool {
    !matches!(
        shutdown.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    )
}

impl BootstrapRun {
    pub(super) async fn run(
        self,
        shutdown: &mut oneshot::Receiver<()>,
    ) -> Result<BootstrapOutcome> {
        if stopping(shutdown) {
            return Ok(BootstrapOutcome::Stopped);
        }
        let mut image = self.output.read().await.clone();
        let mut handovers: HashMap<_, _> = if self.require_complete {
            self.source_ids
                .iter()
                .cloned()
                .map(|id| (id, None))
                .collect()
        } else {
            HashMap::new()
        };
        let mut seen = HashSet::new();
        for input in &self.inputs {
            anyhow::ensure!(
                self.source_ids.contains(&input.source_id) && seen.insert(input.source_id.clone()),
                "Query '{}' has an unknown or duplicate bootstrap source '{}'",
                self.query_id,
                input.source_id
            );
            anyhow::ensure!(
                self.persistence.is_none() || !self.require_complete || input.result.is_some(),
                "Persistent bootstrap source '{}' has no completion channel",
                input.source_id
            );
        }
        if let Some(persistence) = &self.persistence {
            anyhow::ensure!(
                image.as_of_sequence() == 0 && image.outbox_len() == 0 && image.results_len() == 0,
                "Query '{}' cannot bootstrap over existing output",
                self.query_id
            );
            let pending = bootstrap_in_progress_hash(persistence.config_hash);
            persistence
                .stores
                .checkpoint_store
                .write_config_hash(pending)
                .await
                .context("Failed to fence durable bootstrap")?;
            anyhow::ensure!(
                persistence
                    .stores
                    .checkpoint_store
                    .read_config_hash()
                    .await?
                    == Some(pending),
                "Durable bootstrap fence was not persisted"
            );
        }
        {
            let mut state = self.state.write().await;
            for input in &self.inputs {
                state.insert(input.source_id.clone(), BootstrapPhase::InProgress(0));
            }
        }

        // These futures only receive channels. Evaluation and storage have one owner.
        let mut waiting = FuturesUnordered::new();
        for input in self.inputs {
            waiting.push(input.receive());
        }
        while let Some((mut input, step)) = tokio::select! {
            biased;
            _ = &mut *shutdown => return Ok(BootstrapOutcome::Stopped),
            step = waiting.next() => step,
        } {
            if stopping(shutdown) {
                return Ok(BootstrapOutcome::Stopped);
            }
            match step? {
                BootstrapStep::Change(event) => {
                    anyhow::ensure!(
                        event.source_id == input.source_id,
                        "Bootstrap event source '{}' does not match '{}'",
                        event.source_id,
                        input.source_id
                    );
                    input.count = input
                        .count
                        .checked_add(1)
                        .context("Bootstrap count overflow")?;
                    let results = self
                        .query
                        .process_source_change(event.change)
                        .await
                        .with_context(|| {
                            format!(
                                "Query '{}' failed to evaluate bootstrap source '{}'",
                                self.query_id, input.source_id
                            )
                        })?;
                    image.apply_diffs(&evaluation_contexts_to_diffs(&results));
                    self.state.write().await.insert(
                        input.source_id.clone(),
                        BootstrapPhase::InProgress(input.count),
                    );
                    waiting.push(input.receive());
                }
                BootstrapStep::Complete(result) => {
                    if let Some(result) = result {
                        anyhow::ensure!(
                            result.event_count == input.count,
                            "Bootstrap source '{}' reported {} events but delivered {}",
                            input.source_id,
                            result.event_count,
                            input.count
                        );
                        anyhow::ensure!(
                            result.source_position.as_ref().map_or(true, |position| {
                                position.len()
                                    <= crate::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES
                            }),
                            "Bootstrap source '{}' supplied an oversized handover position",
                            input.source_id
                        );
                        anyhow::ensure!(
                            self.persistence.is_none()
                                || (!self.require_complete && input.count == 0)
                                || result.source_position.is_some(),
                            "Persistent bootstrap source '{}' has no replayable handover",
                            input.source_id
                        );
                        if self.require_complete || result.source_position.is_some() {
                            handovers.insert(input.source_id, result.source_position);
                        }
                    } else {
                        anyhow::ensure!(
                            self.persistence.is_none()
                                || (!self.require_complete && input.count == 0),
                            "Persistent bootstrap source '{}' has no replayable handover",
                            input.source_id
                        );
                    }
                }
            }
        }

        if stopping(shutdown) {
            return Ok(BootstrapOutcome::Stopped);
        }
        if let Some(persistence) = &self.persistence {
            let encoded: Vec<_> = image
                .clone_results()
                .iter()
                .map(|(signature, value)| rmp_serde::to_vec(value).map(|data| (*signature, data)))
                .collect::<Result<_, _>>()?;
            let mutations: Vec<_> = encoded
                .iter()
                .map(|(signature, data)| RowMutation {
                    row_signature: *signature,
                    data: Some(data.as_slice()),
                })
                .collect();
            let writer = persistence
                .stores
                .live_results_writer
                .as_ref()
                .context("Persistent bootstrap requires a live-results writer")?;
            let session = SessionGuard::begin(persistence.session.clone())
                .await
                .context("Failed to begin bootstrap publication")?;
            writer
                .apply_mutations(&self.query_id, &mutations)
                .await
                .context("Failed to stage bootstrap rows")?;
            for (source_id, position) in &handovers {
                persistence
                    .stores
                    .checkpoint_store
                    .stage_checkpoint(source_id, 0, position.as_ref())
                    .await
                    .with_context(|| format!("Failed to stage bootstrap handover '{source_id}'"))?;
            }
            persistence
                .stores
                .checkpoint_store
                .stage_result_sequence(&self.query_id, 0)
                .await
                .context("Failed to stage bootstrap sequence")?;
            session
                .commit()
                .await
                .context("Failed to commit bootstrap image")?;

            let checkpoints = persistence
                .stores
                .checkpoint_store
                .read_all_checkpoints()
                .await?;
            anyhow::ensure!(
                handovers.iter().all(|(source_id, position)| {
                    checkpoints.get(source_id).is_some_and(|checkpoint| {
                        checkpoint.sequence == 0 && checkpoint.source_position == *position
                    })
                }),
                "Committed bootstrap handovers do not match the publication"
            );
            let (rows, outbox, sequence, generation) =
                load_durable_output(&self.query_id, &persistence.stores, image.outbox_capacity())
                    .await?;
            anyhow::ensure!(
                rows == image.clone_results()
                    && outbox.is_empty()
                    && sequence == 0
                    && generation == image.generation(),
                "Committed bootstrap output does not match the publication"
            );
        }
        if stopping(shutdown) {
            return Ok(BootstrapOutcome::Stopped);
        }

        *self.output.write().await = image;
        if let Some(persistence) = &self.persistence {
            // RAM and durable data are coherent even if this standalone write is uncertain.
            let published = if self.require_complete {
                bootstrap_published_hash(persistence.config_hash)
            } else {
                persistence.config_hash
            };
            persistence
                .stores
                .checkpoint_store
                .write_config_hash(published)
                .await
                .context("Failed to publish completed bootstrap")?;
            anyhow::ensure!(
                persistence
                    .stores
                    .checkpoint_store
                    .read_config_hash()
                    .await?
                    == Some(published),
                "Completed bootstrap marker was not persisted"
            );
        }
        if stopping(shutdown) {
            return Ok(BootstrapOutcome::Stopped);
        }
        let mut state = self.state.write().await;
        for source_id in seen {
            state.insert(source_id, BootstrapPhase::Completed);
        }
        Ok(BootstrapOutcome::Complete)
    }
}
