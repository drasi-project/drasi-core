// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Paging, cursor, grouping and retention against the in-memory session.

use super::*;
use crate::native::{
    memory::{lsn, MemoryDatabase},
    session::CdcConnector,
    StartPosition,
};
use chrono::{FixedOffset, TimeZone};
use drasi_computation_contracts::{source_failure, SourceFailure};
use drasi_core_types::models::{Element, ElementValue, SourceChange};

fn settings(rows: usize) -> NativeSettings {
    let mut settings = NativeSettings::new("app");
    settings.max_rows = rows;
    settings
}

async fn open(db: &MemoryDatabase, settings: &NativeSettings) -> (Box<dyn CdcSession>, Poller) {
    let mut session = db.connect().await.expect("connect");
    let poller = Poller::open(session.as_mut(), settings, "source")
        .await
        .expect("open");
    (session, poller)
}

fn id(change: &Change) -> (String, &'static str) {
    let (kind, element) = match &change.change {
        SourceChange::Insert { element } => ("insert", Some(element)),
        SourceChange::Update { element, .. } | SourceChange::Replace { element } => {
            ("update", Some(element))
        }
        SourceChange::Delete { metadata } => {
            return (metadata.reference.element_id.to_string(), "delete")
        }
        _ => ("other", None),
    };
    (
        element
            .expect("element")
            .get_metadata()
            .reference
            .element_id
            .to_string(),
        kind,
    )
}

fn failure(error: &anyhow::Error) -> Option<&SourceFailure> {
    source_failure(error)
}

/// Polls until the boundary reaches the maximum LSN, returning every change.
async fn drain(
    session: &mut dyn CdcSession,
    poller: &mut Poller,
    settings: &NativeSettings,
    mut cursor: Option<Cursor>,
) -> (Vec<Change>, Cursor, usize) {
    let mut changes = Vec::new();
    let mut pages = 0;
    loop {
        let page = poller
            .poll(session, settings, cursor.as_ref())
            .await
            .expect("poll");
        pages += 1;
        assert!(pages < 10_000, "paging made no progress");
        if let Some(previous) = &cursor {
            assert_ne!(
                page.boundary.compare(previous).unwrap(),
                Ordering::Less,
                "boundaries never regress"
            );
        }
        let done = page.boundary.row.is_none() && page.changes.is_empty();
        changes.extend(page.changes);
        cursor = Some(page.boundary);
        if done {
            return (changes, cursor.unwrap(), pages);
        }
    }
}

#[tokio::test]
async fn one_row_pages_drain_an_interleaved_backlog_in_global_order() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.add_table("Orders", 9);
    for n in 1..=200u64 {
        let object = if n % 3 == 0 { 9 } else { 7 };
        db.change(object, 10 + n / 2, n, 2, n as i64, 0);
    }
    let settings = settings(1);
    let (mut session, mut poller) = open(&db, &settings).await;
    let start = Cursor::boundary(poller.catalog.binding, lsn(1));
    let (changes, end, pages) = drain(session.as_mut(), &mut poller, &settings, Some(start)).await;
    assert_eq!(changes.len(), 200);
    assert!(changes
        .windows(2)
        .all(|pair| pair[0].position.compare(&pair[1].position).unwrap() == Ordering::Less));
    assert_eq!(end.lsn, db.with(|state| state.max_lsn));
    assert!(pages >= 200);
    let (keys, ranges) = db.with(|state| (state.key_queries, state.range_queries));
    assert_eq!(keys, pages, "one key query per page");
    assert!(
        ranges <= 2 * pages,
        "at most one range query per table per page, never per group"
    );
}

#[tokio::test]
async fn page_size_is_governed_by_max_rows_and_bytes() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    for n in 1..=50u64 {
        db.change(7, 10, n, 2, n as i64, 0);
    }
    let settings = settings(20);
    let (mut session, mut poller) = open(&db, &settings).await;
    let start = Cursor::boundary(poller.catalog.binding, lsn(1));
    let page = poller
        .poll(session.as_mut(), &settings, Some(&start))
        .await
        .unwrap();
    assert_eq!(page.changes.len(), 20);
    assert!(
        page.boundary.row.is_some(),
        "a cut page ends at a row position"
    );
    let size = page.changes[0].size().unwrap();
    let mut small = settings.clone();
    small.max_bytes = size * 3;
    let page = poller
        .poll(session.as_mut(), &small, Some(&start))
        .await
        .unwrap();
    assert_eq!(page.changes.len(), 3);
    let mut tiny = settings.clone();
    tiny.max_bytes = size - 1;
    assert!(poller
        .poll(session.as_mut(), &tiny, Some(&start))
        .await
        .is_err());
}

