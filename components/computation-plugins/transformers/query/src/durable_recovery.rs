// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::interface::SourceCheckpoint;

const RESET_BASELINE: &str = "\0native-query:reset-baseline:v1";

#[derive(Clone, Copy)]
pub(super) struct ResetPlan {
    pub(super) sequence: u64,
    pub(super) emission: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ResetIntent<'a> {
    pub(super) policy: QueryRecoveryPolicy,
    pub(super) bootstrap_configured: bool,
    pub(super) force: bool,
    pub(super) reset_configuration: bool,
    pub(super) update_operation: Option<&'a str>,
}

fn update_needs_reset(operation: Option<&str>, previous: Option<&str>, complete: bool) -> bool {
    operation.is_some_and(|operation| previous != Some(operation) || !complete)
}

pub(super) async fn baseline(
    context: &NativeProcessingContext,
) -> Result<Option<drasi_query_common::QueryResetMarker>> {
    let Some(saved) = context.read_checkpoint(RESET_BASELINE).await? else {
        return Ok(None);
    };
    anyhow::ensure!(saved.sequence == 1, "invalid reset baseline version");
    let marker: drasi_query_common::QueryResetMarker = serde_json::from_slice(
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
    let marker = drasi_query_common::QueryResetMarker {
        sequence,
        generation,
        in_progress: false,
    };
    context
        .stage_checkpoint(RESET_BASELINE, 1, Some(&serde_json::to_vec(&marker)?))
        .await?;
    Ok(())
}

pub(super) fn recovery_policy(request: &CreateRequest) -> Result<QueryRecoveryPolicy> {
    match request
        .configuration
        .get("recovery")
        .and_then(serde_json::Value::as_str)
    {
        None if !request
            .configuration
            .as_object()
            .is_some_and(|object| object.contains_key("recovery")) =>
        {
            Ok(QueryRecoveryPolicy::Strict)
        }
        Some("strict") => Ok(QueryRecoveryPolicy::Strict),
        Some("auto_reset") => Ok(QueryRecoveryPolicy::AutoReset),
        _ => anyhow::bail!("recovery must be 'strict' or 'auto_reset'"),
    }
}

pub(super) fn producer(
    saved: SourceCheckpoint,
    candidate: &GraphProducerIdentity,
) -> Result<GraphProducerIdentity> {
    anyhow::ensure!(saved.sequence == 1, "invalid query producer version");
    let producer: GraphProducerIdentity = serde_json::from_slice(
        saved
            .source_position
            .as_deref()
            .context("missing query producer bytes")?,
    )?;
    producer.validate()?;
    anyhow::ensure!(
        producer.persistent()
            && producer.construction_scope() == candidate.construction_scope()
            && producer.graph_id() == candidate.graph_id()
            && producer.component_id() == candidate.component_id()
            && producer.stream() == candidate.stream(),
        "persisted query producer belongs to another scope"
    );
    Ok(producer)
}

pub(super) async fn reset_if_required(
    context: &NativeProcessingContext,
    configuration: &[u8],
    candidate: &GraphProducerIdentity,
    intent: ResetIntent<'_>,
) -> Result<Option<ResetPlan>> {
    let plan = reset_required(context, configuration, candidate, intent).await?;
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
) -> Result<Option<ResetPlan>> {
    let ResetIntent {
        policy,
        bootstrap_configured,
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
                "invalid query update checkpoint version"
            );
            let operation = std::str::from_utf8(
                checkpoint
                    .source_position
                    .as_deref()
                    .context("missing query update operation")?,
            )?;
            drasi_computation_contracts::validate_identifier("stored query update", operation)?;
            Ok(operation)
        })
        .transpose()?;
    let pending_checkpoint = context
        .read_checkpoint(drasi_query_common::PENDING_QUERY_OUTPUT)
        .await?;
    let pending =
        drasi_query_common::QueryPublicationCheckpoint::is_incomplete(pending_checkpoint.as_ref());
    let resetting = context
        .state_info()
        .await?
        .non_atomic_reset
        .is_some_and(|marker| marker.in_progress);
    let Some(saved) = context.read_checkpoint(CONFIGURATION).await? else {
        anyhow::ensure!(
            previous_update.is_none(),
            "query update checkpoint has no matching configuration"
        );
        anyhow::ensure!(!force, "cannot reset an unidentified query");
        anyhow::ensure!(!pending, QueryRecoveryError::PendingPublication);
        anyhow::ensure!(!resetting, QueryRecoveryError::IncompleteReset);
        return Ok(None);
    };
    anyhow::ensure!(saved.sequence == 1, "invalid stored query configuration");
    let changed = saved
        .source_position
        .as_deref()
        .context("missing stored query configuration")?
        != configuration;
    let complete = number(context, BOOTSTRAP_COMPLETE_CHECKPOINT).await?;
    anyhow::ensure!(complete <= 1, "invalid query bootstrap marker");
    anyhow::ensure!(
        !changed || update_operation.is_none() || update_operation != previous_update,
        "query update operation was reused for a different configuration"
    );
    let updating = update_needs_reset(update_operation, previous_update, complete == 1);
    if !changed && complete == 1 && !force && !pending && !resetting && !updating {
        return Ok(None);
    }
    if policy == QueryRecoveryPolicy::Strict && !(changed && reset_configuration || updating) {
        if changed {
            anyhow::bail!("indexed query configuration changed; retained state cannot be reused");
        }
        anyhow::ensure!(!pending, QueryRecoveryError::PendingPublication);
        anyhow::ensure!(!resetting, QueryRecoveryError::IncompleteReset);
        anyhow::bail!("incomplete or refreshed durable bootstrap requires explicit recovery/reset");
    }
    anyhow::ensure!(
        bootstrap_configured,
        "AutoReset requires a declared bootstrap provider"
    );
    producer(
        context
            .read_checkpoint(PRODUCER)
            .await?
            .context("missing persisted query producer")?,
        candidate,
    )?;
    let sequence = context
        .read_result_sequence()
        .await?
        .context("missing query result sequence")?;
    anyhow::ensure!(
        delivered_sequence(context, false).await? == sequence,
        "bootstrap reset cannot discard unconfirmed output"
    );
    let emission = number(context, EMISSION).await?;
    anyhow::ensure!(emission >= sequence, "invalid query emission progress");
    let retained = validate_reset_ownership(context, candidate).await?;
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

