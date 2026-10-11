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

use futures::StreamExt;

use super::super::query_bootstrap::{
    validate_bootstrap_state, BootstrapState, QueryBootstrapState, BOOTSTRAP_STATE,
};
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResetMarker {
    pub(super) sequence: u64,
    pub(super) in_progress: bool,
    #[serde(default)]
    pub(super) generation: u64,
}

impl ContinuousQueryTransformer {
    /// Wipes every durable trace of the query's output so a recreated query
    /// starts a new incarnation at sequence 1.
    ///
    /// The restarted sequence space is published under a newer output
    /// generation, so a consumer position held for an earlier incarnation can
    /// never match it. The wipe is recorded before it starts: an interrupted
    /// deprovision is an incomplete reset that recovery refuses to resume,
    /// never a partially cleared query. Completion leaves a baseline reset
    /// marker that carries the generation even for checkpoint stores whose
    /// `write_output_generation` is a no-op.
    pub(super) async fn deprovision_state(&mut self) -> anyhow::Result<()> {
        if let Some(bootstrap) = &self.bootstrap {
            bootstrap.stop().await?;
        }
        if self.query.is_none() {
            self.build().await?;
        }
        let query = self.query()?;
        let previous = self.read_reset_marker().await?;
        let generation = self.next_output_generation(previous.as_ref()).await?;
        if !self.provider.is_volatile() {
            let id = self.definition.id.as_str();
            let resources = query.resources();
            let marker = |in_progress| {
                serde_json::to_vec(&ResetMarker {
                    sequence: 0,
                    in_progress,
                    generation,
                })
            };
            if let Some(outbox) = resources.outbox_writer() {
                let started = marker(true)?;
                query
                    .resource_transaction(|| async {
                        outbox.append(&self.reset_key(), 1, &started).await
                    })
                    .await?;
            }
            if let Some(store) = resources.checkpoint_store() {
                store.write_output_generation(id, generation).await?;
            }
            let baseline = marker(false)?;
            query
                .resource_transaction(|| async {
                    resources.indexes().element_index.clear().await?;
                    resources.indexes().archive_index.clear().await?;
                    resources.indexes().result_index.clear().await?;
                    query.future_queue().clear().await?;
                    if let Some(store) = resources.checkpoint_store() {
                        store.clear_checkpoints().await?;
                        store.stage_result_sequence(id, 0).await?;
                    }
                    if let Some(outbox) = resources.outbox_writer() {
                        outbox.clear(id).await?;
                        outbox.append(&self.reset_key(), 1, &baseline).await?;
                    }
                    if let Some(live) = resources.live_results_writer() {
                        live.clear(id).await?;
                    }
                    Ok(())
                })
                .await?;
        }
        query.shutdown().await?;
        self.release_query()?;
        self.pending_output.clear();
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .reset_to_sequence(0, generation);
        Ok(())
    }

    /// The generation for the query's next output incarnation: strictly newer
    /// than every generation it has published, persisted or recorded in
    /// `marker`.
    async fn next_output_generation(&self, marker: Option<&ResetMarker>) -> anyhow::Result<u64> {
        let persisted = match self.query()?.resources().checkpoint_store() {
            Some(store) => store
                .read_output_generation(self.definition.id.as_str())
                .await?
                .unwrap_or(0),
            None => 0,
        };
        self.results
            .snapshot()?
            .generation
            .max(marker.map_or(0, |marker| marker.generation))
            .max(persisted)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query output generation exhausted"))
    }

    fn reset_key(&self) -> String {
        super::query_reset_key(self.definition.id.as_str())
    }

    pub(super) async fn read_reset_marker(&self) -> anyhow::Result<Option<ResetMarker>> {
        let Some(outbox) = self.query()?.resources().outbox_writer() else {
            return Ok(None);
        };
        let rows = outbox.read_from(&self.reset_key(), 0).await?;
        match rows.as_slice() {
            [] => Ok(None),
            [(1, bytes)] => Ok(Some(serde_json::from_slice(bytes)?)),
            _ => Err(QueryRecoveryError::Inconsistent("invalid reset marker".into()).into()),
        }
    }