#[tokio::test]
async fn a_rekey_pair_split_across_pages_resumes_without_loss_or_duplicates() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.change(7, 10, 1, 2, 1, 10);
    db.change(7, 11, 1, 3, 1, 10);
    db.change(7, 11, 1, 4, 2, 10);
    db.change(7, 12, 1, 3, 2, 10);
    db.change(7, 12, 1, 4, 2, 20);
    db.change(7, 13, 1, 1, 2, 20);
    let settings = settings(1);
    let (mut session, mut poller) = open(&db, &settings).await;
    let start = Cursor::boundary(poller.catalog.binding, lsn(1));
    let (changes, ..) = drain(session.as_mut(), &mut poller, &settings, Some(start)).await;
    let summary: Vec<_> = changes.iter().map(id).collect();
    assert_eq!(
        summary,
        vec![
            ("dbo.Items:1".to_string(), "insert"),
            ("dbo.Items:1".to_string(), "delete"),
            ("dbo.Items:2".to_string(), "insert"),
            ("dbo.Items:2".to_string(), "update"),
            ("dbo.Items:2".to_string(), "delete"),
        ]
    );
    assert_eq!(
        changes[2].position.row.unwrap().2,
        1,
        "second image ordinal"
    );
    assert!(changes[3].before.is_some(), "updates keep the before image");
}

#[tokio::test]
async fn beginning_starts_at_each_retained_minimum_and_current_at_the_maximum() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.change(7, 10, 1, 2, 1, 1);
    db.change(7, 11, 1, 2, 2, 1);
    let mut settings = settings(100);
    let (mut session, mut poller) = open(&db, &settings).await;
    assert_eq!(
        poller
            .origin(session.as_mut(), &settings)
            .await
            .unwrap()
            .unwrap()
            .lsn,
        lsn(11)
    );
    settings.start_position = StartPosition::Beginning;
    assert!(poller
        .origin(session.as_mut(), &settings)
        .await
        .unwrap()
        .is_none());
    let page = poller
        .poll(session.as_mut(), &settings, None)
        .await
        .unwrap();
    assert_eq!(page.changes.len(), 2);
    assert_eq!(
        page.boundary,
        Cursor::boundary(poller.catalog.binding, lsn(11))
    );
}

#[tokio::test]
async fn retention_gaps_and_cursors_ahead_of_cdc_are_typed_position_failures() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.change(7, 20, 1, 2, 1, 1);
    let settings = settings(10);
    let (mut session, mut poller) = open(&db, &settings).await;
    let old = Cursor::boundary(poller.catalog.binding, lsn(5));
    db.cleanup(10);
    let error = poller
        .poll(session.as_mut(), &settings, Some(&old))
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::PositionUnavailable(_))
    ));
    let ahead = Cursor::boundary(poller.catalog.binding, lsn(99));
    let error = poller
        .poll(session.as_mut(), &settings, Some(&ahead))
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::PositionUnavailable(_))
    ));
    let foreign = Cursor::boundary([9; 32], lsn(15));
    let error = poller
        .poll(session.as_mut(), &settings, Some(&foreign))
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::SchemaChanged(_))
    ));
}

#[tokio::test]
async fn capture_instance_change_on_a_selected_table_is_schema_changed_and_others_are_ignored() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.add_table("Other", 9);
    db.change(7, 10, 1, 2, 1, 1);
    let mut settings = settings(10);
    settings.tables = vec!["Items".into()];
    let (mut session, mut poller) = open(&db, &settings).await;
    let cursor = Cursor::boundary(poller.catalog.binding, lsn(1));
    // An unselected table changes its capture instance: ignored.
    db.with(|state| {
        state.tables[1].capture.capture = "dbo_Other_v2".into();
        state.fingerprint += 1;
    });
    poller
        .poll(session.as_mut(), &settings, Some(&cursor))
        .await
        .unwrap();
    // A non-key column added to the selected table: adopted.
    db.with(|state| {
        state.tables[0]
            .facts
            .columns
            .push(super::super::session::RawColumn {
                name: "note".into(),
                type_name: "nvarchar".into(),
                precision: 0,
                scale: 0,
            });
        state.fingerprint += 1;
    });
    poller
        .poll(session.as_mut(), &settings, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(poller.catalog.tables[0].columns.len(), 3);
    // The selected table is recaptured: typed SchemaChanged.
    db.with(|state| {
        state.tables[0].capture.capture = "dbo_Items_v2".into();
        state.fingerprint += 1;
    });
    let error = poller
        .poll(session.as_mut(), &settings, Some(&cursor))
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::SchemaChanged(_))
    ));
    // A key change is also bound.
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    let (mut session, mut poller) = open(&db, &settings).await;
    db.with(|state| {
        state.tables[0].facts.primary_key = vec!["value".into()];
        state.fingerprint += 1;
    });
    let error = poller
        .poll(
            session.as_mut(),
            &settings,
            Some(&Cursor::boundary(poller.catalog.binding, lsn(1))),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::SchemaChanged(_))
    ));
}

