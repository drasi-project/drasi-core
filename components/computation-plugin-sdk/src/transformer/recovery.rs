// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Recovery decisions: whether retained state can be reused, whether it must
//! be reset, and whether the reset may proceed.

use std::collections::{HashMap, VecDeque};

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ChangeEnvelope, ChangeOperation, GraphProducerIdentity, GraphProducerProgress, OutputCursor,
    QueryRecoveryError as RecoveryError, QueryRecoveryPolicy as RecoveryPolicy, UpdateSemantics,
    BOOTSTRAP_COMPLETE_CHECKPOINT,
};
use drasi_core_types::interface::SourceCheckpoint;
use serde::{Deserialize, Serialize};

use super::{
    engine::{
        codec, delivered_sequence, number, row_page_limits, CONFIGURATION, EMISSION, PRODUCER,
        UPDATE_OPERATION,
    },
    logical_sequence,
    non_atomic::{Publication, PENDING_OUTPUT},
    output::{Verify, VerifyOwner},
    Rows,
};
use crate::NativeProcessingContext;

const RESET_BASELINE: &str = "\0transformer:reset-baseline:v1";

/// The output position a completed reset left behind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResetMarker {
    pub(super) sequence: u64,
    pub(super) in_progress: bool,
    pub(super) generation: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ResetPlan {
    pub(super) sequence: u64,
    pub(super) emission: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ResetIntent<'a> {
    pub(super) policy: RecoveryPolicy,
    pub(super) bootstrap_configured: bool,
    pub(super) unconnected: bool,
    pub(super) force: bool,
    pub(super) reset_configuration: bool,
    pub(super) update_operation: Option<&'a str>,
}

fn update_needs_reset(operation: Option<&str>, previous: Option<&str>, complete: bool) -> bool {
    operation.is_some_and(|operation| previous != Some(operation) || !complete)
}

/// Only typed recovery failures may be repaired by a reset; storage and
/// ownership failures never are.
pub(super) fn should_reset(
    error: &anyhow::Error,
    policy: RecoveryPolicy,
    reset_configuration: bool,
) -> bool {
    match error.downcast_ref::<RecoveryError>() {
        None | Some(RecoveryError::OwnershipMismatch) => false,
        Some(RecoveryError::ConfigurationChanged) => {
            policy == RecoveryPolicy::AutoReset || reset_configuration
        }
        Some(_) => policy == RecoveryPolicy::AutoReset,
    }
}

