// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Non-atomic publication: evaluation commits first, then its output and the
//! consumed input progress. A marker names the position in between so a
//! restart can tell an interrupted publication from a completed one.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{telemetry::QueryObservationKind, InputEnvelope};
use drasi_core_types::interface::SourceCheckpoint;

use super::{
    engine::{codec, stage_output, Engine},
    observations::Observations,
    output::TransformerOutput,
    recovery, Phase, TransformerLogic,
};
use crate::{NativeProcessing, NativeProcessingContext};

/// The publication marker the host's processing provider also recognizes.
pub(super) const PENDING_OUTPUT: &str = "\0computation:pending-output:v1";

/// Non-atomic publication is incomplete from evaluation until its output and
/// input progress commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Publication {
    Evaluating,
    Pending(u64),
    Complete,
}

impl Publication {
    fn checkpoint(self) -> SourceCheckpoint {
        match self {
            Self::Evaluating => {
                SourceCheckpoint::new(0, Some(Bytes::from_static(b"evaluating-v1")))
            }
            Self::Pending(sequence) => SourceCheckpoint::new(sequence, None),
            Self::Complete => SourceCheckpoint::new(0, None),
        }
    }

    pub(super) fn is_incomplete(checkpoint: Option<&SourceCheckpoint>) -> bool {
        checkpoint.is_some_and(|marker| marker.sequence > 0 || marker.source_position.is_some())
    }
}

pub(super) async fn stage(context: &NativeProcessingContext, state: Publication) -> Result<()> {
    let checkpoint = state.checkpoint();
    context
        .stage_checkpoint(
            PENDING_OUTPUT,
            checkpoint.sequence,
            checkpoint.source_position.as_deref(),
        )
        .await?;
    Ok(())
}

pub(super) async fn begin(processing: &NativeProcessing) -> Result<()> {
    processing
        .run(|context| async move { stage(&context, Publication::Evaluating).await })
        .await
}

/// Mark a new input as being evaluated. Returns whether it is a duplicate.
pub(super) async fn begin_input<L: TransformerLogic>(
    processing: &NativeProcessing,
    logic: Arc<L>,
    input: InputEnvelope,
) -> Result<bool> {
    processing
        .run(move |context| async move {
            if context.validate_input_progress(&input).await?.duplicate {
                anyhow::ensure!(
                    context.prepare_input_progress(&input).await?.duplicate,
                    "duplicate input changed during publication preparation"
                );
                return Ok(true);
            }
            logic.prepare(&input.envelope, None, None, Phase::Steady)?;
            stage(&context, Publication::Evaluating).await?;
            Ok(false)
        })
        .await
}

pub(super) async fn finish<L: TransformerLogic>(
    processing: &NativeProcessing,
    logic: Arc<L>,
    state: Arc<TransformerOutput>,
    retention: u64,
    prepared: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let observations = Observations::capture()?;
    processing
        .run_output(move |context| async move {
            let pending = context
                .read_checkpoint(PENDING_OUTPUT)
                .await?
                .context("non-atomic evaluation has no publication marker")?;
            let output = super::engine::decode(prepared)?;
            let expected = match &output {
                Some(output) => Publication::Pending(output.system().sequence()),
                None => Publication::Evaluating,
            }
            .checkpoint();
            anyhow::ensure!(
                pending.sequence == expected.sequence
                    && pending.source_position == expected.source_position,
                "non-atomic output does not match the committed evaluation"
            );
            observations
                .phase(QueryObservationKind::CommitWork, Phase::Steady, async {
                    context.resume_input_progress().await?;
                    let output = match output {
                        Some(output) => {
                            let sequence = output.system().sequence();
                            Some(
                                stage_output(
                                    &context, &*logic, &state, output, sequence, retention, false,
                                    false,
                                )
                                .await?,
                            )
                        }
                        None => None,
                    };
                    stage(&context, Publication::Complete).await?;
                    Ok(output)
                })
                .await
        })
        .await
}

impl<L: TransformerLogic> Engine<L> {
    pub(super) async fn prepare_non_atomic_reset(
        &self,
        processing: &NativeProcessing,
        force: bool,
    ) -> Result<()> {
        let configuration = self.configuration.clone();
        let candidate = self.producer.clone();
        let policy = self.recovery;
        let bootstrap = self.bootstrap_configured;
        let unconnected = self.unconnected_inputs;
        let reset_configuration = self.reset_configuration;
        let update_operation = self.update_operation.clone();
        let logic = self.logic.clone();
        processing
            .run(move |context| async move {
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    profile.non_atomic_publication
                        && !profile.volatile
                        && profile.persistent_checkpoints,
                    "non-atomic publication requires its actual persistent profile"
                );
                if recovery::reset_required(
                    &context,
                    &configuration,
                    &candidate,
                    recovery::ResetIntent {
                        policy,
                        bootstrap_configured: bootstrap,
                        unconnected,
                        force,
                        reset_configuration,
                        update_operation: update_operation.as_deref(),
                    },
                    &|output, producer| logic.verify_owner(output, producer),
                )
                .await?
                .is_some()
                {
                    context.begin_non_atomic_reset(true).await?;
                }
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_markers_distinguish_interrupted_from_complete() {
        let evaluating = Publication::Evaluating.checkpoint();
        assert_eq!(evaluating.sequence, 0);
        assert_eq!(
            evaluating.source_position.as_deref(),
            Some(b"evaluating-v1".as_slice())
        );
        for state in [Publication::Evaluating, Publication::Pending(7)] {
            assert!(Publication::is_incomplete(Some(&state.checkpoint())));
        }
        assert!(!Publication::is_incomplete(None));
        assert!(!Publication::is_incomplete(Some(
            &Publication::Complete.checkpoint()
        )));
    }
}