    pub(super) async fn initialize_recovery(&mut self) -> anyhow::Result<()> {
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .ready = false;
        let volatile_failed = self.provider.is_volatile() && self.failure.load(Ordering::Acquire);
        if volatile_failed {
            self.reset_for_bootstrap().await?;
        } else if let Err(error) = self.recover().await {
            let explicit_configuration_reset = self.reset_configuration
                && matches!(
                    error.downcast_ref::<QueryRecoveryError>(),
                    Some(QueryRecoveryError::ConfigurationChanged)
                );
            if self.options.recovery != QueryRecoveryPolicy::AutoReset
                && !explicit_configuration_reset
                || error.downcast_ref::<QueryRecoveryError>().is_none()
            {
                return Err(error);
            }
            self.reset_for_bootstrap().await?;
        }
        self.publish_source_progress(false).await?;
        if let Some(provider) = self.bootstrap.clone() {
            let preparation = provider
                .prepare_with_state(&QueryBootstrapState(self.query()?))
                .await?;
            if preparation != super::super::BootstrapPreparation::Ready {
                if preparation == super::super::BootstrapPreparation::RefreshVolatile
                    && self.provider.is_volatile()
                    || self.options.recovery == QueryRecoveryPolicy::AutoReset
                {
                    self.reset_for_bootstrap().await?;
                    self.publish_source_progress(false).await?;
                    if provider
                        .prepare_with_state(&QueryBootstrapState(self.query()?))
                        .await?
                        != super::super::BootstrapPreparation::Ready
                    {
                        return Err(QueryRecoveryError::SourceResetRequired.into());
                    }
                } else {
                    return Err(QueryRecoveryError::SourceResetRequired.into());
                }
            }
        }
        if let Some(progress) = &self.source_progress {
            progress.admit();
        }
        self.run_bootstrap().await
    }

    async fn reset_for_bootstrap(&mut self) -> anyhow::Result<()> {
        if self.bootstrap.is_none() {
            anyhow::bail!(
                "AutoReset requires a declared bootstrap provider; no data is deleted without one"
            );
        }
        if self.output_persistent
            && !self
                .load_delivery_bindings()
                .await?
                .0
                .destinations()
                .is_empty()
        {
            return Err(super::super::OutputBindingError::ResetRequiresUnboundDestinations.into());
        }
        let query = self.query()?;
        let resources = query.resources();
        let mut highwater = self.results.snapshot()?.as_of_sequence;
        let mut delivery_sequence = self.delivery_sequence.load(Ordering::Acquire);
        if let Some(checkpoint) = resources.checkpoint_store() {
            if let Some(saved) = checkpoint
                .read_checkpoint(delivery::DELIVERY_SEQUENCE)
                .await?
            {
                delivery_sequence = delivery_sequence.max(saved.sequence);
            }
        }
        let old_marker = self.read_reset_marker().await?;
        let generation = self.next_output_generation(old_marker.as_ref()).await?;
        if self.provider.is_volatile() {
            query.shutdown().await?;
            self.release_query()?;
            self.build().await?;
            self.results
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
                .reset_to_sequence(highwater, generation);
            self.watermarks
                .lock()
                .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
                .clear();
            self.bootstrap_complete.store(false, Ordering::Release);
            return Ok(());
        }
        if let Some(marker) = old_marker {
            highwater = highwater.max(marker.sequence);
        }
        if let Some(checkpoint) = resources.checkpoint_store() {
            highwater = highwater.max(
                checkpoint
                    .read_result_sequence(self.definition.id.as_str())
                    .await?
                    .unwrap_or(0),
            );
            if let Some(pending) = checkpoint.read_checkpoint(PENDING_OUTPUT).await? {
                highwater = highwater.max(pending.sequence);
            }
        }
        if let Some(outbox) = resources.outbox_writer() {
            highwater = highwater.max(
                outbox
                    .read_latest_sequence(self.definition.id.as_str())
                    .await?
                    .unwrap_or(0),
            );
            let marker = serde_json::to_vec(&ResetMarker {
                sequence: highwater,
                in_progress: true,
                generation,
            })?;
            query
                .resource_transaction(|| async {
                    outbox.append(&self.reset_key(), 1, &marker).await
                })
                .await?;
        }
        if let Some(checkpoint) = resources.checkpoint_store() {
            if let Some(hash) = self.legacy_hash.filter(|_| self.runtime_compatibility) {
                checkpoint
                    .write_config_hash(crate::queries::config_hash::output_reset_in_progress_hash(
                        hash,
                    ))
                    .await?;
            }
            checkpoint
                .write_output_generation(self.definition.id.as_str(), generation)
                .await?;
        }
        let config = self.definition.configuration_bytes(&self.execution)?;
        let bootstrap_state = QueryBootstrapState(query).read().await?;
        query
            .resource_transaction(|| async {
                crate::indexes::check_clear(
                    self.definition.id.as_str(),
                    "clear element index",
                    resources.indexes().element_index.clear().await,
                    self.runtime_compatibility,
                )?;
                crate::indexes::check_clear(
                    self.definition.id.as_str(),
                    "clear archive index",
                    resources.indexes().archive_index.clear().await,
                    self.runtime_compatibility,
                )?;
                crate::indexes::check_clear(
                    self.definition.id.as_str(),
                    "clear result index",
                    resources.indexes().result_index.clear().await,
                    self.runtime_compatibility,
                )?;
                crate::indexes::check_clear(
                    self.definition.id.as_str(),
                    "clear future queue",
                    query.future_queue().clear().await,
                    self.runtime_compatibility,
                )?;
                if let Some(outbox) = resources.outbox_writer() {
                    outbox.clear(self.definition.id.as_str()).await?;
                }
                if let Some(live) = resources.live_results_writer() {
                    live.clear(self.definition.id.as_str()).await?;
                }
                if let Some(checkpoint) = resources.checkpoint_store() {
                    checkpoint.clear_checkpoints().await?;
                    if let Some(state) = &bootstrap_state {
                        checkpoint
                            .stage_checkpoint(BOOTSTRAP_STATE, 1, Some(state))
                            .await?;
                    }
                    checkpoint
                        .stage_checkpoint(CONFIGURATION, 1, Some(&config))
                        .await?;
                    checkpoint.stage_checkpoint(BOOTSTRAP, 0, None).await?;
                    checkpoint
                        .stage_result_sequence(self.definition.id.as_str(), highwater)
                        .await?;
                    if self.delivery_tracking {
                        checkpoint
                            .stage_checkpoint(delivery::DELIVERED, highwater, None)
                            .await?;
                        checkpoint
                            .stage_checkpoint(delivery::DELIVERY_SEQUENCE, delivery_sequence, None)
                            .await?;
                    }
                }
                Ok(())
            })
            .await?;
        if let (true, Some(hash), Some(checkpoint)) = (
            self.runtime_compatibility,
            self.legacy_hash,
            resources.checkpoint_store(),
        ) {
            checkpoint.write_config_hash(hash).await?;
        }
        self.results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
            .reset_to_sequence(highwater, generation);
        self.watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .clear();
        self.bootstrap_complete.store(false, Ordering::Release);
        Ok(())
    }

