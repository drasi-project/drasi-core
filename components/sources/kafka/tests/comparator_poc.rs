// POC: Partition-aware position comparator
//
// Verifies that adding `from_partition` to the position encoding enables correct
// per-subscriber replay filtering across all edge cases.
//
// The SourceBase integration (dispatchers, per-subscriber filtering) is already
// proven by existing tests in lib/src/sources/base.rs. This POC focuses on:
// 1. Correctness of the comparator logic itself
// 2. Encode/decode with the new format
// 3. All edge cases that matter for multi-partition Kafka consumption
//
// Run: cargo test -p drasi-source-kafka --test comparator_poc

use bytes::Bytes;
use drasi_lib::sources::PositionComparator;

// ============================================================================
// New position encoding with from_partition
// ============================================================================

/// Encode: [from_partition: u32 BE][partition_count: u32 BE][offset_0: i64 BE]...
fn encode_position(from_partition: usize, offsets: &[i64]) -> Bytes {
    let partition_count = offsets.len() as u32;
    let mut buf = Vec::with_capacity(8 + 8 * offsets.len());
    buf.extend_from_slice(&(from_partition as u32).to_be_bytes());
    buf.extend_from_slice(&partition_count.to_be_bytes());
    for &offset in offsets {
        buf.extend_from_slice(&offset.to_be_bytes());
    }
    Bytes::from(buf)
}