#[cfg(test)]
mod update_tests {
    use super::*;

    #[test]
    fn accepted_reset_is_once_per_operation_and_can_resume_incomplete_bootstrap() {
        assert!(update_needs_reset(Some("new"), None, true));
        assert!(update_needs_reset(Some("new"), Some("old"), true));
        assert!(update_needs_reset(Some("new"), Some("new"), false));
        assert!(!update_needs_reset(Some("new"), Some("new"), true));
        assert!(!update_needs_reset(None, Some("old"), false));
    }
}

async fn validate_reset_ownership(
    context: &NativeProcessingContext,
    producer: &GraphProducerIdentity,
) -> Result<u64> {
    let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
    let mut after = 0;
    loop {
        let page = context.read_outbox_page(after, row_page_limits()).await?;
        if page.is_empty() {
            return Ok(after);
        }
        for (sequence, bytes) in page {
            anyhow::ensure!(sequence > after, "reset ownership scan did not advance");
            after = sequence;
            let identity = codec
                .decode(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|output| {
                    drasi_computation_contracts::QueryRecoveryIdentity::optional_from_envelope(
                        &output,
                    )
                    .map_err(anyhow::Error::from)
                });
            match identity {
                Ok(Some(stored)) => {
                    let expected = drasi_computation_contracts::QueryRecoveryIdentity::for_producer(
                        producer, stored.configuration_hash(),
                    )?;
                    drasi_query_common::validate_query_reset_owner(&expected, Some(&stored))?;
                }
                Ok(None) => {}
                Err(error) => log::warn!(
                    "Authorized reset of query '{}' encountered corrupt output {sequence}: {error:#}",
                    producer.component_id(),
                ),
            }
        }
    }
}
