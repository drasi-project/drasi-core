// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core_types::interface::SourceCheckpoint;
use std::sync::Arc;

const BINDING: [u8; 32] = [9; 32];

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}

fn row(commit: u64, previous: u64, offset: usize, count: usize, sequence: u64) -> RowPosition {
    RowPosition {
        version: 1,
        binding: BINDING,
        commit_lsn: commit,
        previous_lsn: previous,
        offset: Some(offset),
        count,
        sequence,
        managed_slot: None,
    }
}

fn boundary(commit: u64) -> RowPosition {
    RowPosition::snapshot(BINDING, format!("owned_{}", "a".repeat(32)), commit)
}

fn state(position: &RowPosition) -> Arc<SourceProgressSnapshot> {
    Arc::new(SourceProgressSnapshot {
        ready: true,
        admitting: true,
        recovered: true,
        persistent: false,
        checkpoints: BTreeMap::from([(
            SourceProgressKey::Source("source".into()),
            SourceCheckpoint::new(
                position.sequence,
                Some(serde_json::to_vec(position).unwrap().into()),
            ),
        )]),
        ..Default::default()
    })
}

fn group(positions: &[(&str, RowPosition)]) -> (ConsumerProgressSnapshot, BTreeSet<ComponentId>) {
    let snapshot = ConsumerProgressSnapshot {
        consumers: positions
            .iter()
            .map(|(consumer, position)| (id(consumer), state(position)))
            .collect(),
    };
    let expected = snapshot.consumers.keys().cloned().collect();
    (snapshot, expected)
}

#[test]
fn configured_snapshot_has_an_explicit_format_without_managed_slot_ownership() -> Result<()> {
    let configured = RowPosition::configured_snapshot(BINDING, 100);
    configured.validate()?;
    assert!(configured.is_configured_snapshot());
    assert_eq!(configured.feedback_lsn(), 100);
    assert_eq!(
        compare_row_positions(&configured, &row(100, 80, 2, 3, 9))?,
        Ordering::Equal
    );
    assert_eq!(
        compare_row_positions(&configured, &row(100, 80, 1, 3, 10))?,
        Ordering::Greater
    );
    let (snapshot, members) = group(&[
        ("snapshot", configured.clone()),
        ("partial", row(200, 50, 0, 2, 30)),
    ]);
    let minimum = minimum_row_checkpoint(&snapshot, &members, &id("source"), BINDING)?.unwrap();
    assert_eq!(minimum.position.commit_lsn, 100);
    assert_eq!(minimum.feedback_lsn, 50);
    let mut invalid = configured.clone();
    invalid.version = 1;
    assert!(
        invalid.validate().is_err(),
        "old snapshots must retain actual managed ownership"
    );
    invalid = configured.clone();
    invalid.managed_slot = Some("must-not-own".into());
    assert!(invalid.validate().is_err());
    invalid = row(100, 80, 0, 3, 1);
    invalid.version = 2;
    assert!(
        invalid.validate().is_err(),
        "version two is not a new ordinary row format"
    );
    invalid = configured;
    invalid.sequence = 1;
    assert!(invalid.validate().is_err());
    Ok(())
}

#[test]
fn wal_order_does_not_compare_independent_logical_or_transport_sequences() -> Result<()> {
    let earlier = row(100, 50, 0, 3, 9000);
    let later = row(101, 80, 0, 2, 1);
    assert_eq!(compare_row_positions(&earlier, &later)?, Ordering::Less);
    assert_eq!(compare_row_positions(&later, &earlier)?, Ordering::Greater);
    assert_eq!(
        compare_row_positions(&earlier, &row(100, 50, 1, 3, 4))?,
        Ordering::Less
    );
    assert_eq!(
        compare_row_positions(&earlier, &row(100, 70, 0, 3, 1))?,
        Ordering::Equal,
        "different initial feedback floors do not change the WAL row position"
    );
    assert_eq!(
        compare_row_positions(&later, &boundary(200))?,
        Ordering::Less
    );
    Ok(())
}

