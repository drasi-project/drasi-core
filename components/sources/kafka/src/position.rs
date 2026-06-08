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

//! Multi-partition position encoding and comparison.
//!
//! Position format:
//! `[from_partition: u32 BE][partition_count: u32 BE][next_offset_p0: i64 BE]...[next_offset_pN: i64 BE]`
//!
//! - `from_partition`: which partition produced this event (used by comparator)
//! - `partition_count`: number of partitions tracked
//! - `next_offset_pN`: next offset to consume for partition N
//!
//! Total size: `8 + (8 × num_partitions)` bytes.
//!
//! Each offset represents the **next offset to consume** (not the consumed offset).
//! This convention avoids off-by-one errors at bootstrap→streaming handoff,
//! since Kafka high watermarks already mean "next offset to produce."

use bytes::Bytes;
use drasi_lib::sources::PositionComparator;

const BOOTSTRAP_BOUNDARY_MAGIC: &[u8; 4] = b"KBND";

/// Encode a position blob with the partition that produced the event.
///
/// - `from_partition`: the partition index this message was consumed from
/// - `offsets`: per-partition next-offsets (full offset vector snapshot)
pub fn encode_position(from_partition: usize, offsets: &[i64]) -> Bytes {
    let partition_count = offsets.len() as u32;
    let mut buf = Vec::with_capacity(8 + 8 * offsets.len());
    buf.extend_from_slice(&(from_partition as u32).to_be_bytes());
    buf.extend_from_slice(&partition_count.to_be_bytes());
    for &offset in offsets {
        buf.extend_from_slice(&offset.to_be_bytes());
    }
    Bytes::from(buf)
}

/// Decode a position blob into (from_partition, per-partition next-offsets).
///
/// Returns `None` if the bytes are malformed (wrong length or too short).
pub fn decode_position(bytes: &Bytes) -> Option<(usize, Vec<i64>)> {
    if bytes.len() < 8 || &bytes[0..4] == BOOTSTRAP_BOUNDARY_MAGIC {
        return None;
    }

    let from_partition = u32::from_be_bytes(bytes[0..4].try_into().ok()?) as usize;
    let partition_count = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
    let expected_len = 8 + 8 * partition_count;

    if bytes.len() != expected_len {
        return None;
    }

    let mut offsets = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let start = 8 + 8 * i;
        let end = start + 8;
        let arr: [u8; 8] = bytes[start..end].try_into().ok()?;
        offsets.push(i64::from_be_bytes(arr));
    }

    Some((from_partition, offsets))
}

/// Encode a Kafka bootstrap boundary as partition/high-watermark pairs.
pub fn encode_partition_offsets(partition_offsets: &[(i32, i64)]) -> Bytes {
    let mut buf = Vec::with_capacity(8 + 12 * partition_offsets.len());
    buf.extend_from_slice(BOOTSTRAP_BOUNDARY_MAGIC);
    buf.extend_from_slice(&(partition_offsets.len() as u32).to_be_bytes());
    for &(partition, offset) in partition_offsets {
        buf.extend_from_slice(&partition.to_be_bytes());
        buf.extend_from_slice(&offset.to_be_bytes());
    }
    Bytes::from(buf)
}

/// Decode a Kafka bootstrap boundary into partition/high-watermark pairs.
pub fn decode_partition_offsets(bytes: &Bytes) -> Option<Vec<(i32, i64)>> {
    if bytes.len() < 8 || &bytes[0..4] != BOOTSTRAP_BOUNDARY_MAGIC {
        return None;
    }
    let partition_count = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
    if bytes.len() != 8 + 12 * partition_count {
        return None;
    }
    let mut result = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let start = 8 + 12 * i;
        result.push((
            i32::from_be_bytes(bytes[start..start + 4].try_into().ok()?),
            i64::from_be_bytes(bytes[start + 4..start + 12].try_into().ok()?),
        ));
    }
    Some(result)
}

/// Partition-aware position comparator for multi-partition Kafka sources.
///
/// Each event records which partition it came from (`from_partition`). The comparator
/// checks only that partition's offset against the subscriber's resume position:
///
/// ```text
/// event_offsets[from_partition] > resume_offsets[from_partition]
/// ```
///
/// This correctly handles multi-subscriber scenarios where partitions advance
/// independently. Unlike component-wise "all must advance" comparison, this
/// never suppresses valid new events from one partition just because another
/// partition hasn't caught up.
#[derive(Debug, Clone, Default)]
pub struct KafkaPositionComparator;

impl PositionComparator for KafkaPositionComparator {
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool {
        let Some((from_partition, event_offsets)) = decode_position(event_pos) else {
            return false; // Can't parse event → don't deliver
        };
        let Some((_, resume_offsets)) = decode_position(resume_pos) else {
            // If resume position is malformed, treat all events as new
            return true;
        };

        // Event's partition doesn't exist in resume (topic was expanded) → new
        if from_partition >= resume_offsets.len() {
            return true;
        }
        if from_partition >= event_offsets.len() {
            return false; // Shouldn't happen, fail safe
        }

        // Core: has the subscriber already consumed past this partition's offset?
        event_offsets[from_partition] > resume_offsets[from_partition]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let offsets = vec![10, 20, 30];
        let encoded = encode_position(1, &offsets);
        let (from_p, decoded) = decode_position(&encoded).unwrap();
        assert_eq!(from_p, 1);
        assert_eq!(decoded, offsets);
    }

