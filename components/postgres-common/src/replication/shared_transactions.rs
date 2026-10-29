// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configured-slot whole-commit progress. This format is distinct from both
//! single-owner transaction positions and row/snapshot positions.

use super::*;
use drasi_computation_contracts::{ConsumerProgressSnapshot, MAX_SOURCE_POSITION_BYTES};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionBoundary {
    WholeCommit,
    ConfiguredSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedTransactionPosition {
    pub version: u32,
    pub mode: TransactionBoundary,
    pub binding: [u8; 32],
    pub commit_lsn: u64,
    pub sequence: u64,
}

impl SharedTransactionPosition {
    pub fn commit(binding: [u8; 32], commit_lsn: u64, sequence: u64) -> Result<Self> {
        let position = Self {
            version: 1,
            mode: TransactionBoundary::WholeCommit,
            binding,
            commit_lsn,
            sequence,
        };
        position.validate()?;
        Ok(position)
    }

    pub fn configured_snapshot(binding: [u8; 32], boundary: u64) -> Result<Self> {
        let position = Self {
            version: 1,
            mode: TransactionBoundary::ConfiguredSnapshot,
            binding,
            commit_lsn: boundary,
            sequence: 0,
        };
        position.validate()?;
        Ok(position)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == 1
                && self.commit_lsn > 0
                && match self.mode {
                    TransactionBoundary::WholeCommit => self.sequence > 0,
                    TransactionBoundary::ConfiguredSnapshot => self.sequence == 0,
                },
            "invalid PostgreSQL shared whole-transaction position"
        );
        Ok(())
    }

    pub fn is_configured_snapshot(&self) -> bool {
        self.mode == TransactionBoundary::ConfiguredSnapshot
    }
}

pub fn transaction_binding(base: [u8; 32]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"drasi.postgres/shared-transactions/v1\0");
    digest.update(base);
    digest.finalize().into()
}

pub fn compare_positions(
    left: &SharedTransactionPosition,
    right: &SharedTransactionPosition,
) -> Result<Ordering> {
    left.validate()?;
    right.validate()?;
    anyhow::ensure!(
        left.binding == right.binding,
        "cannot compare PostgreSQL transaction positions from different source bindings"
    );
    Ok(left.commit_lsn.cmp(&right.commit_lsn))
}

pub fn read_position(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
) -> Result<Option<SharedTransactionPosition>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.to_string()))
    else {
        return Ok(None);
    };
    let bytes = checkpoint
        .source_position
        .as_deref()
        .context("PostgreSQL transaction checkpoint has no position")?;
    anyhow::ensure!(
        bytes.len() <= MAX_SOURCE_POSITION_BYTES,
        "PostgreSQL transaction cursor exceeds its byte bound"
    );
    let position: SharedTransactionPosition = serde_json::from_slice(bytes)?;
    position.validate()?;
    anyhow::ensure!(
        position.sequence == checkpoint.sequence,
        "PostgreSQL shared transaction checkpoint sequence mismatch"
    );
    Ok(Some(position))
}

pub fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<SharedTransactionPosition>> {
    let position = read_position(snapshot, source)?;
    if let Some(position) = &position {
        anyhow::ensure!(
            position.binding == binding,
            "PostgreSQL shared transaction source binding mismatch"
        );
    }
    Ok(position)
}

#[derive(Debug)]
pub struct TransactionProgressMinimum {
    pub consumer: ComponentId,
    pub position: SharedTransactionPosition,
}

