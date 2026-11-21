// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use handled_branch_ownership::{
    BranchKey, Error, Limits, Registry, BRANCH_BYTES, LEASE_BYTES, ORIGIN_BYTES,
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Barrier,
    },
    time::Duration,
};

type Rig = (Arc<Registry>, Arc<AtomicU64>);

fn rig() -> Rig {
    let clock = Arc::new(AtomicU64::new(0));
    let registry = Registry::controlled(
        Limits {
            records: 64,
            bytes: 64 * 1024,
        },
        clock.clone(),
    )
    .unwrap();
    (registry, clock)
}

fn key(output: u64, sink: u64) -> BranchKey {
    BranchKey { output, sink }
}

fn advance(clock: &AtomicU64, milliseconds: u64) {
    clock.fetch_add(milliseconds * 1_000_000, Ordering::AcqRel);
}

#[test]
fn fan_out_and_one_to_many_share_start_not_completion() {
    let (registry, clock) = rig();
    let origin = registry.start().unwrap();
    for branch in [key(1, 1), key(1, 2), key(2, 1)] {
        registry.prepare(origin.token(), branch, 1).unwrap();
    }
    advance(&clock, 100);
    assert_eq!(
        registry.handled(origin.token(), key(1, 1), &[0]).unwrap(),
        Some(Duration::from_millis(100))
    );
    advance(&clock, 150);
    for branch in [key(1, 2), key(2, 1)] {
        assert_eq!(
            registry.handled(origin.token(), branch, &[0]).unwrap(),
            Some(Duration::from_millis(250))
        );
    }
    assert_eq!(registry.diagnostics().unwrap().observations, 3);
    drop(origin);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn combined_batch_preserves_each_start_and_split_rows_are_idempotent() {
    let (registry, clock) = rig();
    let first = registry.start().unwrap();
    advance(&clock, 50);
    let second = registry.start().unwrap();
    registry.prepare(first.token(), key(1, 1), 2).unwrap();
    registry.prepare(second.token(), key(2, 1), 1).unwrap();
    let split = first.clone();
    advance(&clock, 50);
    assert_eq!(
        registry.handled(first.token(), key(1, 1), &[0, 0]).unwrap(),
        None
    );
    assert_eq!(
        registry.handled(split.token(), key(1, 1), &[0]).unwrap(),
        None
    );
    drop(first);
    advance(&clock, 150);
    assert_eq!(
        registry.handled(split.token(), key(1, 1), &[1]).unwrap(),
        Some(Duration::from_millis(250))
    );
    assert_eq!(
        registry.handled(second.token(), key(2, 1), &[0]).unwrap(),
        Some(Duration::from_millis(200))
    );
    drop((split, second));
    assert_eq!(registry.diagnostics().unwrap().records, 0);
}

#[test]
fn failed_attempt_and_checkpoint_failure_do_not_reset_or_duplicate_timing() {
    let (registry, clock) = rig();
    let input = registry.start().unwrap();
    registry.prepare(input.token(), key(1, 1), 1).unwrap();
    advance(&clock, 100);
    // A failed effect supplies no positive row completion.
    assert_eq!(registry.diagnostics().unwrap().observations, 0);
    let retry = registry.acquire(input.token()).unwrap();
    drop(input);
    advance(&clock, 300);
    assert_eq!(
        registry.handled(retry.token(), key(1, 1), &[0]).unwrap(),
        Some(Duration::from_millis(400))
    );
    let replay = registry.acquire(retry.token()).unwrap();
    drop(retry);
    advance(&clock, 400);
    registry.prepare(replay.token(), key(1, 1), 1).unwrap();
    assert_eq!(
        registry.handled(replay.token(), key(1, 1), &[0]).unwrap(),
        None
    );
    assert_eq!(registry.diagnostics().unwrap().observations, 1);
    drop(replay);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn count_saturation_preserves_admitted_work_and_does_not_resurrect_skipped_branches() {
    let registry = Registry::new(Limits {
        records: 4,
        bytes: 64 * 1024,
    })
    .unwrap();
    let origin = registry.start().unwrap();
    registry.prepare(origin.token(), key(1, 1), 1).unwrap();
    let transfer = registry.acquire(origin.token()).unwrap();
    assert_eq!(registry.diagnostics().unwrap().records, 4);
    assert!(matches!(registry.start(), Err(Error::Capacity)));
    assert_eq!(
        registry.prepare(origin.token(), key(1, 2), 1),
        Err(Error::Capacity)
    );
    drop(transfer);
    assert_eq!(
        registry.prepare(origin.token(), key(1, 2), 1),
        Err(Error::Sealed)
    );
    assert!(registry
        .handled(origin.token(), key(1, 1), &[0])
        .unwrap()
        .is_some());
    drop(origin);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn byte_admission_precedes_row_allocation_and_exact_boundary_is_supported() {
    let budget = ORIGIN_BYTES + LEASE_BYTES + BRANCH_BYTES + 8;
    for rows in [65, usize::MAX] {
        let registry = Registry::new(Limits {
            records: 100,
            bytes: budget,
        })
        .unwrap();
        let origin = registry.start().unwrap();
        assert_eq!(
            registry.prepare(origin.token(), key(1, 1), rows),
            Err(Error::Capacity)
        );
        assert_eq!(registry.diagnostics().unwrap().records, 2);
        assert_eq!(
            registry.diagnostics().unwrap().bytes,
            ORIGIN_BYTES + LEASE_BYTES
        );
    }
    let registry = Registry::new(Limits {
        records: 100,
        bytes: budget,
    })
    .unwrap();
    let origin = registry.start().unwrap();
    registry.prepare(origin.token(), key(1, 1), 64).unwrap();
    assert_eq!(registry.diagnostics().unwrap().bytes, budget);
    drop(origin);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn invalid_rows_are_atomic_and_unknown_branches_are_not_created_by_completion() {
    let (registry, _) = rig();
    let origin = registry.start().unwrap();
    assert_eq!(
        registry.handled(origin.token(), key(1, 1), &[0]),
        Err(Error::Invalid)
    );
    assert_eq!(
        registry.prepare(origin.token(), key(1, 1), 0),
        Err(Error::Invalid)
    );
    registry.prepare(origin.token(), key(1, 1), 2).unwrap();
    assert_eq!(
        registry.prepare(origin.token(), key(1, 1), 1),
        Err(Error::Invalid)
    );
    assert_eq!(
        registry.handled(origin.token(), key(1, 1), &[0, 2]),
        Err(Error::Invalid)
    );
    assert_eq!(
        registry.handled(origin.token(), key(1, 1), &[1]).unwrap(),
        None
    );
    assert!(registry
        .handled(origin.token(), key(1, 1), &[0])
        .unwrap()
        .is_some());
}

#[test]
fn foreign_expired_and_retired_tokens_cannot_authorize_new_observations() {
    let (one, _) = rig();
    let (two, _) = rig();
    let origin = one.start().unwrap();
    let token = origin.token();
    assert!(matches!(two.acquire(token), Err(Error::Invalid)));
    drop(origin);
    assert!(matches!(one.acquire(token), Err(Error::Invalid)));
    let replacement = one.start().unwrap();
    assert_ne!(replacement.token(), token);
    one.prepare(replacement.token(), key(1, 1), 1).unwrap();
    one.retire().unwrap();
    assert!(matches!(
        one.acquire(replacement.token()),
        Err(Error::Closed)
    ));
    assert_eq!(
        one.handled(replacement.token(), key(1, 1), &[0]),
        Err(Error::Closed)
    );
    drop(replacement);
    assert_eq!(one.diagnostics().unwrap().records, 0);
}

#[test]
fn no_age_expiry_and_no_completed_history_after_final_lease() {
    let (registry, clock) = rig();
    for _ in 0..100 {
        let origin = registry.start().unwrap();
        let weak = Arc::downgrade(&registry);
        registry.prepare(origin.token(), key(1, 1), 1).unwrap();
        advance(&clock, 86_400_000);
        assert_eq!(
            registry.handled(origin.token(), key(1, 1), &[0]).unwrap(),
            Some(Duration::from_secs(86_400))
        );
        drop(origin);
        assert_eq!(registry.diagnostics().unwrap().records, 0);
        assert_eq!(registry.diagnostics().unwrap().bytes, 0);
        assert!(weak.upgrade().is_some());
    }
    let weak = Arc::downgrade(&registry);
    drop(registry);
    assert!(weak.upgrade().is_none());
}

#[test]
fn concurrent_completion_has_one_linearized_success() {
    let (registry, clock) = rig();
    let origin = registry.start().unwrap();
    registry.prepare(origin.token(), key(1, 1), 1).unwrap();
    advance(&clock, 123);
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let registry = registry.clone();
            let origin = origin.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                registry.handled(origin.token(), key(1, 1), &[0]).unwrap()
            })
        })
        .collect();
    let observations: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(observations, [Duration::from_millis(123)]);
    assert_eq!(registry.diagnostics().unwrap().observations, 1);
    drop(origin);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
}

#[test]
fn dropping_unhandled_and_partially_handled_inputs_releases_every_charge() {
    let (registry, _) = rig();
    let untouched = registry.start().unwrap();
    let partial = registry.start().unwrap();
    registry.prepare(partial.token(), key(1, 1), 2).unwrap();
    assert_eq!(
        registry.handled(partial.token(), key(1, 1), &[0]).unwrap(),
        None
    );
    drop((untouched, partial));
    let diagnostics = registry.diagnostics().unwrap();
    assert_eq!(diagnostics.records, 0);
    assert_eq!(diagnostics.bytes, 0);
    assert_eq!(diagnostics.observations, 0);
}
