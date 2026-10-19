// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

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

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == 1
                && self.commit_lsn > 0
                && match self.offset {
                    None =>
                        self.count == 0
                            && self.sequence == 0
                            && self.previous_lsn == self.commit_lsn
                            && self.managed_slot.is_some(),
                    Some(offset) =>
                        self.count > 0
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