#[tokio::test]
async fn split_update_shapes_that_are_not_decodable_fail_typed_without_output() {
    // MS-15 as a unit test: three images in one group, or an unpaired
    // after-image, must not be guessed at.
    for operations in [vec![3, 4, 4], vec![4], vec![3]] {
        let db = MemoryDatabase::new();
        db.add_table("Items", 7);
        for operation in &operations {
            db.change(7, 10, 1, *operation, 1, 1);
        }
        let settings = settings(10);
        let (mut session, mut poller) = open(&db, &settings).await;
        let start = Cursor::boundary(poller.catalog.binding, lsn(1));
        let error = poller
            .poll(session.as_mut(), &settings, Some(&start))
            .await
            .unwrap_err();
        assert!(
            matches!(failure(&error), Some(SourceFailure::Unsupported(_))),
            "{operations:?}: {error:#}"
        );
    }
}

#[tokio::test]
async fn commit_times_convert_with_the_offset_in_force_at_commit_across_dst() {
    // US Eastern: 01:59:00 EST (-05:00) and 03:00:00 EDT (-04:00) on
    // 2026-03-08 are one minute apart in UTC. The server-side
    // `AT TIME ZONE` conversion supplies the historical offset; the engine
    // must use it rather than today's offset.
    let est = FixedOffset::west_opt(5 * 3600).unwrap();
    let edt = FixedOffset::west_opt(4 * 3600).unwrap();
    let before = est.with_ymd_and_hms(2026, 3, 8, 1, 59, 0).single().unwrap();
    let after = edt.with_ymd_and_hms(2026, 3, 8, 3, 0, 0).single().unwrap();
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.change_at(7, 10, 1, 2, 1, 1, before);
    db.change_at(7, 11, 1, 2, 2, 1, after);
    let settings = settings(10);
    let (mut session, mut poller) = open(&db, &settings).await;
    let start = Cursor::boundary(poller.catalog.binding, lsn(1));
    let page = poller
        .poll(session.as_mut(), &settings, Some(&start))
        .await
        .unwrap();
    let times: Vec<u64> = page
        .changes
        .iter()
        .map(|change| match &change.change {
            SourceChange::Insert {
                element: Element::Node { metadata, .. },
            } => metadata.effective_from,
            _ => panic!("insert"),
        })
        .collect();
    assert_eq!(times[1] - times[0], 60_000);
    assert_eq!(times[0], 1_772_953_140_000, "2026-03-08T06:59:00Z");
}

#[tokio::test]
async fn missing_change_table_permission_fails_at_open_with_an_actionable_message() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.with(|state| state.selectable = false);
    let mut session = db.connect().await.unwrap();
    let error = Poller::open(session.as_mut(), &settings(10), "source")
        .await
        .unwrap_err();
    assert!(matches!(
        failure(&error),
        Some(SourceFailure::Unsupported(_))
    ));
    assert!(format!("{error:#}").contains("GRANT SELECT ON cdc.[dbo_Items_CT]"));
}

#[tokio::test]
async fn values_reach_the_element_unchanged() {
    let db = MemoryDatabase::new();
    db.add_table("Items", 7);
    db.change(7, 10, 1, 2, 5, 42);
    let settings = settings(10);
    let (mut session, mut poller) = open(&db, &settings).await;
    let start = Cursor::boundary(poller.catalog.binding, lsn(1));
    let page = poller
        .poll(session.as_mut(), &settings, Some(&start))
        .await
        .unwrap();
    let SourceChange::Insert {
        element: Element::Node {
            properties,
            metadata,
        },
    } = &page.changes[0].change
    else {
        panic!("insert");
    };
    assert_eq!(properties.get("value"), Some(&ElementValue::Integer(42)));
    assert_eq!(metadata.reference.element_id.as_ref(), "dbo.Items:5");
    assert_eq!(metadata.labels[0].as_ref(), "Items");
}
