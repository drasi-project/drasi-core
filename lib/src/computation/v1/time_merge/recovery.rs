// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;

const CHECKPOINT: &str = "\0computation:source-time-merge:v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u32,
    definition: SourceTimeMergeDefinition,
    schema: SchemaDescriptor,
    sources: Vec<SourceState>,
    buffered: Vec<Vec<u8>>,
    pending: Option<(Vec<u8>, bool, u64)>,
    held: Option<Key>,
    last_emitted: Option<DateTime<Utc>>,
    logical_sequence: u64,
    late_sequence: u64,
    emission_sequence: u64,
    late_events: u64,
    discarded: u64,
    identity: GraphProducerIdentity,
    late_identity: Option<GraphProducerIdentity>,
    destinations: Vec<OutputDestination>,
}

impl SourceTimeMergeTransformer {
    fn snapshot_limit(&self) -> anyhow::Result<usize> {
        let configuration =
            rmp_serde::to_vec_named(&(&self.definition, self.schema.descriptor()))?.len();
        self.definition
            .max_buffered_bytes
            .get()
            .checked_mul(2)
            .and_then(|size| size.checked_add(configuration.checked_mul(4)?))
            .and_then(|size| {
                size.checked_add(
                    self.definition
                        .max_buffered_events
                        .get()
                        .checked_mul(1024)?,
                )
            })
            .and_then(|size| size.checked_add(2 * 1024 * 1024))
            .ok_or_else(|| anyhow::anyhow!("merge snapshot limit overflow"))
    }

