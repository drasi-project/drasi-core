// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query initialization over ordinary output providers. The graph binds the
//! lossless live pipes before starting the query; snapshot watermarks suppress
//! buffered duplicates, while replay precedes newly buffered live input.

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputOwner {
    component: ComponentId,
    port: PortId,
    producer: GraphProducerIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handover {
    version: u32,
    outputs: Vec<OutputOwner>,
}

struct Prepared {
    snapshots: Option<Vec<OutputSnapshot>>,
    replay: Vec<ChangeEnvelope>,
    completion: Option<Bytes>,
    origins: BTreeMap<StreamId, OutputEndpoint>,
}

pub(crate) struct OutputBootstrapProvider {
    catalogue: OutputCatalogue,
    outputs: Vec<OutputEndpoint>,
    progress: Arc<QuerySourceProgress>,
    timeout: Duration,
    prepared: Mutex<Option<Prepared>>,
}

impl OutputBootstrapProvider {
    pub(crate) fn new(
        catalogue: OutputCatalogue,
        outputs: Vec<OutputEndpoint>,
        progress: Arc<QuerySourceProgress>,
        timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !outputs.is_empty() && !timeout.is_zero(),
            "output bootstrap needs inputs and a timeout"
        );
        let unique: BTreeSet<_> = outputs.iter().collect();
        anyhow::ensure!(unique.len() == outputs.len(), "duplicate bootstrap output");
        Ok(Arc::new(Self {
            catalogue,
            outputs,
            progress,
            timeout,
            prepared: Mutex::new(None),
        }))
    }

    fn prepared(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Option<Prepared>>> {
        self.prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("output bootstrap state poisoned"))
    }

    async fn prepare_outputs(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<BootstrapPreparation> {
        *self.prepared()? = None;
        let progress = self.progress.snapshot();
        if progress.bootstrap_complete && !progress.persistent {
            // Rebuild the whole volatile input view, including removals that
            // happened while this query was stopped.
            return Ok(BootstrapPreparation::RefreshVolatile);
        }
        let saved = match state {
            Some(state) => state.read().await?,
            None => None,
        };
        let saved: Option<Handover> = saved.as_deref().map(serde_json::from_slice).transpose()?;
        if let Some(saved) = &saved {
            anyhow::ensure!(
                saved.version == 1,
                "invalid output bootstrap handover version"
            );
        }
        let resuming = progress.bootstrap_complete;
        if resuming
            && saved.as_ref().map_or(true, |saved| {
                saved.outputs.len() != self.outputs.len()
                    || saved
                        .outputs
                        .iter()
                        .zip(&self.outputs)
                        .any(|(saved, output)| {
                            saved.component != output.component || saved.port != output.port
                        })
            })
        {
            return Ok(BootstrapPreparation::ResetRequired);
        }
        let mut snapshots = Vec::new();
        let mut replay = Vec::new();
        let mut owners = Vec::new();
        for (index, output) in self.outputs.iter().enumerate() {
            let handle = self.catalogue.resolve(output)?;
            if resuming {
                let saved = &saved.as_ref().expect("resume handover validated").outputs[index];
                saved.producer.validate()?;
                let Some(checkpoint) = progress
                    .checkpoints
                    .get(&SourceProgressKey::Stream(saved.producer.stream().clone()))
                else {
                    return Ok(BootstrapPreparation::ResetRequired);
                };
                let cursor = OutputCursor::try_new(saved.producer.clone(), checkpoint.sequence)?;
                let suffix = handle.replay_after(&cursor).await?;
                owners.push(OutputOwner {
                    component: output.component.clone(),
                    port: output.port.clone(),
                    producer: suffix.head().producer().clone(),
                });
                replay.extend(suffix.into_envelopes());
            } else {
                let snapshot = handle.snapshot().await?;
                anyhow::ensure!(
                    snapshot.schema() == GraphChangeCodec::schema().descriptor(),
                    "query bootstrap requires graph-change input"
                );
                anyhow::ensure!(
                    !progress.persistent || snapshot.cursor().producer().persistent(),
                    "persistent query cannot bootstrap from a volatile output"
                );
                owners.push(OutputOwner {
                    component: output.component.clone(),
                    port: output.port.clone(),
                    producer: snapshot.cursor().producer().clone(),
                });
                snapshots.push(snapshot);
            }
        }
        let completion = if progress.persistent {
            let encoded = Bytes::from(serde_json::to_vec(&Handover {
                version: 1,
                outputs: owners,
            })?);
            super::query_bootstrap::validate_bootstrap_state(&encoded)?;
            Some(encoded)
        } else {
            None
        };
        *self.prepared()? = Some(Prepared {
            snapshots: (!resuming).then_some(snapshots),
            replay,
            completion,
            origins: BTreeMap::new(),
        });
        Ok(BootstrapPreparation::Ready)
    }
}

#[async_trait]
impl ComputationBootstrapProvider for OutputBootstrapProvider {
    fn validate_query_scope(
        &self,
        _: &str,
        graph: &str,
        query: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            graph == self.progress.graph_id() && query == self.progress.component_id(),
            "output bootstrap belongs to a different query"
        );
        Ok(())
    }

    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(SourceProgressReader::Local(self.progress.clone()))
    }

    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        tokio::time::timeout(self.timeout, self.prepare_outputs(None)).await?
    }

    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        tokio::time::timeout(self.timeout, self.prepare_outputs(Some(state))).await?
    }

    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        Ok(self
            .prepared()?
            .as_ref()
            .is_some_and(|prepared| prepared.snapshots.is_some()))
    }

    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let mut prepared = self.prepared()?;
        let snapshots = prepared
            .as_mut()
            .and_then(|prepared| prepared.snapshots.take())
            .ok_or_else(|| {
                anyhow::anyhow!("output snapshot was not prepared or was already consumed")
            })?;
        let watermarks = snapshots
            .iter()
            .map(|snapshot| BootstrapWatermark::from_output(snapshot.cursor()))
            .collect();
        let mut streams = Vec::with_capacity(snapshots.len());
        let origins = &mut prepared.as_mut().expect("prepared snapshot").origins;
        for (snapshot, output) in snapshots.into_iter().zip(&self.outputs) {
            let stream = StreamId::try_new(format!("bootstrap/{}", uuid::Uuid::new_v4()))?;
            origins.insert(stream.clone(), output.clone());
            streams.push((stream, snapshot.records().clone()));
        }
        drop(prepared);
        let changes = streams.into_iter().flat_map(|(stream, records)| {
            records
                .into_iter()
                .enumerate()
                .map(move |(index, (_, record))| {
                    let sequence = u64::try_from(index)?
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("bootstrap ordinal exhausted"))?;
                    let changes = ChangeSet::try_new(
                        ChangeSetId::try_new(
                            stream.as_str(),
                            Bytes::copy_from_slice(&sequence.to_be_bytes()),
                        )?,
                        GraphChangeCodec::schema().descriptor().clone(),
                        vec![ChangeOperation::Added {
                            ordinal: 0,
                            after: record,
                        }],
                    )?;
                    Ok(ChangeEnvelope::new(
                        emission_id(&stream, sequence)?,
                        changes,
                        SystemMetadata::new(stream.clone(), sequence),
                    ))
                })
        });
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(changes)),
            watermarks,
        })
    }

    fn take_input_replay(&self) -> anyhow::Result<Vec<ChangeEnvelope>> {
        let mut prepared = self.prepared()?;
        let prepared = prepared
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("output replay was not prepared"))?;
        Ok(std::mem::take(&mut prepared.replay))
    }

    fn input_origin(&self, stream: &StreamId) -> anyhow::Result<Option<OutputEndpoint>> {
        Ok(self
            .prepared()?
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("output bootstrap was not prepared"))?
            .origins
            .get(stream)
            .cloned())
    }

    fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
        Ok(self
            .prepared()?
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("output handover was not prepared"))?
            .completion
            .clone())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        *self.prepared()? = None;
        Ok(())
    }
}