/// Select an actual member's physical boundary without replacing any member or
/// interpreting its independent logical counter as WAL order.
pub fn minimum_checkpoint(
    snapshot: &ConsumerProgressSnapshot,
    expected: &BTreeSet<ComponentId>,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<TransactionProgressMinimum>> {
    let mut fenced = expected.is_empty() || !snapshot.consumers.keys().eq(expected.iter());
    let mut minimum: Option<TransactionProgressMinimum> = None;
    for (consumer, state) in &snapshot.consumers {
        if let Some(failure) = &state.failure {
            anyhow::bail!("PostgreSQL consumer {consumer} cannot provide committed transaction progress: {failure}");
        }
        fenced |= !state.ready || !state.recovered || !state.admitting;
        let Some(position) = checkpoint(state, source, binding).with_context(|| {
            format!("invalid PostgreSQL transaction progress for consumer {consumer}")
        })?
        else {
            fenced = true;
            continue;
        };
        if minimum
            .as_ref()
            .is_none_or(|current| position.commit_lsn < current.position.commit_lsn)
        {
            minimum = Some(TransactionProgressMinimum {
                consumer: consumer.clone(),
                position,
            });
        }
    }
    Ok(if fenced { None } else { minimum })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::interface::SourceCheckpoint;
    use std::sync::Arc;

    fn id(value: &str) -> ComponentId {
        ComponentId::try_new(value).unwrap()
    }
    fn state(position: SharedTransactionPosition) -> Arc<SourceProgressSnapshot> {
        Arc::new(SourceProgressSnapshot {
            ready: true,
            recovered: true,
            admitting: true,
            persistent: true,
            checkpoints: BTreeMap::from([(
                SourceProgressKey::Source("source".into()),
                SourceCheckpoint::new(
                    position.sequence,
                    Some(serde_json::to_vec(&position).unwrap().into()),
                ),
            )]),
            ..Default::default()
        })
    }

    #[test]
    fn physical_minimum_preserves_independent_whole_commit_positions() -> Result<()> {
        let binding = transaction_binding([1; 32]);
        let left = SharedTransactionPosition::commit(binding, 100, 9000)?;
        let right = SharedTransactionPosition::commit(binding, 200, 2)?;
        let mut snapshot = ConsumerProgressSnapshot {
            consumers: BTreeMap::from([
                (id("left"), state(left.clone())),
                (id("right"), state(right)),
            ]),
        };
        let expected = snapshot.consumers.keys().cloned().collect();
        let minimum = minimum_checkpoint(&snapshot, &expected, &id("source"), binding)?.unwrap();
        assert_eq!(minimum.consumer, id("left"));
        assert_eq!(minimum.position, left);
        assert_eq!(
            compare_positions(
                &left,
                &SharedTransactionPosition::configured_snapshot(binding, 100)?
            )?,
            Ordering::Equal
        );
        Arc::make_mut(snapshot.consumers.get_mut(&id("right")).unwrap()).ready = false;
        assert!(minimum_checkpoint(&snapshot, &expected, &id("source"), binding)?.is_none());
        Arc::make_mut(snapshot.consumers.get_mut(&id("right")).unwrap()).failure =
            Some("failed".into());
        assert!(minimum_checkpoint(&snapshot, &expected, &id("source"), binding).is_err());
        Ok(())
    }

    #[test]
    fn bounded_positions_and_missing_members_do_not_invent_a_checkpoint() -> Result<()> {
        let binding = transaction_binding([1; 32]);
        let position = SharedTransactionPosition::commit(binding, 100, 7)?;
        let mut snapshot = ConsumerProgressSnapshot {
            consumers: BTreeMap::from([(id("left"), state(position.clone()))]),
        };
        let expected = BTreeSet::from([id("left"), id("right")]);
        assert!(minimum_checkpoint(&snapshot, &expected, &id("source"), binding)?.is_none());
        snapshot.consumers.insert(
            id("right"),
            Arc::new(SourceProgressSnapshot {
                ready: true,
                recovered: true,
                admitting: true,
                persistent: true,
                ..Default::default()
            }),
        );
        assert!(minimum_checkpoint(&snapshot, &expected, &id("source"), binding)?.is_none());
        let key = SourceProgressKey::Source("source".into());
        let mut invalid = state(position.clone());
        Arc::make_mut(&mut invalid).checkpoints.insert(
            key.clone(),
            SourceCheckpoint::new(8, Some(serde_json::to_vec(&position)?.into())),
        );
        assert!(read_position(&invalid, &id("source")).is_err());
        let mut oversized = serde_json::to_vec(&position)?;
        oversized.resize(MAX_SOURCE_POSITION_BYTES + 1, b' ');
        Arc::make_mut(&mut invalid)
            .checkpoints
            .insert(key, SourceCheckpoint::new(7, Some(oversized.into())));
        assert!(read_position(&invalid, &id("source")).is_err());
        Ok(())
    }

    #[test]
    fn transaction_mode_does_not_adopt_rows_single_owner_or_managed_state() -> Result<()> {
        let binding = transaction_binding([1; 32]);
        assert_ne!(binding, [1; 32]);
        let position = SharedTransactionPosition::commit(binding, 100, 7)?;
        let state = state(position.clone());
        assert!(super::super::read_position(&state, &id("source")).is_err());
        assert!(super::super::rows::row_position(&state, &id("source")).is_err());
        for value in [
            serde_json::to_value(super::super::Position {
                version: 1,
                binding,
                commit_lsn: 100,
                managed_slot: None,
            })?,
            serde_json::to_value(super::super::rows::RowPosition::configured_snapshot(
                binding, 100,
            ))?,
        ] {
            assert!(serde_json::from_value::<SharedTransactionPosition>(value).is_err());
        }
        let mut managed = serde_json::to_value(&position)?;
        managed["managed_slot"] = "foreign".into();
        assert!(serde_json::from_value::<SharedTransactionPosition>(managed).is_err());
        assert!(SharedTransactionPosition::commit(binding, 100, 0).is_err());
        assert!(SharedTransactionPosition::configured_snapshot(binding, 0).is_err());
        let mut wrong = position.clone();
        wrong.version += 1;
        assert!(wrong.validate().is_err());
        wrong = position;
        wrong.binding = [2; 32];
        assert!(
            compare_positions(&wrong, &read_position(&state, &id("source"))?.unwrap()).is_err()
        );
        Ok(())
    }
}