    pub(super) async fn save(&self) -> anyhow::Result<()> {
        let Some(transaction) = &self.transaction else {
            return Ok(());
        };
        let saved = Saved {
            version: 1,
            definition: self.definition.clone(),
            schema: self.schema.descriptor().clone(),
            sources: self.state.sources.clone(),
            buffered: self
                .state
                .buffer
                .values()
                .map(|input| self.codec.encode(&input.envelope))
                .collect::<std::result::Result<Vec<_>, _>>()?,
            pending: self
                .state
                .pending
                .as_ref()
                .map(|pending| {
                    Ok::<_, BinaryEnvelopeCodecError>((
                        self.codec.encode(&pending.input.envelope)?,
                        pending.late,
                        pending.logical_sequence,
                    ))
                })
                .transpose()?,
            held: self.state.held,
            last_emitted: self.state.last_emitted,
            logical_sequence: self.state.logical_sequence,
            late_sequence: self.state.late_sequence,
            emission_sequence: self.state.emission_sequence,
            late_events: self.state.late_events,
            discarded: self.state.discarded,
            identity: self.state.identity.clone(),
            late_identity: self.state.late_identity.clone(),
            destinations: self.state.destinations.destinations().to_vec(),
        };
        let bytes = Bytes::from(encode_bounded_messagepack(&saved, self.snapshot_limit()?)?);
        transaction
            .run(async {
                transaction
                    .resources()
                    .checkpoint_store()
                    .expect("validated checkpoint")
                    .stage_checkpoint(CHECKPOINT, 1, Some(&bytes))
                    .await?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub(super) async fn load(&mut self) -> anyhow::Result<()> {
        if self.loaded {
            return Ok(());
        }
        let mut guard = self.guard();
        if let Some(transaction) = self.transaction.clone() {
            transaction.quiesce().await?;
            let store = transaction
                .resources()
                .checkpoint_store()
                .expect("validated checkpoint");
            let saved = transaction
                .run(async { Ok(store.read_all_checkpoints().await?) })
                .await?;
            if let Some(checkpoint) = saved.get(CHECKPOINT) {
                anyhow::ensure!(
                    saved.len() == 1 && checkpoint.sequence == 1,
                    "invalid merge checkpoint ownership"
                );
                let saved: Saved = decode_bounded_messagepack(
                    checkpoint
                        .source_position
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("missing merge snapshot"))?,
                    self.snapshot_limit()?,
                )?;
                self.restore(saved)?;
            } else {
                anyhow::ensure!(
                    saved.is_empty()
                        && store.read_config_hash().await?.is_none()
                        && store
                            .read_result_sequence(self.definition.id.as_str())
                            .await?
                            .is_none()
                        && transaction
                            .resources()
                            .outbox_writer()
                            .expect("atomic outbox")
                            .read_from(self.definition.id.as_str(), 0)
                            .await?
                            .is_empty()
                        && transaction
                            .resources()
                            .live_results_writer()
                            .expect("atomic projection")
                            .read_snapshot(self.definition.id.as_str())
                            .await?
                            .is_empty(),
                    "time merge refuses nonempty unowned storage"
                );
                self.save().await?;
            }
        }
        self.bindings
            .publish(self.state.destinations.clone(), self.count() > 0)?;
        self.loaded = true;
        guard.complete = true;
        self.publish();
        Ok(())
    }

    fn restore(&mut self, saved: Saved) -> anyhow::Result<()> {
        anyhow::ensure!(
            saved.version == 1
                && saved.definition == self.definition
                && saved.schema == *self.schema.descriptor()
                && saved.sources.len() == self.definition.sources.len(),
            "merge configuration, schema or storage version changed"
        );
        let validate_identity =
            |identity: &GraphProducerIdentity, stream: &StreamId| -> anyhow::Result<()> {
                identity.validate()?;
                anyhow::ensure!(
                    identity.persistent()
                        && identity.stream() == stream
                        && identity.graph_id() == self.definition.graph_id
                        && identity.construction_scope() == self.definition.graph_id
                        && identity.component_id() == &self.definition.id,
                    "invalid persistent merge producer identity"
                );
                Ok(())
            };
        validate_identity(&saved.identity, &self.definition.output_stream)?;
        match (&saved.late_identity, &self.definition.late_output_stream) {
            (Some(identity), Some(stream)) => validate_identity(identity, stream)?,
            (None, None) => {}
            _ => anyhow::bail!("invalid merge late producer identity"),
        }
        anyhow::ensure!(
            (saved.logical_sequence > 0) == saved.last_emitted.is_some()
                && saved.late_events
                    == saved
                        .late_sequence
                        .checked_add(saved.discarded)
                        .and_then(|count| count.checked_add(u64::from(saved.held.is_some())))
                        .ok_or_else(|| anyhow::anyhow!("merge late count overflow"))?
                && (saved.late_sequence == 0
                    || self.definition.late_policy == LateEventPolicy::Route),
            "inconsistent merge frontier or late counters"
        );
        for (rank, source) in saved.sources.iter().enumerate() {
            anyhow::ensure!(
                if source.sequence == 0 {
                    source.max_time.is_none()
                        && source.progress_key.is_none()
                        && source.producer.is_none()
                        && source.digest == [0; 32]
                } else {
                    source.max_time.is_some()
                        && source
                            .progress_key
                            .as_ref()
                            .is_some_and(|key| !key.is_empty())
                },
                "invalid merge source receipt"
            );
            anyhow::ensure!(
                source.progress_key.as_ref().map_or(0, String::len)
                    + serde_json::to_vec(&source.producer)?.len()
                    <= MAX_SOURCE_IDENTITY_BYTES,
                "oversized merge source receipt"
            );
            if let Some(identity) = &source.producer {
                identity.validate()?;
                anyhow::ensure!(
                    identity.persistent() && identity.stream() == &self.definition.sources[rank],
                    "invalid retained merge input identity"
                );
            }
        }
        let now = Instant::now();
        let decode = |bytes: &[u8]| -> anyhow::Result<(Key, Buffered)> {
            let envelope = self.codec.decode(bytes)?;
            let time = envelope
                .system()
                .timestamp()
                .ok_or(SourceTimeMergeError::MissingSourceTime)?;
            let rank = self
                .definition
                .sources
                .iter()
                .position(|source| source == envelope.system().stream())
                .ok_or_else(|| {
                    SourceTimeMergeError::UnknownSource(envelope.system().stream().clone())
                })?;
            let progress = GraphInputProgress::from_envelope(&envelope)?;
            progress.validate_durability(true)?;
            let source = &saved.sources[rank];
            anyhow::ensure!(
                source.progress_key.as_ref() == Some(&progress.key)
                    && source.producer == progress.producer,
                "retained merge input identity changed"
            );
            if progress.sequence == source.sequence {
                let digest: [u8; 32] =
                    Sha256::digest(self.codec.encode(&envelope.reemit(1)?)?).into();
                anyhow::ensure!(
                    source.digest == digest,
                    "retained merge receipt content changed"
                );
            }
            anyhow::ensure!(
                progress.sequence > 0 && progress.sequence <= saved.sources[rank].sequence,
                "invalid retained merge sequence"
            );
            anyhow::ensure!(
                saved.sources[rank]
                    .max_time
                    .is_some_and(|high| time <= high),
                "invalid retained merge source time"
            );
            Ok((
                (time, rank, progress.sequence),
                Buffered {
                    envelope,
                    bytes: bytes.len(),
                    deadline: now + Duration::from_millis(self.definition.max_wait_ms),
                },
            ))
        };
        let mut buffer = BTreeMap::new();
        let mut deadlines = BTreeSet::new();
        let mut bytes = 0usize;
        let mut sequences = BTreeSet::new();
        for encoded in &saved.buffered {
            let (key, entry) = decode(encoded)?;
            anyhow::ensure!(
                sequences.insert((key.1, key.2)),
                "duplicate retained merge input sequence"
            );
            bytes = bytes
                .checked_add(entry.bytes)
                .ok_or_else(|| anyhow::anyhow!("merge byte overflow"))?;
            deadlines.insert((entry.deadline, key));
            anyhow::ensure!(
                buffer.insert(key, entry).is_none(),
                "duplicate retained merge event"
            );
            anyhow::ensure!(
                saved.last_emitted.map_or(true, |last| key.0 >= last) || saved.held == Some(key),
                "retained merge event precedes output frontier without a hold"
            );
        }
        let pending = saved
            .pending
            .as_ref()
            .map(|(encoded, late, sequence)| -> anyhow::Result<Pending> {
                let (key, input) = decode(encoded)?;
                anyhow::ensure!(
                    sequences.insert((key.1, key.2))
                        && *sequence > 0
                        && *sequence
                            == if *late {
                                saved.late_sequence
                            } else {
                                saved.logical_sequence
                            },
                    "invalid pending merge output"
                );
                anyhow::ensure!(
                    if *late {
                        saved.late_identity.is_some()
                            && saved.last_emitted.is_some_and(|last| key.0 < last)
                    } else {
                        saved.last_emitted == Some(key.0)
                    },
                    "pending merge output has an inconsistent frontier"
                );
                bytes = bytes
                    .checked_add(input.bytes)
                    .ok_or_else(|| anyhow::anyhow!("merge byte overflow"))?;
                Ok(Pending {
                    input,
                    late: *late,
                    logical_sequence: *sequence,
                })
            })
            .transpose()?;
        if let Some(key) = saved.held {
            anyhow::ensure!(
                self.definition.late_policy == LateEventPolicy::FailAndRetain
                    && buffer.contains_key(&key)
                    && pending.is_none()
                    && saved.last_emitted.is_some_and(|last| key.0 < last),
                "invalid held merge event"
            );
        }
        anyhow::ensure!(
            buffer.len() + usize::from(pending.is_some())
                <= self.definition.max_buffered_events.get()
                && bytes <= self.definition.max_buffered_bytes.get(),
            "retained merge buffer exceeds configured bounds"
        );
        anyhow::ensure!(
            saved
                .logical_sequence
                .checked_add(saved.late_sequence)
                .is_some_and(|total| saved.emission_sequence >= total.saturating_sub(1)),
            "invalid merge transport sequence"
        );
        self.state = State {
            sources: saved.sources,
            active_at: vec![now; self.definition.sources.len()],
            buffer,
            deadlines,
            bytes,
            pending,
            held: saved.held,
            last_emitted: saved.last_emitted,
            logical_sequence: saved.logical_sequence,
            late_sequence: saved.late_sequence,
            emission_sequence: saved.emission_sequence,
            late_events: saved.late_events,
            discarded: saved.discarded,
            identity: saved.identity,
            late_identity: saved.late_identity,
            destinations: OutputBindings::try_new(saved.destinations)?,
        };
        Ok(())
    }
}
