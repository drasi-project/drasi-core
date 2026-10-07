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

const MAX_POSITION_BYTES: usize = 64 * 1024;
const MAX_BINLOG_FILE_BYTES: usize = 255;
// Leave space for the filename, numeric fields and JSON framing.
const MAX_GTID_SET_BYTES: usize = 60 * 1024;

/// Opaque checkpoint within one binlog lineage. Row tokens retain the real
/// transaction-end position and a native GTID/BEGIN/TableMap start cursor;
/// their ordinal spans all emitted changes, including multiple statements.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplicationState {
    pub binlog_file: String,
    pub binlog_position: u32,
    pub gtid_set: Option<String>,
    pub last_processed_timestamp: u64,
    /// Native cursor before this transaction's GTID/BEGIN/TableMap event.
    /// Set together with row_offset by flush_transaction for per-row checkpoints.
    /// Both are absent on bootstrap boundaries and unbuffered push_change tokens,
    /// which represent a cursor after all rows at binlog_position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_start_position: Option<u32>,
    /// Zero-based ordinal of the emitted change within the committed transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    row_offset: Option<u64>,
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

    /// Attach a zero-based emitted-row ordinal and native transaction start.
    /// Requires a nonempty binlog filename, 4 <= start_position < binlog_position,
    /// and filename/GTID lengths within the checkpoint limits.
    pub fn with_transaction_row(mut self, start_position: u32, row_offset: u64) -> Result<Self> {
        self.transaction_start_position = Some(start_position);
        self.row_offset = Some(row_offset);
        self.validate()?;
        Ok(self)
    }

    pub fn transaction_start_position(&self) -> Option<u32> {
        self.transaction_start_position
    }

    pub fn row_offset(&self) -> Option<u64> {
        self.row_offset
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.binlog_file.len() <= MAX_BINLOG_FILE_BYTES,
            "MySQL binlog filename exceeds {MAX_BINLOG_FILE_BYTES} bytes"
        );
        anyhow::ensure!(
            self.gtid_set
                .as_ref()
                .is_none_or(|gtid| gtid.len() <= MAX_GTID_SET_BYTES),
            "MySQL GTID set exceeds {MAX_GTID_SET_BYTES} bytes"
        );
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

    /// Order by file, transaction-end position, then emitted-row ordinal.
    /// A boundary token without an ordinal sorts after every row at that cursor.
    /// Only positions from the same binlog lineage are comparable.
    pub fn compare_cursor(&self, other: &Self) -> Ordering {
        self.binlog_file
            .cmp(&other.binlog_file)
            .then_with(|| self.binlog_position.cmp(&other.binlog_position))
            .then_with(|| match (self.row_offset(), other.row_offset()) {
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
    anyhow::ensure!(
        bytes.len() < MAX_POSITION_BYTES,
        "MySQL replication position must be smaller than {MAX_POSITION_BYTES} bytes"
    );
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
/// This PositionComparator implementation uses strict ordering (equal tokens
/// are suppressed) and assumes one binlog lineage with consistently padded
/// numeric filename suffixes; it cannot compare positions across failover.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_string_limits_apply_to_boundary_and_row_tokens() {
        let boundary = ReplicationState::new(
            "f".repeat(MAX_BINLOG_FILE_BYTES),
            200,
            Some("g".repeat(MAX_GTID_SET_BYTES)),
            0,
        );
        for state in [
            boundary.clone(),
            boundary.with_transaction_row(4, u64::MAX).unwrap(),
        ] {
            let decoded = decode_position(&state.to_position_bytes()).unwrap();
            assert_eq!(
                decoded.transaction_start_position(),
                state.transaction_start_position()
            );
            assert_eq!(decoded.row_offset(), state.row_offset());

            let mut too_long = state.clone();
            too_long.binlog_file.push('f');
            assert!(decode_position(&too_long.to_position_bytes())
                .unwrap_err()
                .to_string()
                .contains("binlog filename exceeds"));

            let mut too_long = state;
            too_long.gtid_set.as_mut().unwrap().push('g');
            assert!(decode_position(&too_long.to_position_bytes())
                .unwrap_err()
                .to_string()
                .contains("GTID set exceeds"));
        }
    }

    #[test]
    fn encoded_token_limit_is_checked_before_deserialization() {
        let mut token = ReplicationState::new("mysql-bin.000001", 200, None, 0)
            .to_position_bytes()
            .to_vec();
        token.resize(MAX_POSITION_BYTES - 1, b' ');
        assert!(decode_position(&token).is_ok());
        token.push(b' ');
        assert!(decode_position(&token)
            .unwrap_err()
            .to_string()
            .contains("must be smaller than"));
        assert!(decode_position(&vec![b'!'; MAX_POSITION_BYTES])
            .unwrap_err()
            .to_string()
            .contains("must be smaller than"));
    }

    #[test]
    fn row_constructor_validates_cursor_and_string_limits() {
        for start in [0, 3, 200, 201] {
            assert!(ReplicationState::new("mysql-bin.000001", 200, None, 0)
                .with_transaction_row(start, 0)
                .is_err());
        }
        assert!(ReplicationState::new("", 200, None, 0)
            .with_transaction_row(4, 0)
            .is_err());
        assert!(
            ReplicationState::new("f".repeat(MAX_BINLOG_FILE_BYTES + 1), 200, None, 0)
                .with_transaction_row(4, 0)
                .is_err()
        );
        assert!(ReplicationState::new(
            "mysql-bin.000001",
            200,
            Some("g".repeat(MAX_GTID_SET_BYTES + 1)),
            0,
        )
        .with_transaction_row(4, 0)
        .is_err());
    }
}
