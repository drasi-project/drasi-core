// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use drasi_core::interface::SourceCheckpoint;
use drasi_lib::computation::v1::{SourceProgressKey, StreamId};

fn position(version: u32, end: u32) -> Position {
    Position {
        version,
        binding: [7; 32],
        file: "mysql-bin.000001".into(),
        start: 4,
        end,
        digest: [version as u8; 32],
    }
}

fn state(position: Option<Position>, transport: u64) -> Arc<SourceProgressSnapshot> {
    Arc::new(SourceProgressSnapshot {
        ready: true,
        recovered: true,
        admitting: true,
        persistent: true,
        bootstrap_complete: true,
        checkpoints: position
            .into_iter()
            .map(|position| {
                (
                    SourceProgressKey::Source("source".into()),
                    SourceCheckpoint::new(
                        position.sequence().expect("sequence"),
                        Some(Bytes::from(
                            serde_json::to_vec(&position).expect("position"),
                        )),
                    ),
                )
            })
            .collect(),
        transport_sequences: BTreeMap::from([(
            StreamId::try_new("changes").expect("stream"),
            transport,
        )]),
        ..Default::default()
    })
}

#[test]
fn minimum_selects_one_actual_member_and_fresh_members_require_the_configured_floor() {
    let slow = state(Some(position(1, 100)), 900);
    let fast = state(Some(position(2, 200)), 3);
    let source = ComponentId::try_new("source").expect("source");
    let mut group = ConsumerProgressSnapshot {
        consumers: BTreeMap::from([
            (ComponentId::try_new("slow").expect("slow"), slow.clone()),
            (ComponentId::try_new("fast").expect("fast"), fast),
        ]),
    };
    let initial = Starting::Consumers(group.clone());
    assert!(std::ptr::eq(
        initial
            .minimum(&source, [7; 32])
            .expect("minimum")
            .expect("member"),
        slow.as_ref(),
    ));
    let fresh = state(None, 1000);
    group
        .consumers
        .insert(ComponentId::try_new("fresh").expect("fresh"), fresh.clone());
    let initial = Starting::Consumers(group);
    assert!(std::ptr::eq(
        initial
            .minimum(&source, [7; 32])
            .expect("minimum")
            .expect("member"),
        fresh.as_ref(),
    ));
}

#[test]
fn every_member_must_have_valid_persistent_unfenced_progress_even_when_not_the_minimum() {
    let source = ComponentId::try_new("source").expect("source");
    for fault in [
        "persistence",
        "failure",
        "readiness",
        "admission",
        "generation-shape",
        "binding",
    ] {
        let mut fast = state(Some(position(1, 200)), 1).as_ref().clone();
        match fault {
            "persistence" => fast.persistent = false,
            "failure" => fast.failure = Some("failed".into()),
            "readiness" => fast.ready = false,
            "admission" => fast.admitting = false,
            "generation-shape" => {
                fast.checkpoints
                    .get_mut(&SourceProgressKey::Source("source".into()))
                    .expect("checkpoint")
                    .sequence += 1;
            }
            "binding" => {
                let mut other = position(1, 200);
                other.binding = [8; 32];
                fast = state(Some(other), 1).as_ref().clone();
            }
            _ => unreachable!(),
        }
        let group = Starting::Consumers(ConsumerProgressSnapshot {
            consumers: BTreeMap::from([
                (
                    ComponentId::try_new("slow").expect("slow"),
                    state(Some(position(1, 100)), 999),
                ),
                (ComponentId::try_new("fast").expect("fast"), Arc::new(fast)),
            ]),
        });
        assert!(group.minimum(&source, [7; 32]).is_err(), "{fault}");
    }
    assert!(Starting::Consumers(ConsumerProgressSnapshot {
        consumers: BTreeMap::new()
    })
    .minimum(&source, [7; 32])
    .is_err());
}

#[test]
fn physical_cursor_order_preserves_snapshot_boundaries_and_rejects_contradictory_history() {
    let transaction = position(1, 100);
    let boundary = position(2, 100);
    assert_eq!(
        compare(&transaction, &boundary).expect("same boundary"),
        Ordering::Equal
    );
    assert_eq!(
        compare(&position(1, 150), &boundary).expect("later"),
        Ordering::Greater
    );
    let mut contradictory = transaction.clone();
    contradictory.digest = [9; 32];
    assert!(compare(&transaction, &contradictory).is_err());
    contradictory = transaction.clone();
    contradictory.file = "other-bin.000002".into();
    assert!(compare(&transaction, &contradictory).is_err());
    contradictory = transaction.clone();
    contradictory.binding = [9; 32];
    assert!(compare(&transaction, &contradictory).is_err());
    contradictory = transaction.clone();
    contradictory.version = 3;
    assert!(compare(&transaction, &contradictory).is_err());
    let mut next_file = position(1, 20);
    next_file.file = "mysql-bin.000002".into();
    assert_eq!(
        compare(&next_file, &transaction).expect("rotated"),
        Ordering::Greater
    );
    assert!(Positions.is_after(b"not a position", b"{}").is_err());
}

#[test]
fn checkpoint_position_bounds_are_enforced_before_decoding() {
    let position = position(1, 100);
    let mut snapshot = state(Some(position.clone()), 1).as_ref().clone();
    let key = SourceProgressKey::Source("source".into());
    let mut bytes = serde_json::to_vec(&position).expect("position");
    bytes.resize(
        drasi_lib::sources::SourceBase::MAX_SOURCE_POSITION_BYTES,
        b' ',
    );
    snapshot
        .checkpoints
        .get_mut(&key)
        .expect("checkpoint")
        .source_position = Some(Bytes::from(bytes.clone()));
    assert_eq!(
        source::checkpoint(
            &snapshot,
            &ComponentId::try_new("source").expect("source"),
            [7; 32]
        )
        .expect("exact bound"),
        Some(position),
    );
    bytes.push(b' ');
    snapshot
        .checkpoints
        .get_mut(&key)
        .expect("checkpoint")
        .source_position = Some(Bytes::from(bytes));
    assert!(source::checkpoint(
        &snapshot,
        &ComponentId::try_new("source").expect("source"),
        [7; 32],
    )
    .expect_err("over bound")
    .to_string()
    .contains("position limit"));
}
