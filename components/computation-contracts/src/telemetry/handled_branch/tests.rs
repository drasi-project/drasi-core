// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::telemetry::{ExecutionContext, TraceContextV1};

fn tracker() -> Arc<HandledBranchTracker> {
    HandledBranchTracker::new(HandledBranchLimits::default()).unwrap()
}

#[test]
fn opt_in_policy_obeys_profiles_signals_exclusions_and_validates_limits() {
    use crate::telemetry::{TelemetryProfile, TelemetrySettings};
    let name = "drasi.pipeline.end_to_end.duration";
    for profile in [
        TelemetryProfile::Off,
        TelemetryProfile::Basic,
        TelemetryProfile::Debug,
        TelemetryProfile::Persistence,
    ] {
        assert!(!TelemetrySettings::new(profile)
            .resolve()
            .unwrap()
            .allows_metric(name));
        for metrics in [true, false] {
            let policy = TelemetrySettings::new(profile)
                .with_metrics(metrics)
                .with_tracing(false)
                .with_handled_branch_latency(Default::default())
                .resolve()
                .unwrap();
            assert_eq!(
                policy.allows_metric(name),
                profile != TelemetryProfile::Off && metrics
            );
            let excluded = TelemetrySettings::new(profile)
                .with_handled_branch_latency(Default::default())
                .exclude_metrics("end_to_end")
                .resolve()
                .unwrap();
            assert!(excluded.handled_branch_limits().is_none());
        }
    }
    assert!(TelemetrySettings::new(TelemetryProfile::Basic)
        .with_handled_branch_latency(HandledBranchLimits {
            max_records: 0,
            max_bytes: 0
        })
        .resolve()
        .is_err());
}

fn key(output: u64, sink: &str) -> HandledBranchKey {
    HandledBranchKey {
        producer_epoch: [1; 16],
        output_sequence: output,
        sink: sink.into(),
    }
}

fn trace(id: u8) -> ExecutionContext {
    ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], 0, None).unwrap())
}