    async fn run_bootstrap(&self) -> anyhow::Result<()> {
        let Some(provider) = &self.bootstrap else {
            return Ok(());
        };
        // Ordinary Source subscriptions can request a new snapshot when no
        // source checkpoint exists. Consume the receivers chosen for this start
        // without resetting previously committed output or its generation.
        let requested_snapshot = self.runtime_compatibility && provider.has_pending_snapshot()?;
        if self.bootstrap_complete.load(Ordering::Acquire) && !requested_snapshot {
            return Ok(());
        }
        let query = self.query()?;
        if let Some(checkpoint) = query.resources().checkpoint_store() {
            if checkpoint
                .read_checkpoint(BOOTSTRAP)
                .await?
                .is_some_and(|marker| marker.sequence == 1)
                && !requested_snapshot
            {
                self.bootstrap_complete.store(true, Ordering::Release);
                return Ok(());
            }
            query
                .resource_transaction(|| async {
                    checkpoint.stage_checkpoint(BOOTSTRAP, 0, None).await
                })
                .await?;
        }
        let mut snapshot = provider
            .snapshot_with_state(&QueryBootstrapState(query))
            .await?;
        validate_watermarks(&snapshot.watermarks)?;
        let bootstrap_stream = StreamId::try_new(format!(
            "{}/bootstrap/{}",
            self.definition.id,
            uuid::Uuid::new_v4()
        ))?;
        let mut ordinal = 0u64;
        while let Some(input) = snapshot.changes.next().await {
            let input = input?;
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("bootstrap ordinal exhausted"))?;
            let changes = GraphChangeCodec::decode_changes(&input)?;
            let prepared = Mutex::new(None);
            let hook =
                |results: Arc<[drasi_core::evaluation::context::QueryPartEvaluationContext]>| {
                    let input = &input;
                    let prepared = &prepared;
                    let stream = bootstrap_stream.clone();
                    async move {
                        let mut output = QueryChangeCodec::encode_evaluation(
                            Some(input),
                            &self.definition.id,
                            SystemMetadata::new(stream, ordinal),
                            &results,
                            QueryOutputMetadata {
                                query_id: self.definition.id.to_string(),
                                source_id: None,
                                timestamp: Utc::now(),
                                metadata: HashMap::new(),
                                profiling: None,
                            },
                        )
                        .map_err(IndexError::other)?;
                        if let Some(output) = &mut output {
                            self.stamp_output_identity(output)
                                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
                        }
                        if self.output_persistent
                            && self.options.publication == QueryPublicationMode::Atomic
                        {
                            if let Some(output) = &output {
                                self.stage_projection(output).await?;
                            }
                        }
                        *prepared.lock().map_err(|_| IndexError::CorruptedData)? = output;
                        Ok(())
                    }
                };
            if self.options.publication == QueryPublicationMode::Atomic && self.output_persistent {
                query
                    .process_source_changes_with_result_hook(
                        changes,
                        &query.resources().atomic_result_transaction()?,
                        hook,
                    )
                    .await?;
            } else {
                query
                    .process_source_changes_with_non_atomic_result_hook(changes, hook)
                    .await?;
            }
            if let Some(output) = prepared
                .into_inner()
                .map_err(|_| anyhow::anyhow!("bootstrap output poisoned"))?
            {
                if self.output_persistent
                    && self.options.publication == QueryPublicationMode::NonAtomic
                {
                    query
                        .resource_transaction(|| async { self.stage_projection(&output).await })
                        .await?;
                }
                self.results
                    .state
                    .write()
                    .map_err(|_| anyhow::anyhow!("query output state poisoned"))?
                    .apply_rows(&output);
            }
        }
        snapshot
            .watermarks
            .extend(provider.complete_snapshot().await?);
        validate_watermarks(&snapshot.watermarks)?;
        let completion_state = provider.completion_state()?;
        if let Some(state) = &completion_state {
            validate_bootstrap_state(state)?;
            query.resources().atomic_result_transaction()?;
            anyhow::ensure!(
                query.resources().checkpoint_store().is_some(),
                "bootstrap handover completion requires a checkpoint store"
            );
        }
        let watermarks: Vec<_> = snapshot
            .watermarks
            .into_iter()
            .map(|watermark| {
                (
                    progress_key(watermark.stream.as_str(), watermark.source_id.as_deref()),
                    watermark.sequence,
                    watermark.position,
                )
            })
            .collect();
        if let Some(checkpoint) = query.resources().checkpoint_store() {
            let sequence = self.results.snapshot()?.as_of_sequence;
            let reset = self.read_reset_marker().await?;
            let marker = serde_json::to_vec(&ResetMarker {
                sequence,
                in_progress: false,
                generation: self.results.snapshot()?.generation,
            })?;
            query
                .resource_transaction(|| async {
                    for (key, sequence, position) in &watermarks {
                        checkpoint
                            .stage_checkpoint(key, *sequence, position.as_ref())
                            .await?;
                    }
                    checkpoint.stage_checkpoint(BOOTSTRAP, 1, None).await?;
                    if let Some(state) = &completion_state {
                        checkpoint
                            .stage_checkpoint(BOOTSTRAP_STATE, 1, Some(state))
                            .await?;
                    }
                    checkpoint
                        .stage_result_sequence(self.definition.id.as_str(), sequence)
                        .await?;
                    if reset.is_some() {
                        query
                            .resources()
                            .outbox_writer()
                            .ok_or(IndexError::NotSupported)?
                            .append(&self.reset_key(), 1, &marker)
                            .await?;
                    }
                    Ok(())
                })
                .await?;
        }
        if let Some(progress) = &self.source_progress {
            for (key, sequence, position) in &watermarks {
                progress.confirm(
                    progress_identity(key)?,
                    SourceCheckpoint::new(*sequence, position.clone()),
                );
            }
        }
        self.watermarks
            .lock()
            .map_err(|_| anyhow::anyhow!("watermarks poisoned"))?
            .extend(
                watermarks
                    .into_iter()
                    .map(|(key, sequence, _)| (key, sequence)),
            );
        self.bootstrap_complete.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn begin_non_atomic(&self) -> anyhow::Result<()> {
        if !self.output_persistent || self.options.publication != QueryPublicationMode::NonAtomic {
            return Ok(());
        }
        let query = self.query()?;
        let in_progress = Bytes::from_static(b"evaluating-v1");
        query
            .resource_transaction(|| async {
                query
                    .resources()
                    .checkpoint_store()
                    .ok_or(IndexError::NotSupported)?
                    .stage_checkpoint(PENDING_OUTPUT, 0, Some(&in_progress))
                    .await
            })
            .await?;
        Ok(())
    }

    pub(super) async fn finish_non_atomic(
        &self,
        output: Option<&super::super::ChangeEnvelope>,
    ) -> anyhow::Result<()> {
        let query = self.query()?;
        query
            .resource_transaction(|| async {
                if let Some(output) = output {
                    self.stage_output(output).await?;
                }
                query
                    .resources()
                    .checkpoint_store()
                    .ok_or(IndexError::NotSupported)?
                    .stage_checkpoint(PENDING_OUTPUT, 0, None)
                    .await?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}
