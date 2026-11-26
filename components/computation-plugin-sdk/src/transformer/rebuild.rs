// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Replacing installed state by reading the single input again, after a
//! history gap or when a dependent installed the view before its first input
//! connected. The replacement commits as one keyed difference under the same
//! output epoch, so a dependent following this output continues without a
//! gap or a duplicate.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    BootstrapCompletion, ChangeOperation, ComponentId, GraphChangeCodec, InputEnvelope,
    OutputCursor, QueryRecoveryPolicy as RecoveryPolicy, RecordImage, SourceProgressKey,
    BOOTSTRAP_COMPLETE_CHECKPOINT, BOOTSTRAP_STATE_CHECKPOINT,
};

use super::{
    apply_operations,
    engine::{
        bound, codec, delivered_sequence, number, stage_output, Engine, AWAITING_FIRST_INPUT,
        ROW_COUNT,
    },
    logical_sequence, Phase, Rows, TransformerLogic,
};
use crate::NativeProcessing;

// Keep scan calls out of the installation transaction. A larger rebuild needs
// a storage-backed staging generation, not an unbounded Vec or a partial install.
const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;
const MAX_STAGED_BATCHES: usize = 1024;

pub(super) struct Rebuild {
    /// The installed input cut, or `None` when this is the first input.
    after: Option<OutputCursor>,
    batches: Vec<(Vec<u8>, Option<ComponentId>)>,
    bytes: usize,
}

impl Rebuild {
    pub(super) fn push(&mut self, input: InputEnvelope, scope: Option<ComponentId>) -> Result<()> {
        anyhow::ensure!(
            input.envelope.system().control().is_none()
                && input
                    .envelope
                    .changes()
                    .operations()
                    .iter()
                    .all(|op| matches!(op, ChangeOperation::Added { after, .. }
                        if after.image() == RecordImage::Full)),
            "replacement state must contain only full-image additions"
        );
        let bytes = codec()?.encode(&input.envelope)?;
        anyhow::ensure!(
            self.batches.len() < MAX_STAGED_BATCHES && bytes.len() <= MAX_STAGED_BYTES - self.bytes,
            "replacement state exceeds the atomic rebuild budget"
        );
        self.bytes += bytes.len();
        self.batches.push((bytes, scope));
        Ok(())
    }
}

impl<L: TransformerLogic> Engine<L> {
    pub(super) fn begin_rebuild(&mut self, after: Option<OutputCursor>) -> Result<bool> {
        let eligible = !self.volatile
            && !self.non_atomic
            && self.bootstrap_complete
            && self.source_transactions.is_none()
            && self.logic.rebuild_supported()
            && match &after {
                // Replacing data after a gap is a recovery decision. Reading a
                // first input is initialization, which Strict also permits.
                Some(after) => {
                    self.recovery == RecoveryPolicy::AutoReset && after.producer().persistent()
                }
                None => self.first_input_rebuild,
            };
        if !eligible {
            return Ok(false);
        }
        anyhow::ensure!(
            self.pending.is_none() && self.sequence == self.delivered,
            "bootstrap rebuild cannot discard unconfirmed output"
        );
        anyhow::ensure!(
            self.rebuild.is_none(),
            "a transformer rebuild is already in progress"
        );
        self.first_input_rebuild = false;
        self.rebuild = Some(Rebuild {
            after,
            batches: Vec::new(),
            bytes: 0,
        });
        Ok(true)
    }