#[tokio::test(start_paused = true)]
async fn fan_out_outputs_and_batches_keep_independent_completion() {
    let tracker = tracker();
    let first = tracker.start().unwrap();
    let one = first.token().unwrap();
    for branch in [key(1, "a"), key(1, "b"), key(2, "a")] {
        tracker.prepare(one, &branch, 1).unwrap();
    }
    tokio::time::advance(Duration::from_millis(50)).await;
    let second = tracker.start().unwrap();
    let two = second.token().unwrap();
    tracker.prepare(two, &key(3, "a"), 1).unwrap();
    tokio::time::advance(Duration::from_millis(50)).await;
    assert_eq!(
        tracker.handled_all(one, &key(1, "a")).unwrap(),
        Some(Duration::from_millis(100))
    );
    tokio::time::advance(Duration::from_millis(150)).await;
    for branch in [key(1, "b"), key(2, "a")] {
        assert_eq!(
            tracker.handled_all(one, &branch).unwrap(),
            Some(Duration::from_millis(250))
        );
    }
    assert_eq!(
        tracker.handled_all(two, &key(3, "a")).unwrap(),
        Some(Duration::from_millis(200))
    );
    drop((first, second));
    let diagnostics = tracker.diagnostics().unwrap();
    assert_eq!(diagnostics.completed_branches, 4);
    assert_eq!(diagnostics.records, 0);
    assert_eq!(diagnostics.record_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn split_rows_retries_and_long_lived_work_do_not_duplicate_or_expire() {
    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let branch = key(1, "sink");
    tracker.prepare(token, &branch, 3).unwrap();
    assert!(tracker.handled_rows(token, &branch, &[0, 3]).is_err());
    assert_eq!(tracker.handled_rows(token, &branch, &[1, 1]).unwrap(), None);
    let retry = tracker.acquire(token).unwrap();
    drop(origin);
    let token = retry.token().unwrap();
    assert_eq!(tracker.handled_rows(token, &branch, &[1, 2]).unwrap(), None);
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_eq!(
        tracker.handled_rows(token, &branch, &[0]).unwrap(),
        Some(Duration::from_secs(86_400))
    );
    tracker.prepare(token, &branch, 3).unwrap();
    assert_eq!(tracker.handled_all(token, &branch).unwrap(), None);
    assert_eq!(tracker.diagnostics().unwrap().duplicate_completions, 1);
    drop(retry);
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
}

#[test]
fn count_saturation_seals_new_branches_without_evicting_admitted_work() {
    let tracker = HandledBranchTracker::new(HandledBranchLimits {
        max_records: 4,
        ..Default::default()
    })
    .unwrap();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let branch = key(1, "sink");
    tracker.prepare(token, &branch, 1).unwrap();
    let transfer = tracker.retain_token(token).unwrap();
    assert_eq!(tracker.diagnostics().unwrap().records, 4);
    assert!(matches!(
        tracker.retain_token(token),
        Err(TelemetryError::Capacity(_))
    ));
    tracker.release_token(transfer).unwrap();
    assert!(tracker.prepare(token, &key(2, "sink"), 1).is_err());
    assert!(tracker.handled_all(token, &branch).unwrap().is_some());
    drop(origin);
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
}

#[test]
fn byte_limit_checks_row_storage_before_allocating() {
    let branch = key(1, "sink");
    let budget = ORIGIN_BYTES + LEASE_BYTES + BRANCH_BYTES + branch.sink.len() + 8;
    for rows in [65, usize::MAX] {
        let tracker = HandledBranchTracker::new(HandledBranchLimits {
            max_records: 100,
            max_bytes: budget,
        })
        .unwrap();
        let origin = tracker.start().unwrap();
        assert!(matches!(
            tracker.prepare(origin.token().unwrap(), &branch, rows),
            Err(TelemetryError::Capacity(_))
        ));
        let diagnostics = tracker.diagnostics().unwrap();
        assert_eq!(diagnostics.records, 2);
        assert_eq!(diagnostics.record_bytes, ORIGIN_BYTES + LEASE_BYTES);
    }
    let tracker = HandledBranchTracker::new(HandledBranchLimits {
        max_records: 100,
        max_bytes: budget,
    })
    .unwrap();
    let origin = tracker.start().unwrap();
    tracker
        .prepare(origin.token().unwrap(), &branch, 64)
        .unwrap();
    assert_eq!(tracker.diagnostics().unwrap().record_bytes, budget);
    drop(origin);
    assert_eq!(tracker.diagnostics().unwrap().record_bytes, 0);
}

#[test]
fn default_record_and_byte_limits_admit_the_exact_boundary_only() {
    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let limits = HandledBranchLimits::default();
    for sequence in 0..(limits.max_records - 2) as u64 {
        tracker.prepare(token, &key(sequence, "sink"), 1).unwrap();
    }
    assert_eq!(tracker.diagnostics().unwrap().records, limits.max_records);
    assert!(matches!(
        tracker.prepare(token, &key(limits.max_records as u64, "sink"), 1),
        Err(TelemetryError::Capacity(_))
    ));
    drop(origin);
    assert_eq!(tracker.diagnostics().unwrap().record_bytes, 0);

    let origin = tracker.start().unwrap();
    let branch = key(1, "12345678");
    let row_bytes =
        limits.max_bytes - ORIGIN_BYTES - LEASE_BYTES - BRANCH_BYTES - branch.sink.len();
    assert_eq!(row_bytes % size_of::<u64>(), 0);
    let rows = row_bytes / size_of::<u64>() * 64;
    tracker
        .prepare(origin.token().unwrap(), &branch, rows)
        .unwrap();
    assert_eq!(
        tracker.diagnostics().unwrap().record_bytes,
        limits.max_bytes
    );
    assert!(matches!(
        tracker.prepare(origin.token().unwrap(), &key(2, "sink"), 1),
        Err(TelemetryError::Capacity(_))
    ));
    drop(origin);
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
    assert_eq!(tracker.diagnostics().unwrap().record_bytes, 0);
}

#[test]
fn retirement_foreign_tokens_and_double_release_are_explicit() {
    let one = tracker();
    let two = tracker();
    let origin = one.start().unwrap();
    let token = origin.token().unwrap();
    assert!(two.acquire(token).is_err());
    let native = one.retain_token(token).unwrap();
    one.release_token(native).unwrap();
    assert!(one.release_token(native).is_err());
    one.prepare(token, &key(1, "sink"), 1).unwrap();
    one.retire().unwrap();
    assert!(matches!(one.acquire(token), Err(TelemetryError::Retired)));
    assert!(matches!(
        one.handled_all(token, &key(1, "sink")),
        Err(TelemetryError::Retired)
    ));
    drop(origin);
    assert_eq!(one.diagnostics().unwrap().records, 0);
    assert!(two.acquire(token).is_err());
}

#[test]
fn concurrent_completion_has_one_success_and_no_retained_history() {
    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let branch = key(1, "sink");
    tracker.prepare(token, &branch, 1).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let tracker = tracker.clone();
            let branch = branch.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                tracker.handled_all(token, &branch).unwrap()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .count(),
        1
    );
    let weak = Arc::downgrade(&tracker);
    drop(origin);
    assert_eq!(tracker.diagnostics().unwrap().record_bytes, 0);
    drop(tracker);
    assert!(weak.upgrade().is_none());
}

#[test]
fn invalid_limits_keys_and_row_set_changes_do_not_mutate_completion() {
    assert!(HandledBranchTracker::new(HandledBranchLimits {
        max_records: 1,
        max_bytes: usize::MAX,
    })
    .is_err());
    assert!(HandledBranchTracker::new(HandledBranchLimits {
        max_records: 100,
        max_bytes: 1,
    })
    .is_err());
    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    assert!(tracker.prepare(token, &key(1, ""), 1).is_err());
    assert!(tracker
        .prepare(token, &key(1, &"x".repeat(MAX_SINK_BYTES + 1)), 1)
        .is_err());
    assert!(tracker.prepare(token, &key(1, "sink"), 0).is_err());
    tracker.prepare(token, &key(1, "sink"), 2).unwrap();
    assert!(tracker.prepare(token, &key(1, "sink"), 1).is_err());
    assert_eq!(tracker.diagnostics().unwrap().completed_branches, 0);
}

#[test]
fn trace_replacement_preserves_timing_and_v1_transport_cannot_silently_drop_it() {
    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let context = trace(1).with_handled_branch(origin);
    let replaced = context.clone().with_trace_from(&trace(2));
    assert_eq!(replaced.trace(), trace(2).trace());
    assert_eq!(replaced.handled_branch().unwrap().token(), Some(token));
    assert!(matches!(
        replaced.encode(),
        Err(TelemetryError::Unsupported(_))
    ));
    let trace_only = replaced.without_handled_branch();
    assert_eq!(
        ExecutionContext::decode(&trace_only.encode().unwrap()).unwrap(),
        trace(2)
    );
    drop(context);
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
}

#[test]
fn explicitly_unavailable_context_is_not_empty_or_serialized_as_trace_only() {
    for reason in [
        HandledBranchUnavailable::Capacity,
        HandledBranchUnavailable::Replay,
        HandledBranchUnavailable::Ambiguous,
        HandledBranchUnavailable::Unsupported,
    ] {
        let context = ExecutionContext::default()
            .with_handled_branch(HandledBranchContext::Unavailable(reason));
        assert!(!context.is_empty());
        assert!(context.encode().is_err());
        assert_eq!(context.handled_branch().unwrap().token(), None);
    }
}

#[test]
fn batch_trace_selection_does_not_choose_a_timing_origin() {
    let tracker = tracker();
    let first = trace(1).with_handled_branch(tracker.start().unwrap());
    let second = trace(1).with_handled_branch(tracker.start().unwrap());
    let selected =
        crate::telemetry::BatchTraceParent::select([&first, &second], Default::default());
    assert_eq!(selected.parent.trace(), trace(1).trace());
    assert!(selected.parent.handled_branch().is_none());
    drop((first, second));
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
}

#[cfg(feature = "tracing-context")]
#[tokio::test]
async fn adapter_capture_poll_and_drop_keep_timing_without_ambient_leakage() {
    use crate::telemetry::{ExecutionScope, TraceContextAdapter, TraceContextGuard};
    use std::{future::Future, pin::pin};

    #[derive(Debug)]
    struct Adapter;
    struct Guard;
    impl TraceContextGuard for Guard {}
    impl TraceContextAdapter for Adapter {
        fn enter(&self, _: &ExecutionContext) -> Box<dyn TraceContextGuard> {
            Box::new(Guard)
        }
        fn current_context(&self) -> ExecutionContext {
            trace(2)
        }
        fn span_context(&self, _: &tracing::Span) -> ExecutionContext {
            trace(2)
        }
        fn set_parent(&self, _: &tracing::Span, _: &ExecutionContext) -> Result<()> {
            Ok(())
        }
        fn add_link(&self, _: &tracing::Span, _: &TraceContextV1) -> Result<()> {
            Ok(())
        }
    }

    let tracker = tracker();
    let origin = tracker.start().unwrap();
    let token = origin.token().unwrap();
    let scope = ExecutionScope::new(trace(1).with_handled_branch(origin))
        .with_adapter(Some(Arc::new(Adapter)));
    let future = scope.scope(async {
        let current = ExecutionContext::current();
        assert_eq!(current.trace(), trace(2).trace());
        assert_eq!(current.handled_branch().unwrap().token(), Some(token));
        std::future::pending::<()>().await;
    });
    drop(scope);
    {
        let mut future = pin!(future);
        futures::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(ExecutionContext::current().is_empty());
    }
    assert!(ExecutionContext::current().is_empty());
    assert_eq!(tracker.diagnostics().unwrap().records, 0);
}
