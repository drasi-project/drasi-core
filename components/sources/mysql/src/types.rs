// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! MySQL source internal types

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_lib::sources::PositionComparator;
use std::cmp::Ordering;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplicationState {
    pub binlog_file: String,
    pub binlog_position: u32,
    pub gtid_set: Option<String>,
    pub last_processed_timestamp: u64,
    /// Native cursor before this transaction's GTID/BEGIN/TableMap event.
    /// Both row fields are absent on bootstrap and completed-transaction tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_start_position: Option<u32>,
    /// Zero-based ordinal of the emitted change within the committed transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_offset: Option<u64>,
}

impl ReplicationState {
    pub fn new(
        binlog_file: impl Into<String>,
        binlog_position: u32,
        gtid_set: Option<String>,
        last_processed_timestamp: u64,
    ) -> Self {
        Self {
            binlog_file: binlog_file.into(),
            binlog_position,
            gtid_set: gtid_set.filter(|gtid| !gtid.trim().is_empty()),
            last_processed_timestamp,
            transaction_start_position: None,
            row_offset: None,
        }
    }

    pub fn with_transaction_row(mut self, start_position: u32, row_offset: u64) -> Result<Self> {
        self.transaction_start_position = Some(start_position);
        self.row_offset = Some(row_offset);
        self.validate()?;
        Ok(self)
    }

    fn validate(&self) -> Result<()> {
        match (self.transaction_start_position, self.row_offset) {
            (None, None) => Ok(()),
            (Some(start), Some(_)) => {
                anyhow::ensure!(
                    !self.binlog_file.is_empty() && start >= 4 && start < self.binlog_position,
                    "Invalid MySQL row position: expected a binlog file and transaction start \
                     between byte 4 and commit position {}",
                    self.binlog_position
                );
                Ok(())
            }
            _ => anyhow::bail!(
                "Incomplete MySQL row position: transaction_start_position and row_offset \
                 must both be present"
            ),
        }
    }

    pub fn compare_cursor(&self, other: &Self) -> Ordering {
        self.binlog_file
            .cmp(&other.binlog_file)
            .then_with(|| self.binlog_position.cmp(&other.binlog_position))
            .then_with(|| match (self.row_offset, other.row_offset) {
                (Some(left), Some(right)) => left.cmp(&right),
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (None, None) => Ordering::Equal,
            })
    }

    /// Serialize this state to bytes for use as a source position token.
    pub fn to_position_bytes(&self) -> Bytes {
        encode_position(self).expect("ReplicationState serialization cannot fail")
    }

    /// Deserialize from position bytes.
    pub fn from_position_bytes(bytes: &[u8]) -> Option<Self> {
        decode_position(bytes).ok()
    }
}

/// Encode a MySQL source position token shared by bootstrap handover and CDC checkpoints.
pub fn encode_position(state: &ReplicationState) -> Result<Bytes> {
    serde_json::to_vec(state)
        .map(Bytes::from)
        .context("Failed to encode MySQL replication position")
}

/// Decode a MySQL source position token shared by bootstrap handover and CDC checkpoints.
pub fn decode_position(bytes: &[u8]) -> Result<ReplicationState> {
    let state: ReplicationState =
        serde_json::from_slice(bytes).context("Failed to decode MySQL replication position")?;
    state.validate()?;
    Ok(state)
}

/// Position comparator for MySQL binlog positions.
///
/// Orders positions by binlog file, commit position, and transaction row ordinal.
/// A token without row fields marks a completed cursor, after every row at that
/// position. Timestamps and GTIDs are metadata, not per-row ordering keys.
#[derive(Debug, Clone, Default)]
pub struct MySqlPositionComparator;

impl PositionComparator for MySqlPositionComparator {
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool {
        let Some(event_state) = ReplicationState::from_position_bytes(event_pos) else {
            return false;
        };
        let Some(resume_state) = ReplicationState::from_position_bytes(resume_pos) else {
            // Cannot parse resume position — deliver the event to be safe
            return true;
        };

        event_state.compare_cursor(&resume_state).is_gt()
    }
}
