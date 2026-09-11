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
            let preparation = provider.prepare().await?;
            if preparation != super::super::BootstrapPreparation::Ready {
                if preparation == super::super::BootstrapPreparation::RefreshVolatile
                    && self.provider.is_volatile()
                    || self.options.recovery == QueryRecoveryPolicy::AutoReset
                {
                    self.reset_for_bootstrap().await?;
                    self.publish_source_progress(false).await?;
                    if provider.prepare().await? != super::super::BootstrapPreparation::Ready {
                        return Err(QueryRecoveryError::SourceResetRequired.into());
                    }
                } else {
                    return Err(QueryRecoveryError::SourceResetRequired.into());
                }
            }
        }
        self.run_bootstrap().await
    }

    async fn reset_for_bootstrap(&mut self) -> anyhow::Result<()> {
        if self.bootstrap.is_none() {
            anyhow::bail!(
                "AutoReset requires a declared bootstrap provider; no data is deleted without one"
            );
        }
        let query = self.query()?;
        let resources = query.resources();
        let mut highwater = self.results.snapshot()?.as_of_sequence;
        let old_marker = self.read_reset_marker().await?;
        let generation = self
            .results
            .snapshot()?
            .generation
            .max(
                old_marker
                    .as_ref()
                    .map(|marker| marker.generation)
                    .unwrap_or(0),
            )
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query reset generation exhausted"))?;
        if self.provider.is_volatile() {
            query.shutdown().await?;
            self.query = None;
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
        let config = self.definition.configuration_bytes(&self.execution)?;
        query
            .resource_transaction(|| async {
                resources.indexes().element_index.clear().await?;
                match resources.indexes().archive_index.clear().await {
                    Err(IndexError::NotSupported) if self.runtime_compatibility => {
                        log::debug!(
                            "Query {} has no clearable archive index",
                            self.definition.id
                        );
                    }
                    result => result?,
                }
                resources.indexes().result_index.clear().await?;
                query.future_queue().clear().await?;
                if let Some(outbox) = resources.outbox_writer() {
                    outbox.clear(self.definition.id.as_str()).await?;
                }
                if let Some(live) = resources.live_results_writer() {
                    live.clear(self.definition.id.as_str()).await?;
                }
                if let Some(checkpoint) = resources.checkpoint_store() {
                    checkpoint.clear_checkpoints().await?;
                    checkpoint
                        .stage_checkpoint(CONFIGURATION, 1, Some(&config))
                        .await?;
                    if self.runtime_compatibility {
                        if let Some(hash) = self.legacy_hash {
                            checkpoint.write_config_hash(hash).await?;
                        }
                    }
                    checkpoint.stage_checkpoint(BOOTSTRAP, 0, None).await?;
                    checkpoint
                        .write_result_sequence(self.definition.id.as_str(), highwater)
                        .await?;
                }
                Ok(())
            })
            .await?;
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
        if self.bootstrap_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        let query = self.query()?;
        if let Some(checkpoint) = query.resources().checkpoint_store() {
            if checkpoint
                .read_checkpoint(BOOTSTRAP)
                .await?
                .is_some_and(|marker| marker.sequence == 1)
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
        let mut snapshot = provider.snapshot().await?;
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
                        let output = QueryChangeCodec::encode_evaluation(
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
                    checkpoint
                        .write_result_sequence(self.definition.id.as_str(), sequence)
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