    #[test]
    fn test_encode_decode_single_partition() {
        let offsets = vec![42];
        let encoded = encode_position(0, &offsets);
        assert_eq!(encoded.len(), 8 + 8); // 4 from_partition + 4 count + 8 offset
        let (from_p, decoded) = decode_position(&encoded).unwrap();
        assert_eq!(from_p, 0);
        assert_eq!(decoded, vec![42]);
    }

    #[test]
    fn test_encode_decode_empty() {
        let offsets: Vec<i64> = vec![];
        let encoded = encode_position(0, &offsets);
        assert_eq!(encoded.len(), 8); // Just from_partition + count
        let (from_p, decoded) = decode_position(&encoded).unwrap();
        assert_eq!(from_p, 0);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_decode_malformed_too_short() {
        let bytes = Bytes::from(vec![0u8, 0, 0]); // Less than 8 bytes
        assert!(decode_position(&bytes).is_none());
    }

    #[test]
    fn test_decode_malformed_wrong_length() {
        // Says 2 partitions but only has bytes for 1
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_be_bytes()); // from_partition
        buf.extend_from_slice(&2u32.to_be_bytes()); // partition_count = 2
        buf.extend_from_slice(&42i64.to_be_bytes()); // only 1 offset
        let bytes = Bytes::from(buf);
        assert!(decode_position(&bytes).is_none());
    }

    #[test]
    fn test_comparator_event_from_advanced_partition() {
        let comparator = KafkaPositionComparator;
        // Event from P0, P0 offset advanced
        let event = encode_position(0, &[11, 20, 30]);
        let resume = encode_position(0, &[10, 20, 30]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_event_from_behind_partition() {
        let comparator = KafkaPositionComparator;
        // Event from P0, but P0 offset not past resume
        let event = encode_position(0, &[9, 20, 30]);
        let resume = encode_position(0, &[10, 20, 30]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_event_at_exact_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(0, &[10, 20, 30]);
        let resume = encode_position(0, &[10, 20, 30]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_multi_partition_independent() {
        let comparator = KafkaPositionComparator;
        // Event from P0 is advanced, even though P1 is behind resume
        // (this is the key case the old comparator got wrong)
        let event = encode_position(0, &[11, 19, 30]);
        let resume = encode_position(0, &[10, 20, 30]);
        // Only checks P0: 11 > 10? YES → deliver
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_checks_only_from_partition() {
        let comparator = KafkaPositionComparator;
        // Event from P1, P1 not advanced but P0 is
        let event = encode_position(1, &[11, 20, 30]);
        let resume = encode_position(0, &[10, 20, 30]);
        // Only checks P1: 20 > 20? NO → suppress
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_multi_subscriber_scenario() {
        let comparator = KafkaPositionComparator;
        // A: resume [10, 5], B: resume [8, 7]
        let resume_a = encode_position(0, &[10, 5]);
        let resume_b = encode_position(0, &[8, 7]);

        // Event from P0 offset 8 (position [9, 5])
        let p0_8 = encode_position(0, &[9, 5]);
        assert!(!comparator.position_reached(&p0_8, &resume_a)); // A: 9>10? NO
        assert!(comparator.position_reached(&p0_8, &resume_b)); // B: 9>8? YES

        // Event from P1 offset 5 (position [9, 6])
        let p1_5 = encode_position(1, &[9, 6]);
        assert!(comparator.position_reached(&p1_5, &resume_a)); // A: 6>5? YES
        assert!(!comparator.position_reached(&p1_5, &resume_b)); // B: 6>7? NO
    }

    #[test]
    fn test_comparator_malformed_event() {
        let comparator = KafkaPositionComparator;
        let event = Bytes::from(vec![0u8, 0]);
        let resume = encode_position(0, &[10, 20]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_malformed_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(0, &[10, 20]);
        let resume = Bytes::from(vec![0u8, 0]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_partition_count_mismatch() {
        let comparator = KafkaPositionComparator;
        // Event has 3 partitions, resume has 2 (topic expanded)
        // from_partition=2 doesn't exist in resume → treat as new
        let event = encode_position(2, &[10, 20, 30]);
        let resume = encode_position(0, &[10, 20]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_resume_from_partition_ignored() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(0, &[11, 5]);
        // Different from_partition in resume should not matter
        let resume_a = encode_position(0, &[10, 5]);
        let resume_b = encode_position(1, &[10, 5]);
        assert!(comparator.position_reached(&event, &resume_a));
        assert!(comparator.position_reached(&event, &resume_b));
    }

    #[test]
    fn test_large_offsets() {
        let offsets = vec![i64::MAX, i64::MAX - 1, 0, -1];
        let encoded = encode_position(2, &offsets);
        let (from_p, decoded) = decode_position(&encoded).unwrap();
        assert_eq!(from_p, 2);
        assert_eq!(decoded, offsets);
    }
}
