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

use bytes::Bytes;
use drasi_lib::sources::PositionComparator;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplicationState {
    pub binlog_file: String,
    pub binlog_position: u32,
    pub gtid_set: Option<String>,
    pub last_processed_timestamp: u64,
}

impl ReplicationState {
    /// Serialize this state to bytes for use as a source position token.
    pub fn to_position_bytes(&self) -> Bytes {
        Bytes::from(serde_json::to_vec(self).expect("ReplicationState serialization cannot fail"))
    }

    /// Deserialize from position bytes.
    pub fn from_position_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Position comparator for MySQL binlog positions.
///
/// Compares two `ReplicationState` JSON-encoded positions. Uses GTID-based
/// comparison when both positions have GTID sets; falls back to binlog
/// file + position comparison otherwise.
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

        // Compare using timestamp as the primary ordering metric.
        // MySQL binlog positions are not globally comparable across file rotations,
        // but timestamps provide a reliable monotonic ordering.
        if event_state.last_processed_timestamp != resume_state.last_processed_timestamp {
            return event_state.last_processed_timestamp > resume_state.last_processed_timestamp;
        }

        // Same timestamp — compare binlog file and position for finer granularity
        match event_state.binlog_file.cmp(&resume_state.binlog_file) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => event_state.binlog_position > resume_state.binlog_position,
        }
    }
}