/// Retained history must be contiguous, belong to this producer and
/// generation, end at the committed position and agree with the rows.
pub(super) fn validate(
    cursor: &OutputCursor,
    generation: u64,
    rows: &Rows,
    retained: &[ChangeEnvelope],
    baseline: bool,
    verify: Verify<'_>,
) -> Result<()> {
    let mut previous = None;
    let mut expected = HashMap::new();
    for envelope in retained {
        let producer = GraphProducerProgress::from_envelope(envelope)?.ok_or_else(|| {
            RecoveryError::Inconsistent("retained output has no producer progress".into())
        })?;
        let position = producer.sequence();
        if position == 0 {
            return Err(
                RecoveryError::Inconsistent("retained output has a zero sequence".into()).into(),
            );
        }
        if previous.is_some_and(|previous: u64| previous.checked_add(1) != Some(position)) {
            return Err(
                RecoveryError::Inconsistent("retained history has an interior gap".into()).into(),
            );
        }
        if envelope.system().stream() != cursor.producer().stream()
            || producer.identity() != cursor.producer()
        {
            return Err(RecoveryError::OwnershipMismatch.into());
        }
        if position > cursor.sequence() {
            return Err(RecoveryError::Inconsistent(
                "retained output differs from committed progress".into(),
            )
            .into());
        }
        verify(envelope, cursor.producer(), generation).map_err(|error| {
            if error.downcast_ref::<RecoveryError>().is_some() {
                error
            } else {
                error.context(RecoveryError::Inconsistent(format!(
                    "retained output {position} does not match this transformer"
                )))
            }
        })?;
        for operation in envelope.changes().operations() {
            // A full image is checked by value; a patch only by presence.
            let image = match operation {
                ChangeOperation::Added { after, .. }
                | ChangeOperation::Updated {
                    after,
                    semantics: UpdateSemantics::Replace,
                    ..
                } => Some(Some(after.payload().clone())),
                ChangeOperation::Updated { .. } => Some(None),
                ChangeOperation::Deleted { .. } => None,
            };
            expected.insert(operation.identity().clone(), image);
        }
        previous = Some(position);
    }
    if cursor.sequence() > 0
        && !(baseline && retained.is_empty())
        && previous != Some(cursor.sequence())
    {
        return Err(RecoveryError::Inconsistent(
            "retained tail differs from committed sequence".into(),
        )
        .into());
    }
    for (id, expected) in expected {
        let consistent = match (expected, rows.get(&id)) {
            (None, None) | (Some(None), Some(_)) => true,
            (Some(Some(image)), Some(row)) => row.payload() == &image,
            _ => false,
        };
        if !consistent {
            return Err(RecoveryError::Inconsistent(
                "snapshot contradicts retained history".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// Committed outputs a restart must deliver again, oldest first.
pub(super) fn pending_outputs<'a>(
    retained: impl IntoIterator<Item = &'a ChangeEnvelope>,
    sequence: u64,
    delivered: u64,
) -> Result<VecDeque<ChangeEnvelope>> {
    anyhow::ensure!(
        delivered <= sequence,
        "transformer delivery progress is ahead of its output"
    );
    let mut pending = VecDeque::new();
    let mut previous = delivered;
    for envelope in retained {
        let logical = logical_sequence(envelope)?;
        if logical <= delivered {
            continue;
        }
        anyhow::ensure!(
            previous.checked_add(1) == Some(logical),
            "handoff history was retired before durable acceptance"
        );
        pending.push_back(envelope.clone());
        previous = logical;
    }
    anyhow::ensure!(
        previous == sequence,
        "handoff history was retired before durable acceptance"
    );
    Ok(pending)
}

pub(super) async fn baseline(context: &NativeProcessingContext) -> Result<Option<ResetMarker>> {
    let Some(saved) = context.read_checkpoint(RESET_BASELINE).await? else {
        return Ok(None);
    };
    anyhow::ensure!(saved.sequence == 1, "invalid reset baseline version");
    let marker: ResetMarker = serde_json::from_slice(
        saved
            .source_position
            .as_deref()
            .context("missing reset baseline")?,
    )?;
    anyhow::ensure!(
        !marker.in_progress && marker.generation > 0,
        "invalid reset baseline"
    );
    Ok(Some(marker))
}

pub(super) async fn stage_baseline(context: &NativeProcessingContext, sequence: u64) -> Result<()> {
    let generation = context
        .reset_generation()
        .await?
        .context("reset has no generation")?;
    let marker = ResetMarker {
        sequence,
        generation,
        in_progress: false,
    };
    context
        .stage_checkpoint(RESET_BASELINE, 1, Some(&serde_json::to_vec(&marker)?))
        .await?;
    Ok(())
}

fn same_output(producer: &GraphProducerIdentity, candidate: &GraphProducerIdentity) -> bool {
    producer.construction_scope() == candidate.construction_scope()
        && producer.graph_id() == candidate.graph_id()
        && producer.component_id() == candidate.component_id()
        && producer.stream() == candidate.stream()
}

pub(super) fn producer(
    saved: SourceCheckpoint,
    candidate: &GraphProducerIdentity,
) -> Result<GraphProducerIdentity> {
    anyhow::ensure!(saved.sequence == 1, "invalid transformer producer version");
    let producer: GraphProducerIdentity = serde_json::from_slice(
        saved
            .source_position
            .as_deref()
            .context("missing transformer producer bytes")?,
    )?;
    producer.validate()?;
    anyhow::ensure!(
        producer.persistent() && same_output(&producer, candidate),
        "persisted transformer producer belongs to another scope"
    );
    Ok(producer)
}

pub(super) async fn reset_if_required(
    context: &NativeProcessingContext,
    configuration: &[u8],
    candidate: &GraphProducerIdentity,
    intent: ResetIntent<'_>,
    owner: VerifyOwner<'_>,
) -> Result<Option<ResetPlan>> {
    let plan = reset_required(context, configuration, candidate, intent, owner).await?;
    if plan.is_some() {
        if context.state_info().await?.non_atomic_publication {
            context.reset_non_atomic_state(true).await?;
        } else {
            context.reset_state(true).await?;
        }
    }
    Ok(plan)
}

pub(super) async fn reset_required(
    context: &NativeProcessingContext,
    configuration: &[u8],
    candidate: &GraphProducerIdentity,
    intent: ResetIntent<'_>,
    owner: VerifyOwner<'_>,
) -> Result<Option<ResetPlan>> {
    let ResetIntent {
        policy,
        bootstrap_configured,
        unconnected,
        force,
        reset_configuration,
        update_operation,
    } = intent;
    let previous_update = context.read_checkpoint(UPDATE_OPERATION).await?;
    let previous_update = previous_update
        .as_ref()
        .map(|checkpoint| -> Result<&str> {
            anyhow::ensure!(
                checkpoint.sequence == 1,
                "invalid transformer update checkpoint version"
            );
            let operation = std::str::from_utf8(
                checkpoint
                    .source_position
                    .as_deref()
                    .context("missing transformer update operation")?,
            )?;
            drasi_computation_contracts::validate_identifier("stored update", operation)?;
            Ok(operation)
        })
        .transpose()?;
    let pending_checkpoint = context.read_checkpoint(PENDING_OUTPUT).await?;
    let pending = Publication::is_incomplete(pending_checkpoint.as_ref());
    let resetting = context
        .state_info()
        .await?
        .non_atomic_reset
        .is_some_and(|marker| marker.in_progress);
    let Some(saved) = context.read_checkpoint(CONFIGURATION).await? else {
        anyhow::ensure!(
            previous_update.is_none(),
            "update checkpoint has no matching configuration"
        );
        anyhow::ensure!(!force, "cannot reset an unidentified transformer");
        anyhow::ensure!(!pending, RecoveryError::PendingPublication);
        anyhow::ensure!(!resetting, RecoveryError::IncompleteReset);
        return Ok(None);
    };
    anyhow::ensure!(
        saved.sequence == 1,
        "invalid stored transformer configuration"
    );
    let changed = saved
        .source_position
        .as_deref()
        .context("missing stored transformer configuration")?
        != configuration;
    let complete = number(context, BOOTSTRAP_COMPLETE_CHECKPOINT).await?;
    anyhow::ensure!(complete <= 1, "invalid transformer bootstrap marker");
    anyhow::ensure!(
        !changed || update_operation.is_none() || update_operation != previous_update,
        "update operation was reused for a different configuration"
    );
    let updating = update_needs_reset(update_operation, previous_update, complete == 1);
    if !changed && complete == 1 && !force && !pending && !resetting && !updating {
        return Ok(None);
    }
    anyhow::ensure!(
        !unconnected,
        "cannot rebuild retained state without a bound seed or input"
    );
    if policy == RecoveryPolicy::Strict && !(changed && reset_configuration || updating) {
        if changed {
            anyhow::bail!("transformer configuration changed; retained state cannot be reused");
        }
        anyhow::ensure!(!pending, RecoveryError::PendingPublication);
        anyhow::ensure!(!resetting, RecoveryError::IncompleteReset);
        anyhow::bail!("incomplete or refreshed durable bootstrap requires explicit recovery/reset");
    }
    anyhow::ensure!(
        bootstrap_configured || updating,
        "unmanaged recovery reset requires a declared bootstrap provider"
    );
    producer(
        context
            .read_checkpoint(PRODUCER)
            .await?
            .context("missing persisted transformer producer")?,
        candidate,
    )?;
    let sequence = context
        .read_result_sequence()
        .await?
        .context("missing transformer result sequence")?;
    anyhow::ensure!(
        delivered_sequence(context, false).await? == sequence,
        "bootstrap reset cannot discard unconfirmed output"
    );
    let emission = number(context, EMISSION).await?;
    anyhow::ensure!(
        emission >= sequence,
        "invalid transformer emission progress"
    );
    let retained = validate_reset_ownership(context, candidate, owner).await?;
    let sequence = sequence
        .max(retained)
        .max(pending_checkpoint.map_or(0, |checkpoint| checkpoint.sequence))
        .max(baseline(context).await?.map_or(0, |marker| marker.sequence));
    // Prepared non-atomic output can reserve a logical position before it gets
    // a transport emission. Neither counter may reuse that position on reset.
    Ok(Some(ResetPlan {
        sequence,
        emission: emission.max(sequence),
    }))
}

/// A reset may discard only history this output produced.
async fn validate_reset_ownership(
    context: &NativeProcessingContext,
    candidate: &GraphProducerIdentity,
    owner: VerifyOwner<'_>,
) -> Result<u64> {
    let codec = codec()?;
    let mut after = 0;
    loop {
        let page = context.read_outbox_page(after, row_page_limits()).await?;
        if page.is_empty() {
            return Ok(after);
        }
        for (sequence, bytes) in page {
            anyhow::ensure!(sequence > after, "reset ownership scan did not advance");
            after = sequence;
            let output = codec
                .decode(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|output| {
                    let progress = GraphProducerProgress::from_envelope(&output)?;
                    Ok((output, progress))
                });
            match output {
                Ok((output, progress)) => {
                    anyhow::ensure!(
                        progress.is_none_or(|progress| same_output(progress.identity(), candidate)),
                        RecoveryError::OwnershipMismatch
                    );
                    owner(&output, candidate)?;
                }
                Err(error) => log::warn!(
                    "Authorized reset of transformer '{}' encountered corrupt output {sequence}: {error:#}",
                    candidate.component_id(),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_reset_is_once_per_operation_and_can_resume_incomplete_bootstrap() {
        assert!(update_needs_reset(Some("new"), None, true));
        assert!(update_needs_reset(Some("new"), Some("old"), true));
        assert!(update_needs_reset(Some("new"), Some("new"), false));
        assert!(!update_needs_reset(Some("new"), Some("new"), true));
        assert!(!update_needs_reset(None, Some("old"), false));
    }

    #[test]
    fn only_typed_recovery_failures_reset() {
        let corrupt = anyhow::anyhow!("malformed row")
            .context(RecoveryError::Inconsistent("invalid retained row".into()));
        assert!(should_reset(&corrupt, RecoveryPolicy::AutoReset, false));
        assert!(!should_reset(&corrupt, RecoveryPolicy::Strict, true));
        assert!(should_reset(
            &RecoveryError::ConfigurationChanged.into(),
            RecoveryPolicy::Strict,
            true,
        ));
        for error in [
            RecoveryError::OwnershipMismatch.into(),
            anyhow::anyhow!("storage is unavailable"),
        ] {
            assert!(!should_reset(&error, RecoveryPolicy::AutoReset, true));
        }
    }
}
