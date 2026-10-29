// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::ConsumerProgressSnapshot;
use std::cmp::Ordering;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowPosition {
    pub version: u32,
    pub binding: [u8; 32],
    pub commit_lsn: u64,
    pub previous_lsn: u64,
    pub offset: Option<usize>,
    pub count: usize,
    pub sequence: u64,
    pub managed_slot: Option<String>,
}

/// Compare coverage of the same PostgreSQL stream, not consumer-local logical
/// counters. A snapshot boundary is after its commit; the last row of that
/// commit has equal coverage. A snapshot's zero count does not describe the
/// transaction's row count.
pub fn compare_row_positions(left: &RowPosition, right: &RowPosition) -> Result<Ordering> {
    left.validate()?;
    right.validate()?;
    anyhow::ensure!(
        left.binding == right.binding,
        "cannot compare PostgreSQL row positions from different source bindings"
    );
    let commit = left.commit_lsn.cmp(&right.commit_lsn);
    if commit != Ordering::Equal {
        return Ok(commit);
    }
    if left.offset.is_some() && right.offset.is_some() {
        anyhow::ensure!(
            left.count == right.count,
            "PostgreSQL row positions disagree about the same transaction's change count"
        );
    }
    match (left.complete(), right.complete()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(Ordering::Greater),
        (false, true) => Ok(Ordering::Less),
        (false, false) => Ok(left.offset.cmp(&right.offset)),
    }
}

/// An actual member's minimum cursor, unchanged, plus the conservative feedback
/// floor across all members. The feedback floor can be lower than the selected
/// cursor's own floor when another member has a partial later transaction.
/// Its logical sequence is still that member's counter, not a shared producer
/// sequence-allocation agreement.
#[derive(Debug)]
pub struct RowProgressMinimum {
    pub consumer: ComponentId,
    pub position: RowPosition,
    pub feedback_lsn: u64,
}

/// `expected_consumers` must come from the already validated, fixed host binding.
/// This helper establishes neither ownership nor durability, and does not
/// compare reset generations belonging to different consumers.
///
/// `None` fences feedback: membership is empty/changed, a member is not ready
/// for live processing, or a member has no cursor for this source. Never turn
/// it into acknowledgement of a received/read LSN. Failed owners and malformed,
/// rebound or conflicting positions are errors, even if another member is
/// pending. No aggregate SourceProgressSnapshot or replacement cursor is made.
pub fn minimum_row_checkpoint(
    snapshot: &ConsumerProgressSnapshot,
    expected_consumers: &BTreeSet<ComponentId>,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<RowProgressMinimum>> {
    let mut fenced =
        expected_consumers.is_empty() || !snapshot.consumers.keys().eq(expected_consumers.iter());
    let mut minimum: Option<RowProgressMinimum> = None;
    let mut feedback = None;
    let mut counts = BTreeMap::new();
    for (consumer, state) in &snapshot.consumers {
        if let Some(failure) = &state.failure {
            anyhow::bail!(
                "PostgreSQL consumer {consumer} cannot provide committed row progress: {failure}"
            );
        }
        fenced |= !state.ready || !state.recovered || !state.admitting;
        let Some(position) = row_checkpoint(state, source, binding)
            .with_context(|| format!("invalid PostgreSQL row progress for consumer {consumer}"))?
        else {
            fenced = true;
            continue;
        };
        if position.offset.is_some() {
            if let Some(previous) = counts.insert(position.commit_lsn, position.count) {
                anyhow::ensure!(
                    previous == position.count,
                    "PostgreSQL consumers disagree about transaction {} change count",
                    position.commit_lsn
                );
            }
        }
        let floor = position.feedback_lsn();
        feedback = Some(feedback.map_or(floor, |current: u64| current.min(floor)));
        let earlier = match &minimum {
            None => true,
            Some(previous) => {
                compare_row_positions(&position, &previous.position)? == Ordering::Less
            }
        };
        if earlier {
            minimum = Some(RowProgressMinimum {
                consumer: consumer.clone(),
                position,
                feedback_lsn: floor,
            });
        }
    }
    if fenced {
        return Ok(None);
    }
    if let Some(minimum) = &mut minimum {
        minimum.feedback_lsn = feedback.context("PostgreSQL minimum has no feedback floor")?;
    }
    Ok(minimum)
}

impl RowPosition {
    pub fn snapshot(binding: [u8; 32], slot: String, boundary: u64) -> Self {
        Self {
            version: 1,
            binding,
            commit_lsn: boundary,
            previous_lsn: boundary,
            offset: None,
            count: 0,
            sequence: 0,
            managed_slot: Some(slot),
        }
    }

    /// An after-commit snapshot of an externally configured live slot. Version
    /// two grants no managed-slot ownership and is only valid for this boundary.
    pub fn configured_snapshot(binding: [u8; 32], boundary: u64) -> Self {
        Self {
            version: 2,
            binding,
            commit_lsn: boundary,
            previous_lsn: boundary,
            offset: None,
            count: 0,
            sequence: 0,
            managed_slot: None,
        }
    }

    pub fn is_configured_snapshot(&self) -> bool {
        self.version == 2 && self.offset.is_none() && self.managed_slot.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.commit_lsn > 0
                && match self.offset {
                    None =>
                        self.count == 0
                            && self.sequence == 0
                            && self.previous_lsn == self.commit_lsn
                            && ((self.version == 1 && self.managed_slot.is_some())
                                || self.is_configured_snapshot()),
                    Some(offset) =>
                        self.version == 1
                            && self.count > 0
                            && offset < self.count
                            && self.sequence > 0
                            && self.previous_lsn < self.commit_lsn,
                },
            "invalid PostgreSQL row position"
        );
        Ok(())
    }

    pub fn complete(&self) -> bool {
        self.offset.is_none_or(|offset| offset + 1 == self.count)
    }

    pub fn feedback_lsn(&self) -> u64 {
        if self.complete() {
            self.commit_lsn
        } else {
            self.previous_lsn
        }
    }
}

pub fn row_position(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
) -> Result<Option<RowPosition>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.to_string()))
    else {
        return Ok(None);
    };
    let position: RowPosition = serde_json::from_slice(
        checkpoint
            .source_position
            .as_deref()
            .context("PostgreSQL row checkpoint has no position")?,
    )?;
    position.validate()?;
    anyhow::ensure!(
        position.sequence == checkpoint.sequence,
        "PostgreSQL row checkpoint sequence mismatch"
    );
    Ok(Some(position))
}

pub fn row_checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<RowPosition>> {
    let position = row_position(snapshot, source)?;
    if let Some(position) = &position {
        anyhow::ensure!(
            position.binding == binding,
            "PostgreSQL row checkpoint source binding mismatch"
        );
    }
    Ok(position)
}

pub fn row_live_config(
    config: &PostgresTransactionConfig,
    progress: &SourceProgressSnapshot,
    source: &ComponentId,
) -> Result<PostgresTransactionConfig> {
    config.validate_coordinated()?;
    anyhow::ensure!(
        progress.bootstrap_complete,
        "PostgreSQL row snapshot is incomplete"
    );
    let position = row_position(progress, source)?
        .context("PostgreSQL row snapshot has no committed position")?;
    let slot = position
        .managed_slot
        .context("PostgreSQL row snapshot has no slot ownership")?;
    validate_managed_slot(&config.connection.slot_name, &slot)?;
    let mut config = config.clone();
    config.connection.slot_name = slot;
    config.start_lsn = format_lsn(position.commit_lsn);
    Ok(config)
}

#[cfg(test)]
mod tests;
