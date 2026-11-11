// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::super::tests::{producer, reader};
use super::*;
use drasi_computation_plugin_sdk::output_reads::WireRecord;
use std::borrow::Cow;

fn fixture(count: u64) -> (Reader, Content) {
    let (reader, _) = reader(Vec::new(), OutputCapability::Snapshot);
    let content = Content::new(
        &reader.context,
        WireReadMetadata::Snapshot(WireSnapshotMetadata {
            cursor: WireCursor {
                producer: producer("component"),
                sequence: 0,
            },
            dataset: "dataset".into(),
            schema: reader.schema.descriptor().clone(),
            records: count,
        }),
        ext::operation::SNAPSHOT,
        None,
    )
    .unwrap();
    (reader, content)
}

fn record(id: &str) -> Record {
    use drasi_computation_contracts::ElementImage;
    use drasi_core::models::{ElementMetadata, ElementReference};
    GraphChangeCodec::element_record(&ElementImage::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from(Vec::<Arc<str>>::new()),
            effective_from: 1,
        },
        properties: Default::default(),
    })
    .unwrap()
}

fn page(index: u64, done: bool, records: &[Record]) -> WireReadPage<'_> {
    WireReadPage {
        index,
        done,
        records: records
            .iter()
            .map(|record| WireRecord {
                namespace: Cow::Borrowed(record.identity().namespace()),
                identity: Cow::Borrowed(record.identity().value()),
                image: 0,
                payload: Cow::Borrowed(record.payload()),
            })
            .collect(),
        envelopes: Vec::new(),
    }
}

#[test]
fn pages_reject_skips_truncation_empty_progress_overflow_and_cross_page_duplicates() {
    let records = [record("first"), record("second"), record("third")];
    for invalid_page in [
        page(2, false, &records[..1]),
        page(1, true, &records[..1]),
        page(1, false, &[]),
        page(1, true, &records),
        page(1, false, &records[..2]),
    ] {
        let (reader, mut content) = fixture(2);
        assert!(matches!(
            content.append(&reader.context, invalid_page),
            Err(OutputReadError::Invalid(_))
        ));
    }
    let (reader, mut content) = fixture(2);
    assert!(!content
        .append(&reader.context, page(1, false, &records[..1]))
        .unwrap());
    assert!(matches!(
        content.append(&reader.context, page(2, true, &records[..1])),
        Err(OutputReadError::Invalid(_)),
    ));
    let (reader, mut content) = fixture(2);
    assert!(!content
        .append(&reader.context, page(1, false, &records[..1]))
        .unwrap());
    assert!(content
        .append(&reader.context, page(2, true, &records[1..2]))
        .unwrap());
    let Read::Snapshot(snapshot) = content.finish().unwrap() else {
        panic!("wrong read kind");
    };
    assert_eq!(snapshot.records().len(), 2);
    assert_eq!(
        snapshot.records().get(records[0].identity()),
        Some(&records[0])
    );
    assert_eq!(
        snapshot.records().get(records[1].identity()),
        Some(&records[1])
    );
    let too_many = vec![records[0].clone(); ext::MAX_PAGE_ITEMS + 1];
    let (reader, mut content) = fixture(too_many.len() as u64);
    assert!(matches!(
        content.append(&reader.context, page(1, true, &too_many)),
        Err(OutputReadError::Invalid(_)),
    ));
}

#[test]
fn transfer_metadata_rejects_mixed_observations_and_wrong_operations() {
    let (reader, _) = fixture(0);
    let cursor = WireCursor {
        producer: producer("component"),
        sequence: 0,
    };
    for code in [ext::operation::QUERY_VIEW, ext::operation::SNAPSHOT] {
        let metadata = WireReadMetadata::QueryView {
            snapshot: WireSnapshotMetadata {
                cursor: cursor.clone(),
                dataset: "dataset".into(),
                schema: reader.schema.descriptor().clone(),
                records: 0,
            },
            retained: WireReplayMetadata {
                after: cursor.clone(),
                head: WireCursor {
                    sequence: u64::from(code == ext::operation::QUERY_VIEW),
                    ..cursor.clone()
                },
                envelopes: 0,
            },
            generation: 0,
            config_hash: 0,
        };
        assert!(matches!(
            Content::new(&reader.context, metadata, code, None),
            Err(OutputReadError::Invalid(_)),
        ));
    }
}
