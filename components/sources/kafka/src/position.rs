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
//! Position format: `[partition_count: u32 BE][next_offset_p0: i64 BE]...[next_offset_pN: i64 BE]`
//!
//! Total size: `4 + (8 × num_partitions)` bytes.
//!
//! Each offset represents the **next offset to consume** (not the consumed offset).
//! This convention avoids off-by-one errors at bootstrap→streaming handoff,
//! since Kafka high watermarks already mean "next offset to produce."

use bytes::Bytes;
use drasi_lib::sources::PositionComparator;

/// Encode a vector of per-partition next-offsets into a position blob.
pub fn encode_position(offsets: &[i64]) -> Bytes {
    let partition_count = offsets.len() as u32;
    let mut buf = Vec::with_capacity(4 + 8 * offsets.len());
    buf.extend_from_slice(&partition_count.to_be_bytes());
    for &offset in offsets {
        buf.extend_from_slice(&offset.to_be_bytes());
    }
    Bytes::from(buf)
}

/// Decode a position blob into per-partition next-offsets.
///
/// Returns `None` if the bytes are malformed (wrong length).
pub fn decode_position(bytes: &Bytes) -> Option<Vec<i64>> {
    if bytes.len() < 4 {
        return None;
    }

    let partition_count = u32::from_be_bytes(bytes[0..4].try_into().ok()?) as usize;
    let expected_len = 4 + 8 * partition_count;

    if bytes.len() != expected_len {
        return None;
    }

    let mut offsets = Vec::with_capacity(partition_count);
    for i in 0..partition_count {
        let start = 4 + 8 * i;
        let end = start + 8;
        let arr: [u8; 8] = bytes[start..end].try_into().ok()?;
        offsets.push(i64::from_be_bytes(arr));
    }

    Some(offsets)
}

/// Custom position comparator for multi-partition Kafka sources.
///
/// Unlike `ByteLexPositionComparator`, this performs component-wise vector comparison.
/// An event position is "reached" (i.e., is new/after resume) if:
/// - ALL partition offsets in event_pos >= corresponding resume_pos offsets, AND
/// - At least one partition offset is strictly greater.
///
/// This is correct because partitions advance independently. A simple byte comparison
/// would incorrectly order multi-partition positions.
#[derive(Debug, Clone, Default)]
pub struct KafkaPositionComparator;

impl PositionComparator for KafkaPositionComparator {
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool {
        let Some(event_offsets) = decode_position(event_pos) else {
            return false;
        };
        let Some(resume_offsets) = decode_position(resume_pos) else {
            // If resume position is malformed, treat all events as new
            return true;
        };

        // Different partition counts → incompatible, treat as new
        if event_offsets.len() != resume_offsets.len() {
            return true;
        }

        let mut has_advance = false;
        for (e, r) in event_offsets.iter().zip(resume_offsets.iter()) {
            if e < r {
                return false; // Regression in any partition → already delivered
            }
            if e > r {
                has_advance = true;
            }
        }

        has_advance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let offsets = vec![10, 20, 30];
        let encoded = encode_position(&offsets);
        let decoded = decode_position(&encoded).unwrap();
        assert_eq!(decoded, offsets);
    }

    #[test]
    fn test_encode_decode_single_partition() {
        let offsets = vec![42];
        let encoded = encode_position(&offsets);
        assert_eq!(encoded.len(), 4 + 8); // 4 byte count + 8 byte offset
        let decoded = decode_position(&encoded).unwrap();
        assert_eq!(decoded, vec![42]);
    }

    #[test]
    fn test_encode_decode_empty() {
        let offsets: Vec<i64> = vec![];
        let encoded = encode_position(&offsets);
        assert_eq!(encoded.len(), 4); // Just the count
        let decoded = decode_position(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_decode_malformed_too_short() {
        let bytes = Bytes::from(vec![0u8, 0, 0]); // Less than 4 bytes
        assert!(decode_position(&bytes).is_none());
    }

    #[test]
    fn test_decode_malformed_wrong_length() {
        // Says 2 partitions but only has bytes for 1
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&42i64.to_be_bytes());
        let bytes = Bytes::from(buf);
        assert!(decode_position(&bytes).is_none());
    }

    #[test]
    fn test_comparator_event_after_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[11, 20, 30]);
        let resume = encode_position(&[10, 20, 30]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_event_before_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[9, 20, 30]);
        let resume = encode_position(&[10, 20, 30]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_event_at_exact_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[10, 20, 30]);
        let resume = encode_position(&[10, 20, 30]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_mixed_partition_states() {
        // One partition advanced, one behind → already delivered (regression)
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[11, 19, 30]);
        let resume = encode_position(&[10, 20, 30]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_all_advanced() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[11, 21, 31]);
        let resume = encode_position(&[10, 20, 30]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_malformed_event() {
        let comparator = KafkaPositionComparator;
        let event = Bytes::from(vec![0u8, 0]);
        let resume = encode_position(&[10, 20]);
        assert!(!comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_malformed_resume() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[10, 20]);
        let resume = Bytes::from(vec![0u8, 0]);
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_comparator_different_partition_counts() {
        let comparator = KafkaPositionComparator;
        let event = encode_position(&[10, 20, 30]);
        let resume = encode_position(&[10, 20]);
        // Different partition counts → treat as new
        assert!(comparator.position_reached(&event, &resume));
    }

    #[test]
    fn test_large_offsets() {
        let offsets = vec![i64::MAX, i64::MAX - 1, 0, -1];
        let encoded = encode_position(&offsets);
        let decoded = decode_position(&encoded).unwrap();
        assert_eq!(decoded, offsets);
    }
}
