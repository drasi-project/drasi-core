// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::*;
use bytes::Bytes;
use std::num::NonZeroUsize;

/// A non-graph schema. A descriptor-only or graph-change decoder won't do.
struct Row;

impl RecordValidator for Row {
    fn validate_identity(
        &self,
        _: &SchemaDescriptor,
        id: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        if id.namespace() == "test" && id.value().len() == 1 {
            Ok(())
        } else {
            Err(RecordValidationError::new("key", "expected one byte"))
        }
    }

    fn validate(
        &self,
        _: &SchemaDescriptor,
        id: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> std::result::Result<(), RecordValidationError> {
        if image == RecordImage::Full && payload.first() == id.value().first() {
            Ok(())
        } else {
            Err(RecordValidationError::new(
                "image",
                "expected the embedded key",
            ))
        }
    }
}

fn view(count: u8) -> (OutputSnapshot, Arc<Schema>) {
    let schema = Arc::new(Schema::new(
        SchemaDescriptor::try_new(
            SchemaId::try_new("test.rows").unwrap(),
            SchemaVersion::try_new(1).unwrap(),
            "bytes",
            Bytes::from_static(b"key followed by value"),
        )
        .unwrap(),
        Arc::new(Row),
    ));
    let producer = GraphProducerIdentity::volatile(
        "host".into(),
        "graph".into(),
        crate::test_helpers::component_id(1),
        StreamId::try_new("live").unwrap(),
    )
    .unwrap();
    let records = (0..count)
        .map(|i| {
            let id = RecordId::try_new("test", Bytes::from(vec![i])).unwrap();
            let record =
                Record::try_new(&schema, id.clone(), RecordImage::Full, vec![i; 64].into())
                    .unwrap();
            (id, record)
        })
        .collect();
    let snapshot = OutputSnapshot::try_new(
        OutputCursor::try_new(producer, 40).unwrap(),
        "dataset",
        schema.descriptor().clone(),
        records,
    )
    .unwrap();
    (snapshot, schema)
}

fn batches(snapshot: &OutputSnapshot, limits: SnapshotBatchLimits) -> SnapshotBatches {
    SnapshotBatches::new(
        snapshot,
        StreamId::try_new("baseline/test").unwrap(),
        limits,
    )
}

async fn sealed(
    snapshot: &OutputSnapshot,
    schema: Arc<Schema>,
    limits: SnapshotBatchLimits,
) -> FileTransfer {
    FileTransfer::seal(
        batches(snapshot, limits),
        [1; 32],
        schema,
        limits,
        NonZeroU64::new(1 << 20).unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn file_import_preserves_the_exact_ordered_envelopes_of_the_captured_view() {
    let (snapshot, schema) = view(23);
    let limits = SnapshotBatchLimits {
        max_records: NonZeroUsize::new(5).unwrap(),
        ..Default::default()
    };
    let mut file = sealed(&snapshot, schema.clone(), limits)
        .await
        .import([1; 32]);
    let mut codec = BinaryEnvelopeCodec::new(limits.max_bytes);
    codec.register_schema(schema).unwrap();
    let mut actual = im::HashMap::new();
    let mut sizes = Vec::new();
    for (i, expected) in batches(&snapshot, limits).enumerate() {
        let expected = expected.unwrap();
        let got = file.next().await.unwrap().unwrap();
        assert_eq!(
            codec.encode(&expected).unwrap(),
            codec.encode(&got).unwrap()
        );
        assert_eq!(got.system().sequence(), (i + 1) as u64);
        assert_ne!(got.system().stream(), snapshot.cursor().producer().stream());
        assert!(GraphProducerProgress::from_envelope(&got)
            .unwrap()
            .is_none());
        assert_eq!(got.changes().schema(), snapshot.schema());
        sizes.push(got.changes().operations().len());
        for (n, operation) in got.changes().operations().iter().enumerate() {
            let ChangeOperation::Added { ordinal, after } = operation else {
                panic!("not an addition")
            };
            assert_eq!(*ordinal, n as u64);
            assert!(actual
                .insert(after.identity().clone(), after.clone())
                .is_none());
        }
    }
    assert_eq!(sizes, [5, 5, 5, 5, 3]);
    assert_eq!(&actual, snapshot.records());
    assert!(file.next().await.is_none());
}

#[test]
fn byte_limit_splits_batches_and_rejects_an_indivisible_record() {
    let (snapshot, _) = view(20);
    let mut limits = SnapshotBatchLimits {
        max_records: NonZeroUsize::new(4).unwrap(),
        ..Default::default()
    };
    let four = batches(&snapshot, limits).next().unwrap().unwrap();
    limits.max_bytes =
        NonZeroUsize::new(BinaryEnvelopeCodec::encoded_size(&four).unwrap() - 1).unwrap();
    let split: Vec<_> = batches(&snapshot, limits)
        .map(|batch| batch.unwrap())
        .collect();
    assert_eq!(
        split
            .iter()
            .map(|e| e.changes().operations().len())
            .sum::<usize>(),
        20
    );
    for envelope in split {
        assert!(envelope.changes().operations().len() < 4);
        assert!(BinaryEnvelopeCodec::encoded_size(&envelope).unwrap() <= limits.max_bytes.get());
    }
    limits.max_records = NonZeroUsize::MIN;
    let one = batches(&snapshot, limits).next().unwrap().unwrap();
    let size = BinaryEnvelopeCodec::encoded_size(&one).unwrap();
    limits.max_bytes = NonZeroUsize::new(size).unwrap();
    assert!(batches(&snapshot, limits).next().unwrap().is_ok());
    limits.max_bytes = NonZeroUsize::new(size - 1).unwrap();
    let mut rejected = batches(&snapshot, limits);
    assert!(matches!(
        rejected.next(),
        Some(Err(OutputReadError::Invalid(_)))
    ));
    assert!(rejected.next().is_none());
}

#[tokio::test]
async fn corrupt_or_foreign_files_fail_before_the_first_batch() {
    let (snapshot, schema) = view(4);
    let limits = SnapshotBatchLimits {
        max_records: NonZeroUsize::MIN,
        ..Default::default()
    };
    for fault in ["truncated", "changed last frame", "wrong session"] {
        let mut file = sealed(&snapshot, schema.clone(), limits).await;
        let mut expected = [1; 32];
        match fault {
            "truncated" => file.file.get_ref().set_len(file.bytes - 1).await.unwrap(),
            "changed last frame" => {
                let f = file.file.get_mut();
                f.seek(std::io::SeekFrom::End(-1)).await.unwrap();
                let byte = f.read_u8().await.unwrap();
                f.seek(std::io::SeekFrom::End(-1)).await.unwrap();
                f.write_u8(!byte).await.unwrap();
                f.flush().await.unwrap();
            }
            _ => expected = [2; 32],
        }
        assert!(
            matches!(
                file.import(expected).next().await,
                Some(Err(OutputReadError::Invalid(_)))
            ),
            "{fault}"
        );
    }
    let (empty, schema) = view(0);
    assert!(matches!(
        sealed(&empty, schema, limits)
            .await
            .import([2; 32])
            .next()
            .await,
        Some(Err(_))
    ));
}

#[tokio::test]
async fn dropping_a_partial_write_stops_the_scan_and_never_publishes_a_file() {
    let (snapshot, schema) = view(4);
    let limits = SnapshotBatchLimits {
        max_records: NonZeroUsize::MIN,
        ..Default::default()
    };
    let quota = NonZeroU64::new(1 << 20).unwrap();
    let sealed = sealed(&snapshot, schema.clone(), limits).await;
    // There is no named spool to recover, even if the process exits before Drop.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(sealed.file.get_ref().metadata().await.unwrap().nlink(), 0);
    }
    let insufficient = NonZeroU64::new(sealed.bytes - 1).unwrap();
    drop(sealed);
    assert!(matches!(
        FileTransfer::seal(
            batches(&snapshot, limits),
            [1; 32],
            schema.clone(),
            limits,
            insufficient
        )
        .await,
        Err(OutputReadError::Invalid(_))
    ));

    let started = tokio::sync::Notify::new();
    let guard = Arc::new(());
    let scan = batches(&snapshot, limits).inspect({
        let guard = guard.clone();
        let started = &started;
        move |_| {
            assert!(Arc::strong_count(&guard) > 1);
            started.notify_one();
        }
    });
    tokio::select! {
        biased;
        () = started.notified() => (),
        _ = FileTransfer::seal(scan, [1; 32], schema, limits, quota) => panic!("write already completed"),
    }
    assert_eq!(
        Arc::strong_count(&guard),
        1,
        "no detached scan owns the snapshot"
    );
}
