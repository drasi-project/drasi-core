// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::collections::BTreeMap;

const HISTORY: &str = "
MATCH (n:Item)
RETURN n.value AS value,
  drasi.getVersionByTimestamp(n, 999) AS original,
  drasi.getVersionsByTimeRange(n, 1000, 2001) AS versions,
  drasi.getVersionsByTimeRange(n, 1500, 2001, true) AS including_previous";

fn archive_provider(path: &Path) -> Result<Arc<RocksDbComputationProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            true,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    )))
}

async fn snapshot(query: &NativeComponentProxy) -> Result<BTreeMap<Box<str>, VariableValue>> {
    let readers = query.output_readers();
    let snapshot = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.snapshot())
        .context("missing native snapshot")?
        .snapshot()
        .await?;
    assert_eq!(snapshot.records().len(), 1);
    let row = QueryChangeCodec::decode_row(
        snapshot
            .records()
            .values()
            .next()
            .context("missing archive result row")?,
    )?;
    Ok(row.values)
}

async fn history_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let make = |generation| {
        factory()?.create_component_with_processing(
            component_id()?,
            json!({"query": HISTORY}),
            scope(generation),
            archive_provider(directory.path())?,
        )
    };
    let versions: Vec<_> = (0..=3)
        .map(|value| item(value, value as u64 * 1000))
        .collect();
    let mut query = make(1)?;
    query.initialize().await?;
    query.start().await?;
    for (index, element) in versions.iter().enumerate() {
        let output = query
            .transform(input(
                if index == 0 {
                    SourceChange::Insert {
                        element: element.clone(),
                    }
                } else {
                    SourceChange::Replace {
                        element: element.clone(),
                    }
                },
                index as u64 + 1,
            )?)
            .await?;
        query.delivery_completed(&output).await?;
    }
    let expected = BTreeMap::from([
        ("value".into(), VariableValue::from(json!(3))),
        ("original".into(), versions[0].to_expression_variable()),
        (
            "versions".into(),
            VariableValue::List(vec![
                versions[1].to_expression_variable(),
                versions[2].to_expression_variable(),
            ]),
        ),
        (
            "including_previous".into(),
            VariableValue::List(vec![
                versions[1].to_expression_variable(),
                versions[2].to_expression_variable(),
            ]),
        ),
    ]);
    assert_eq!(snapshot(&query).await?, expected);
    query.stop().await?;
    drop(query);

    let mut query = make(2)?;
    query.initialize().await?;
    query.start().await?;
    assert_eq!(snapshot(&query).await?, expected);
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(4, 4000),
            },
            5,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let mut expected = expected;
    expected.insert("value".into(), VariableValue::from(json!(4)));
    assert_eq!(snapshot(&query).await?, expected);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_archives_point_and_range_reopen_current_thread() -> Result<()> {
    history_reopen().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_archives_point_and_range_reopen_multi_thread() -> Result<()> {
    history_reopen().await
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_archives_require_actual_provider_support() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), HISTORY, 1)?;
    query.initialize().await?;
    query.start().await?;
    let error = query
        .transform(input(
            SourceChange::Insert {
                element: item(0, 1000),
            },
            1,
        )?)
        .await
        .expect_err("an archive-disabled provider must not produce empty history");
    let detail = format!("{error:#}").to_ascii_lowercase();
    assert!(detail.contains("archive"), "{error:#}");
    query.stop().await?;
    Ok(())
}