    pub(super) async fn complete_or_rebuild(
        &mut self,
        processing: &NativeProcessing,
        completion: BootstrapCompletion,
    ) -> Result<()> {
        let Some(rebuild) = self.rebuild.take() else {
            anyhow::ensure!(
                !self.first_input_rebuild,
                "the connected first input was not read; its subscribers would miss its rows"
            );
            return self.complete_bootstrap(processing, completion).await;
        };
        let [cut] = completion.watermarks.as_slice() else {
            anyhow::bail!("atomic rebuild needs exactly one output cut")
        };
        cut.validate()?;
        let upstream = cut
            .producer
            .clone()
            .context("atomic rebuild needs an output cut")?;
        match &rebuild.after {
            Some(after) => anyhow::ensure!(
                after.producer() == &upstream && cut.sequence > after.sequence(),
                "replacement snapshot did not advance the installed producer"
            ),
            None => anyhow::ensure!(
                upstream.persistent(),
                "a durable first input requires a persistent producer"
            ),
        }
        let installed = rebuild.after.as_ref().map(OutputCursor::sequence);
        let head = cut.sequence;
        let handover = completion
            .state
            .context("replacement snapshot has no handover state")?;
        let output = self.output.clone();
        let expected = self.sequence;
        let old = output.rows_at(&OutputCursor::try_new(self.producer.clone(), expected)?)?;
        let logic = self.logic.clone();
        let stream = self.stream.clone();
        let retention = u64::try_from(self.capacity.get())?;
        let observations = self.observations.clone();
        let replacement = Arc::new(Mutex::new(None));
        let staged = replacement.clone();
        let bytes: Option<Vec<u8>> = processing
            .run_output(move |context| async move {
                let (context, _binding) = bound(&*logic, context)?;
                let key = SourceProgressKey::Stream(upstream.stream().clone());
                let checkpoint = context.read_input_checkpoint(upstream.stream()).await?;
                anyhow::ensure!(
                    number(&context, BOOTSTRAP_COMPLETE_CHECKPOINT).await? == 1
                        && match installed {
                            Some(installed) => checkpoint.is_some_and(|c| {
                                c.sequence == installed && c.source_position.is_none()
                            }),
                            None => checkpoint.is_none(),
                        }
                        && context.read_result_sequence().await? == Some(expected)
                        && delivered_sequence(&context, false).await? == expected
                        && number(&context, ROW_COUNT).await? == u64::try_from(old.len())?,
                    "rebuild no longer matches installed input or output"
                );
                if installed.is_none() {
                    anyhow::ensure!(
                        context
                            .read_checkpoint(AWAITING_FIRST_INPUT)
                            .await?
                            .is_some_and(|m| m.sequence == 1 && m.source_position.is_none()),
                        "first input was already initialized"
                    );
                    context
                        .stage_checkpoint(AWAITING_FIRST_INPUT, 0, None)
                        .await?;
                }
                context.bind_input_producer(&upstream, &key).await?;
                // Private stores restart empty; the replacement state reads
                // the input again from its cut.
                context.rebuild_evaluator().await?;
                let next = logic.open(&context).await?;
                let codec = codec()?;
                let mut rows = Rows::new();
                for (bytes, scope) in rebuild.batches {
                    let envelope = codec.decode(&bytes)?;
                    let mut prepared = logic.prepare(&envelope, scope, None, Phase::Bootstrap)?;
                    let evaluation = logic
                        .evaluate(&next, &mut prepared, Phase::Bootstrap, &observations)
                        .await?;
                    if let Some(batch) = logic.project(&envelope, prepared, evaluation, 1)? {
                        apply_operations(&mut rows, batch.changes().operations())?;
                    }
                }
                let sequence = expected
                    .checked_add(1)
                    .context("transformer output sequence exhausted")?;
                let bytes =
                    match logic.difference(&old, &rows, &stream, sequence, upstream.stream())? {
                        Some(delta) => Some(
                            stage_output(
                                &context, &*logic, &output, delta, sequence, retention, false,
                                false,
                            )
                            .await?,
                        ),
                        None => None,
                    };
                context.stage_bootstrap_watermark(&key, head, None).await?;
                context
                    .stage_checkpoint(BOOTSTRAP_STATE_CHECKPOINT, 1, Some(&handover))
                    .await?;
                *staged
                    .lock()
                    .map_err(|_| anyhow::anyhow!("replacement state poisoned"))? = Some(next);
                Ok(bytes)
            })
            .await?;
        self.state = Some(
            replacement
                .lock()
                .map_err(|_| anyhow::anyhow!("replacement state poisoned"))?
                .take()
                .context("missing committed replacement state")?,
        );
        if let Some(bytes) = bytes {
            let output = codec()?.decode(&bytes)?;
            let logic = self.logic.clone();
            self.output
                .publish(&output, &|envelope, producer, generation| {
                    logic.verify(envelope, producer, generation)
                })?;
            self.sequence = logical_sequence(&output)?;
            self.pending = Some(output);
            self.replay = true;
        }
        self.refresh_schedule(processing).await?;
        self.output.activate()
    }
}