#[test]
fn bootstrap_boundaries_are_after_commit_not_row_zero() -> Result<()> {
    let snapshot = boundary(100);
    let partial = row(100, 80, 1, 3, 42);
    let completed = row(100, 80, 2, 3, 43);
    assert_eq!(compare_row_positions(&partial, &snapshot)?, Ordering::Less);
    assert_eq!(
        compare_row_positions(&snapshot, &partial)?,
        Ordering::Greater
    );
    assert_eq!(
        compare_row_positions(&completed, &snapshot)?,
        Ordering::Equal
    );
    assert_eq!(
        compare_row_positions(&snapshot, &completed)?,
        Ordering::Equal
    );
    assert_eq!(
        compare_row_positions(&row(99, 80, 0, 1, 9000), &snapshot)?,
        Ordering::Less
    );
    assert_eq!(
        compare_row_positions(&snapshot, &row(101, 100, 0, 2, 1))?,
        Ordering::Less
    );
    Ok(())
}

#[test]
fn comparisons_reject_different_bindings_invalid_positions_and_inconsistent_counts() {
    let valid = row(100, 80, 2, 3, 43);
    let mut other_binding = boundary(200);
    other_binding.binding = [8; 32];
    assert!(compare_row_positions(&valid, &other_binding).is_err());
    assert!(compare_row_positions(&valid, &row(100, 80, 3, 4, 44)).is_err());
    assert!(compare_row_positions(&valid, &row(100, 80, 3, 3, 44)).is_err());
    assert!(compare_row_positions(&valid, &row(100, 100, 0, 1, 1)).is_err());
    let mut invalid_snapshot = boundary(100);
    invalid_snapshot.count = 3;
    assert!(compare_row_positions(&valid, &invalid_snapshot).is_err());
}

#[test]
fn full_width_positions_order_without_incrementing_the_lsn_or_overflowing_offsets() -> Result<()> {
    let partial = row(u64::MAX, u64::MAX - 1, usize::MAX - 2, usize::MAX, u64::MAX);
    let complete = row(u64::MAX, u64::MAX - 1, usize::MAX - 1, usize::MAX, 1);
    let snapshot = boundary(u64::MAX);
    assert_eq!(compare_row_positions(&partial, &complete)?, Ordering::Less);
    assert_eq!(
        compare_row_positions(&complete, &snapshot)?,
        Ordering::Equal
    );
    assert_eq!(partial.feedback_lsn(), u64::MAX - 1);
    assert_eq!(snapshot.feedback_lsn(), u64::MAX);
    assert!(
        compare_row_positions(&row(u64::MAX, 0, usize::MAX, usize::MAX, 1), &snapshot).is_err()
    );
    Ok(())
}

#[test]
fn minimum_selects_an_actual_member_cursor_without_aggregating_durability() -> Result<()> {
    let expected_position = row(100, 80, 0, 3, 9000);
    let (mut snapshot, expected) = group(&[
        ("ahead", row(200, 180, 0, 1, 1)),
        ("behind", expected_position.clone()),
    ]);
    {
        let state = Arc::make_mut(snapshot.consumers.get_mut(&id("behind")).unwrap());
        state.persistent = true;
        state.reset_generation = 999;
        state
            .transport_sequences
            .insert(StreamId::try_new("source/out")?, u64::MAX);
    }
    let before = snapshot.consumers.clone();
    let minimum = minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING)?
        .context("ready minimum")?;
    assert_eq!(minimum.consumer, id("behind"));
    assert_eq!(
        serde_json::to_value(&minimum.position)?,
        serde_json::to_value(&expected_position)?
    );
    assert_eq!(minimum.feedback_lsn, 80);
    for (consumer, state) in &snapshot.consumers {
        assert!(Arc::ptr_eq(state, &before[consumer]));
    }
    assert!(!snapshot.consumers[&id("ahead")].persistent);
    assert!(snapshot.consumers[&id("behind")].persistent);
    Ok(())
}

