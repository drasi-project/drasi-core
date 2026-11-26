// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::Record;
use std::num::NonZeroU64;

fn file() -> StateTransfer {
    StateTransfer::TemporaryFile {
        max_file_bytes: NonZeroU64::new(1024 * 1024).unwrap(),
    }
}

fn options(transfer: StateTransfer) -> CurrentStateOptions {
    CurrentStateOptions {
        batches: SnapshotBatchLimits {
            max_records: NonZeroUsize::new(6).unwrap(),
            ..Default::default()
        },
        transfer,
    }
}

fn check_batch(envelope: ChangeEnvelope, rows: &mut im::HashMap<RecordId, Record>) {
    assert_eq!(
        envelope.changes().schema(),
        GraphChangeCodec::schema().descriptor()
    );
    assert!(GraphProducerProgress::from_envelope(&envelope)
        .unwrap()
        .is_none());
    assert!(envelope.system().control().is_none());
    for (i, operation) in envelope.changes().operations().iter().enumerate() {
        let ChangeOperation::Added { ordinal, after } = operation else {
            panic!("expected full addition")
        };
        assert_eq!(*ordinal, i as u64);
        assert!(rows
            .insert(after.identity().clone(), after.clone())
            .is_none());
    }
}

#[tokio::test]
async fn one_shot_ends_at_40_and_only_follow_receives_41_and_42() {
    let fixture = started_fixture(1, 1).await;
    let handle = fixture
        .core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("native", "out"))
        .unwrap();
    for i in 2..=40 {
        fixture.state.apply(&format!("key-{i}"), i);
    }
    let expected = handle.snapshot().await.unwrap();
    assert_eq!(expected.cursor().sequence(), 40);

    let mut reads = Vec::new();
    for transfer in [StateTransfer::Stream, file()] {
        for follow in [false, true] {
            let mut read = if follow {
                handle
                    .current_state_and_follow(options(transfer), follow_limits(8))
                    .await
                    .unwrap()
            } else {
                handle.current_state(options(transfer)).await.unwrap()
            };
            assert_eq!(read.cursor(), Some(expected.cursor()));
            assert_eq!(read.schema(), expected.schema());
            assert_eq!(read.dataset(), "inventory-dataset");
            let first = read.next_batch().await.unwrap().unwrap();
            assert_eq!(first.changes().operations().len(), 6);
            let mut rows = im::HashMap::new();
            check_batch(first, &mut rows);
            reads.push((read, rows, follow));
        }
    }
    // Every import is paused. The fixture commits these updates on the same
    // native follow hub that was armed under its snapshot lock.
    fixture.state.apply("a", 41);
    fixture.state.apply("key-42", 42);
    for (mut read, mut rows, follow) in reads {
        while let Some(batch) = read.next_batch().await.unwrap() {
            check_batch(batch, &mut rows);
        }
        assert_eq!(&rows, expected.records());
        if follow {
            let mut tail = read.into_follow().unwrap();
            for seq in [41, 42] {
                let event = bounded(tail.next()).await.unwrap();
                let progress = GraphProducerProgress::from_envelope(&event)
                    .unwrap()
                    .unwrap();
                assert_eq!(progress.sequence(), seq);
                assert_eq!(progress.identity(), expected.cursor().producer());
            }
        } else {
            assert!(read.next_batch().await.unwrap().is_none());
            assert!(read.into_follow().is_err());
        }
    }
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_is_not_absent_and_snapshot_only_never_opens_a_follow() {
    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    for (name, snapshot) in [("snapshot-only", true), ("plain", false)] {
        let state = Shared::new(name, 1, 8);
        add_source(
            &core,
            fixture_source(
                name,
                &state,
                Registration {
                    snapshot,
                    replay: false,
                },
                "out",
            ),
        )
        .await;
        let handle = core
            .output_catalogue()
            .unwrap()
            .resolve(&endpoint(name, "out"))
            .unwrap();
        for transfer in [StateTransfer::default(), file()] {
            let result = handle.current_state(options(transfer)).await;
            if snapshot {
                let mut read = result.unwrap();
                assert_eq!(read.cursor().map(OutputCursor::sequence), Some(0));
                assert!(read.next_batch().await.unwrap().is_none());
            } else {
                assert!(matches!(
                    result,
                    Err(OutputReadError::Absent {
                        capability: OutputCapability::Snapshot,
                        ..
                    })
                ));
            }
            assert!(matches!(
                handle
                    .current_state_and_follow(options(transfer), follow_limits(1))
                    .await,
                Err(OutputReadError::Absent {
                    capability: OutputCapability::SnapshotFollow,
                    ..
                })
            ));
        }
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_and_cancelled_transfers_release_the_lease_and_never_complete() {
    let fixture = started_fixture(1, 1).await;
    let handle = fixture
        .core
        .output_catalogue()
        .unwrap()
        .resolve(&endpoint("native", "out"))
        .unwrap();
    // Leave exactly one slot. Any leaked fourth lease blocks the next request.
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(handle.snapshot_and_follow(follow_limits(8)).await.unwrap());
    }
    let tiny = StateTransfer::TemporaryFile {
        max_file_bytes: NonZeroU64::new(64).unwrap(),
    };
    assert!(matches!(
        handle
            .current_state_and_follow(options(tiny), follow_limits(8))
            .await,
        Err(OutputReadError::Invalid(_))
    ));
    let bad_batch = CurrentStateOptions {
        batches: SnapshotBatchLimits {
            max_bytes: NonZeroUsize::MIN,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut read = handle
        .current_state_and_follow(bad_batch, follow_limits(8))
        .await
        .unwrap();
    assert!(matches!(
        read.next_batch().await,
        Err(OutputReadError::Invalid(_))
    ));
    assert!(
        read.next_batch().await.is_err(),
        "failure must not become successful EOF"
    );
    assert!(read.into_follow().is_err());
    let read = handle
        .current_state_and_follow(options(file()), follow_limits(8))
        .await
        .unwrap();
    assert!(
        read.into_follow().is_err(),
        "unconsumed file cannot be installed"
    );
    let read = handle
        .current_state_and_follow(options(file()), follow_limits(8))
        .await
        .unwrap();
    drop(read); // Abandon a sealed file before importing it.

    let mut read = handle
        .current_state_and_follow(options(file()), follow_limits(8))
        .await
        .unwrap();
    bounded(fixture.handle.stop()).await.unwrap();
    assert!(matches!(
        read.next_batch().await,
        Err(OutputReadError::Retired)
    ));
    assert!(read.into_follow().is_err());
    drop(held);
    fixture.core.shutdown().await.unwrap();
}
