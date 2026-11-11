// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::{HashMap, VecDeque};

use anyhow::Context;
use drasi_computation_contracts::{
    ChangeEnvelope, ChangeOperation, GraphProducerProgress, OutputCursor, QueryChangeCodec,
    QueryRecoveryError, QueryRecoveryIdentity, QueryRecoveryPolicy, Record, RecordId,
};

/// Only identified query-state recovery errors authorize automatic rebuilding.
/// Storage/transport failures and foreign ownership never do.
pub fn should_reset_query_error(
    error: &anyhow::Error,
    policy: QueryRecoveryPolicy,
    reset_configuration: bool,
) -> bool {
    match error.downcast_ref::<QueryRecoveryError>() {
        None | Some(QueryRecoveryError::OwnershipMismatch) => false,
        Some(QueryRecoveryError::ConfigurationChanged) => {
            policy == QueryRecoveryPolicy::AutoReset || reset_configuration
        }
        Some(_) => policy == QueryRecoveryPolicy::AutoReset,
    }
}

/// A reset may change configuration, but cannot take another producer's state.
pub fn validate_query_reset_owner(
    expected: &QueryRecoveryIdentity,
    stored: Option<&QueryRecoveryIdentity>,
) -> anyhow::Result<()> {
    if let Some(stored) = stored {
        anyhow::ensure!(
            stored.construction_scope() == expected.construction_scope()
                && stored.graph_id() == expected.graph_id()
                && stored.query_id() == expected.query_id()
                && stored.incarnation() == expected.incarnation(),
            QueryRecoveryError::OwnershipMismatch
        );
    }
    Ok(())
}

/// Validate committed output before adopting it. Storage decoding and commit
/// authority stay with the host; the query's existing recovery rules are shared.
pub fn validate_query_recovery(
    cursor: &OutputCursor,
    identity: &QueryRecoveryIdentity,
    generation: u64,
    rows: &im::HashMap<RecordId, Record>,
    retained: &[ChangeEnvelope],
    baseline: bool,
) -> anyhow::Result<()> {
    let mut previous = None;
    let mut expected = HashMap::new();
    for envelope in retained {
        let position = QueryChangeCodec::query_sequence(envelope)?;
        if position == 0 {
            return Err(QueryRecoveryError::Inconsistent(
                "retained output has a zero sequence".into(),
            )
            .into());
        }
        if previous.is_some_and(|previous: u64| previous.checked_add(1) != Some(position)) {
            return Err(QueryRecoveryError::Inconsistent(
                "retained history has an interior gap".into(),
            )
            .into());
        }
        let metadata = QueryChangeCodec::metadata(envelope).with_context(|| {
            QueryRecoveryError::Inconsistent(format!(
                "invalid durable outbox metadata at {position}"
            ))
        })?;
        if envelope.system().stream() != cursor.producer().stream()
            || metadata.query_id != identity.query_id()
        {
            return Err(QueryRecoveryError::OwnershipMismatch.into());
        }
        if QueryChangeCodec::query_generation(envelope).with_context(|| {
            QueryRecoveryError::Inconsistent(format!(
                "invalid durable outbox generation at {position}"
            ))
        })? != generation
        {
            return Err(QueryRecoveryError::Inconsistent(
                "retained output belongs to another reset generation".into(),
            )
            .into());
        }
        match QueryRecoveryIdentity::optional_from_envelope(envelope).with_context(|| {
            QueryRecoveryError::Inconsistent(format!(
                "invalid durable outbox producer identity at {position}"
            ))
        })? {
            Some(stored) if &stored == identity => {}
            Some(_) => return Err(QueryRecoveryError::OwnershipMismatch.into()),
            None => {
                return Err(QueryRecoveryError::Inconsistent(
                    "retained output has no producer identity".into(),
                )
                .into())
            }
        }
        let producer = GraphProducerProgress::from_envelope(envelope)?.ok_or_else(|| {
            QueryRecoveryError::Inconsistent("retained output has no producer progress".into())
        })?;
        if producer.identity() != cursor.producer() {
            return Err(QueryRecoveryError::OwnershipMismatch.into());
        }
        if producer.sequence() != position || position > cursor.sequence() {
            return Err(QueryRecoveryError::Inconsistent(
                "retained output differs from committed progress".into(),
            )
            .into());
        }
        for operation in envelope.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                    expected.insert(after.identity().clone(), Some(after.payload().clone()));
                }
                ChangeOperation::Deleted { identity, .. } => {
                    expected.insert(identity.identity().clone(), None);
                }
            }
        }
        previous = Some(position);
    }
    if cursor.sequence() > 0
        && !(baseline && retained.is_empty())
        && previous != Some(cursor.sequence())
    {
        return Err(QueryRecoveryError::Inconsistent(
            "retained tail differs from committed sequence".into(),
        )
        .into());
    }
    for (id, expected) in expected {
        if rows.get(&id).map(|row| row.payload()) != expected.as_ref() {
            return Err(QueryRecoveryError::Inconsistent(
                "snapshot contradicts retained history".into(),
            )
            .into());
        }
    }
    Ok(())
}

pub fn pending_query_outputs<'a>(
    retained: impl IntoIterator<Item = &'a ChangeEnvelope>,
    sequence: u64,
    delivered: u64,
) -> anyhow::Result<VecDeque<ChangeEnvelope>> {
    anyhow::ensure!(
        delivered <= sequence,
        "query delivery progress is ahead of its output"
    );
    let mut pending = VecDeque::new();
    let mut previous = delivered;
    for envelope in retained {
        let logical = QueryChangeCodec::query_sequence(envelope)?;
        if logical <= delivered {
            continue;
        }
        anyhow::ensure!(
            previous.checked_add(1) == Some(logical),
            "query handoff history was retired before durable acceptance"
        );
        pending.push_back(envelope.clone());
        previous = logical;
    }
    anyhow::ensure!(
        previous == sequence,
        "query handoff history was retired before durable acceptance"
    );
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_policy_requires_a_typed_recoverable_query_error() {
        let corrupt = anyhow::anyhow!("malformed row").context(QueryRecoveryError::Inconsistent(
            "invalid retained row".into(),
        ));
        assert!(should_reset_query_error(
            &corrupt,
            QueryRecoveryPolicy::AutoReset,
            false
        ));
        assert!(!should_reset_query_error(
            &corrupt,
            QueryRecoveryPolicy::Strict,
            true
        ));
        assert!(should_reset_query_error(
            &QueryRecoveryError::ConfigurationChanged.into(),
            QueryRecoveryPolicy::Strict,
            true,
        ));
        for error in [
            QueryRecoveryError::OwnershipMismatch.into(),
            anyhow::anyhow!("storage is unavailable"),
        ] {
            assert!(!should_reset_query_error(
                &error,
                QueryRecoveryPolicy::AutoReset,
                true
            ));
        }
    }

    #[test]
    fn reset_owner_ignores_only_the_configuration_fingerprint() {
        let id = drasi_computation_contracts::ComponentId::try_new("query").expect("query ID");
        let expected =
            QueryRecoveryIdentity::try_new("graph", id.clone(), 1, None).expect("current identity");
        let previous =
            QueryRecoveryIdentity::try_new("graph", id, 2, None).expect("previous identity");
        validate_query_reset_owner(&expected, Some(&previous)).expect("same owner");
        let foreign = previous
            .with_construction_scope("another-instance")
            .expect("foreign scope");
        assert!(validate_query_reset_owner(&expected, Some(&foreign)).is_err());
    }
}