#[test]
fn feedback_uses_every_partial_transaction_not_only_the_minimum_cursor() -> Result<()> {
    let (snapshot, expected) = group(&[
        ("initial", boundary(80)),
        ("partial", row(100, 50, 0, 3, 1000)),
    ]);
    let minimum = minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING)?
        .context("ready minimum")?;
    assert_eq!(minimum.consumer, id("initial"));
    assert_eq!(minimum.position.feedback_lsn(), 80);
    assert_eq!(minimum.feedback_lsn, 50);

    let (snapshot, expected) = group(&[
        ("first", row(100, 70, 1, 3, 1000)),
        ("second", row(100, 0, 1, 3, 2)),
    ]);
    let minimum = minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING)?
        .context("equal WAL positions")?;
    assert_eq!(minimum.consumer, id("first"));
    assert_eq!(
        minimum.feedback_lsn, 0,
        "zero is a real conservative floor, not a missing value"
    );

    let (snapshot, expected) = group(&[
        ("first", row(100, 50, 2, 3, 1000)),
        ("second", boundary(100)),
    ]);
    assert_eq!(
        minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING)?
            .context("completed commit")?
            .feedback_lsn,
        100
    );
    Ok(())
}

#[test]
fn missing_unready_and_cursorless_members_fence_feedback() -> Result<()> {
    for condition in [
        "missing",
        "extra",
        "cursor",
        "ready",
        "recovered",
        "admitting",
    ] {
        let (mut snapshot, expected) = group(&[
            ("first", row(100, 80, 0, 1, 5)),
            ("second", row(200, 180, 0, 1, 1)),
        ]);
        match condition {
            "missing" => {
                snapshot.consumers.remove(&id("second"));
            }
            "extra" => {
                snapshot
                    .consumers
                    .insert(id("undeclared"), state(&boundary(300)));
            }
            _ => {
                let state = Arc::make_mut(snapshot.consumers.get_mut(&id("second")).unwrap());
                match condition {
                    "cursor" => state.checkpoints.clear(),
                    "ready" => state.ready = false,
                    "recovered" => state.recovered = false,
                    "admitting" => state.admitting = false,
                    _ => unreachable!(),
                }
            }
        }
        assert!(
            minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING)?.is_none(),
            "{condition}"
        );
    }
    let empty = ConsumerProgressSnapshot {
        consumers: BTreeMap::new(),
    };
    assert!(minimum_row_checkpoint(&empty, &BTreeSet::new(), &id("source"), BINDING)?.is_none());
    Ok(())
}

#[test]
fn failed_rebound_and_corrupt_members_error_even_while_another_member_is_pending() -> Result<()> {
    for condition in ["failed", "binding", "sequence", "position"] {
        let (mut snapshot, expected) = group(&[
            ("pending", boundary(80)),
            ("invalid", row(100, 80, 0, 1, 1)),
        ]);
        Arc::make_mut(snapshot.consumers.get_mut(&id("pending")).unwrap()).ready = false;
        let state = Arc::make_mut(snapshot.consumers.get_mut(&id("invalid")).unwrap());
        match condition {
            "failed" => state.failure = Some("owner is fenced".into()),
            "binding" => {
                let mut position = row(100, 80, 0, 1, 1);
                position.binding = [4; 32];
                state
                    .checkpoints
                    .get_mut(&SourceProgressKey::Source("source".into()))
                    .unwrap()
                    .source_position = Some(serde_json::to_vec(&position)?.into());
            }
            "sequence" => {
                state
                    .checkpoints
                    .get_mut(&SourceProgressKey::Source("source".into()))
                    .unwrap()
                    .sequence = 2
            }
            "position" => {
                state
                    .checkpoints
                    .get_mut(&SourceProgressKey::Source("source".into()))
                    .unwrap()
                    .source_position = None
            }
            _ => unreachable!(),
        }
        assert!(
            minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING).is_err(),
            "{condition}"
        );
    }
    Ok(())
}

#[test]
fn count_conflicts_between_nonminimum_members_are_not_hidden() -> Result<()> {
    let (snapshot, expected) = group(&[
        ("minimum", boundary(10)),
        ("later-first", row(100, 80, 0, 2, 1)),
        ("later-second", row(100, 80, 1, 3, 500)),
    ]);
    assert!(minimum_row_checkpoint(&snapshot, &expected, &id("source"), BINDING).is_err());
    Ok(())
}