/// Decode → (from_partition, offsets)
fn decode_position(bytes: &Bytes) -> Option<(usize, Vec<i64>)> {
    if bytes.len() < 8 {
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

// ============================================================================
// Partition-aware comparator
// ============================================================================

#[derive(Debug, Clone)]
struct PartitionAwareComparator;

impl PositionComparator for PartitionAwareComparator {
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool {
        let Some((from_partition, event_offsets)) = decode_position(event_pos) else {
            return false; // Can't parse event → don't deliver
        };
        let Some((_, resume_offsets)) = decode_position(resume_pos) else {
            return true; // Can't parse resume → treat all events as new
        };

        // Event's partition doesn't exist in resume (topic expanded) → new
        if from_partition >= resume_offsets.len() {
            return true;
        }
        if from_partition >= event_offsets.len() {
            return false; // Shouldn't happen
        }

        // Core: is the subscriber already past this partition's offset?
        event_offsets[from_partition] > resume_offsets[from_partition]
    }
}

// ============================================================================
// Tests: Multi-subscriber replay simulation
// ============================================================================

/// Simulates the exact scenario from the plan:
/// 2 partitions, 2 subscribers with different checkpoints.
/// Verifies each event is correctly classified per subscriber.
#[test]
fn poc_multi_subscriber_two_partitions() {
    let cmp = PartitionAwareComparator;

    // Query A: resume [P0=10, P1=5] (consumed P0 up to 9, P1 up to 4)
    // Query B: resume [P0=8, P1=7] (consumed P0 up to 7, P1 up to 6)
    let resume_a = encode_position(0, &[10, 5]);
    let resume_b = encode_position(0, &[8, 7]);

    // Consumer restarts from min per partition: [8, 5]
    // Messages consumed in order:

    // P0 offset 8 → position (from=0, [9, 5])
    let p0_8 = encode_position(0, &[9, 5]);
    assert!(!cmp.position_reached(&p0_8, &resume_a), "A already has P0:8 (9>10? NO)");
    assert!(cmp.position_reached(&p0_8, &resume_b), "B needs P0:8 (9>8? YES)");

    // P1 offset 5 → position (from=1, [9, 6])
    let p1_5 = encode_position(1, &[9, 6]);
    assert!(cmp.position_reached(&p1_5, &resume_a), "A needs P1:5 (6>5? YES)");
    assert!(!cmp.position_reached(&p1_5, &resume_b), "B already has P1:5 (6>7? NO)");

    // P0 offset 9 → position (from=0, [10, 6])
    let p0_9 = encode_position(0, &[10, 6]);
    assert!(!cmp.position_reached(&p0_9, &resume_a), "A already has P0:9 (10>10? NO)");
    assert!(cmp.position_reached(&p0_9, &resume_b), "B needs P0:9 (10>8? YES)");

    // P1 offset 6 → position (from=1, [10, 7])
    let p1_6 = encode_position(1, &[10, 7]);
    assert!(cmp.position_reached(&p1_6, &resume_a), "A needs P1:6 (7>5? YES)");
    assert!(!cmp.position_reached(&p1_6, &resume_b), "B already has P1:6 (7>7? NO)");

    // P0 offset 10 → position (from=0, [11, 7])
    let p0_10 = encode_position(0, &[11, 7]);
    assert!(cmp.position_reached(&p0_10, &resume_a), "A needs P0:10 (11>10? YES)");
    assert!(cmp.position_reached(&p0_10, &resume_b), "B needs P0:10 (11>8? YES)");

    // P1 offset 7 → position (from=1, [11, 8])
    let p1_7 = encode_position(1, &[11, 8]);
    assert!(cmp.position_reached(&p1_7, &resume_a), "A needs P1:7 (8>5? YES)");
    assert!(cmp.position_reached(&p1_7, &resume_b), "B needs P1:7 (8>7? YES)");
}

/// Tally: A gets [P1:5, P1:6, P0:10, P1:7] = 4 events
///        B gets [P0:8, P0:9, P0:10, P1:7] = 4 events
#[test]
fn poc_multi_subscriber_delivery_count() {
    let cmp = PartitionAwareComparator;
    let resume_a = encode_position(0, &[10, 5]);
    let resume_b = encode_position(0, &[8, 7]);

    let events = vec![
        encode_position(0, &[9, 5]),  // P0:8
        encode_position(1, &[9, 6]),  // P1:5
        encode_position(0, &[10, 6]), // P0:9
        encode_position(1, &[10, 7]), // P1:6
        encode_position(0, &[11, 7]), // P0:10
        encode_position(1, &[11, 8]), // P1:7
    ];

    let a_count: usize = events
        .iter()
        .filter(|e| cmp.position_reached(e, &resume_a))
        .count();
    let b_count: usize = events
        .iter()
        .filter(|e| cmp.position_reached(e, &resume_b))
        .count();

    assert_eq!(a_count, 4, "A should receive 4 events");
    assert_eq!(b_count, 4, "B should receive 4 events");
}

// ============================================================================
// Tests: Single partition (degenerate case)
// ============================================================================

#[test]
fn poc_single_partition_simple() {
    let cmp = PartitionAwareComparator;
    let resume = encode_position(0, &[5]); // consumed up to offset 4

    // Offsets 3,4 (next=4,5): should NOT pass
    assert!(!cmp.position_reached(&encode_position(0, &[4]), &resume), "offset 3: 4>5? NO");
    assert!(!cmp.position_reached(&encode_position(0, &[5]), &resume), "offset 4: 5>5? NO");

    // Offsets 5,6,7 (next=6,7,8): should pass
    assert!(cmp.position_reached(&encode_position(0, &[6]), &resume), "offset 5: 6>5? YES");
    assert!(cmp.position_reached(&encode_position(0, &[7]), &resume), "offset 6: 7>5? YES");
    assert!(cmp.position_reached(&encode_position(0, &[8]), &resume), "offset 7: 8>5? YES");
}

// ============================================================================
// Tests: Three partitions
// ============================================================================

#[test]
fn poc_three_partitions_mixed() {
    let cmp = PartitionAwareComparator;
    // Subscriber checkpoint: P0=10, P1=5, P2=20
    let resume = encode_position(0, &[10, 5, 20]);

    // P0:10 (next=11): 11>10? YES
    assert!(cmp.position_reached(&encode_position(0, &[11, 5, 20]), &resume));
    // P1:5 (next=6): 6>5? YES
    assert!(cmp.position_reached(&encode_position(1, &[11, 6, 20]), &resume));
    // P2:20 (next=21): 21>20? YES
    assert!(cmp.position_reached(&encode_position(2, &[11, 6, 21]), &resume));

    // P0:9 (next=10): 10>10? NO (already consumed)
    assert!(!cmp.position_reached(&encode_position(0, &[10, 5, 20]), &resume));
    // P1:4 (next=5): 5>5? NO
    assert!(!cmp.position_reached(&encode_position(1, &[10, 5, 20]), &resume));
    // P2:19 (next=20): 20>20? NO
    assert!(!cmp.position_reached(&encode_position(2, &[10, 5, 20]), &resume));
}

// ============================================================================
// Tests: Edge cases
// ============================================================================

#[test]
fn poc_exact_resume_position_suppressed() {
    let cmp = PartitionAwareComparator;
    let resume = encode_position(0, &[10, 5]);

    // Event at exact resume position → NOT new (already consumed)
    let event = encode_position(0, &[10, 5]);
    assert!(!cmp.position_reached(&event, &resume));

    let event2 = encode_position(1, &[10, 5]);
    assert!(!cmp.position_reached(&event2, &resume));
}

#[test]
fn poc_partition_count_mismatch_topic_expanded() {
    let cmp = PartitionAwareComparator;

    // Resume has 2 partitions, event has 3 (topic expanded)
    let resume = encode_position(0, &[10, 10]);

    // from_partition=2 doesn't exist in resume → treat as new
    assert!(cmp.position_reached(&encode_position(2, &[10, 10, 5]), &resume));

    // from_partition=0, advanced → new
    assert!(cmp.position_reached(&encode_position(0, &[11, 10, 5]), &resume));

    // from_partition=1, NOT advanced → not new
    assert!(!cmp.position_reached(&encode_position(1, &[10, 10, 5]), &resume));
}

#[test]
fn poc_malformed_event_position() {
    let cmp = PartitionAwareComparator;
    let good_resume = encode_position(0, &[5, 5]);

    // Too short
    assert!(!cmp.position_reached(&Bytes::from(vec![0u8; 3]), &good_resume));
    // Wrong length (says 2 partitions but only has 1)
    let mut bad = Vec::new();
    bad.extend_from_slice(&0u32.to_be_bytes()); // from_partition
    bad.extend_from_slice(&2u32.to_be_bytes()); // partition_count = 2
    bad.extend_from_slice(&42i64.to_be_bytes()); // only 1 offset
    assert!(!cmp.position_reached(&Bytes::from(bad), &good_resume));
}

#[test]
fn poc_malformed_resume_position() {
    let cmp = PartitionAwareComparator;
    let good_event = encode_position(0, &[6, 5]);

    // Malformed resume → treat all events as new (safe default)
    assert!(cmp.position_reached(&good_event, &Bytes::from(vec![0u8; 3])));
}

#[test]
fn poc_encode_decode_roundtrip() {
    // Basic roundtrip
    let pos = encode_position(2, &[100, 200, 300]);
    let (from_p, offsets) = decode_position(&pos).unwrap();
    assert_eq!(from_p, 2);
    assert_eq!(offsets, vec![100, 200, 300]);

    // Single partition
    let pos2 = encode_position(0, &[42]);
    let (from_p2, offsets2) = decode_position(&pos2).unwrap();
    assert_eq!(from_p2, 0);
    assert_eq!(offsets2, vec![42]);

    // Large offsets
    let pos3 = encode_position(0, &[i64::MAX, i64::MIN, 0]);
    let (from_p3, offsets3) = decode_position(&pos3).unwrap();
    assert_eq!(from_p3, 0);
    assert_eq!(offsets3, vec![i64::MAX, i64::MIN, 0]);
}

/// Verify the interleaving order doesn't matter — the comparator gives the
/// same answer regardless of what order events arrive in.
#[test]
fn poc_interleaving_order_independent() {
    let cmp = PartitionAwareComparator;
    let resume = encode_position(0, &[10, 5]);

    // An event from P0 offset 8 (next=9) with P1 at 5
    let ev_a = encode_position(0, &[9, 5]);
    // Same event from P0 offset 8 but P1 has advanced to 6 (different interleaving)
    let ev_b = encode_position(0, &[9, 6]);

    // Both should give the same result for P0: 9>10? NO
    assert_eq!(
        cmp.position_reached(&ev_a, &resume),
        cmp.position_reached(&ev_b, &resume),
        "P1's value should not affect P0 comparison"
    );
    assert!(!cmp.position_reached(&ev_a, &resume));
    assert!(!cmp.position_reached(&ev_b, &resume));
}

/// Verify that the from_partition in the RESUME position is ignored —
/// only the event's from_partition matters.
#[test]
fn poc_resume_from_partition_field_ignored() {
    let cmp = PartitionAwareComparator;

    // Same offsets, different from_partition in resume — should not affect result
    let event = encode_position(0, &[11, 5]); // from P0, P0=11
    let resume_fp0 = encode_position(0, &[10, 5]); // resume says from=0
    let resume_fp1 = encode_position(1, &[10, 5]); // resume says from=1

    // Both should give same answer: P0 11>10? YES
    assert!(cmp.position_reached(&event, &resume_fp0));
    assert!(cmp.position_reached(&event, &resume_fp1));
}
