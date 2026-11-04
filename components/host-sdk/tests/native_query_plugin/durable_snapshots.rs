// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationTransaction},
    interface::RowMutation,
};
use std::collections::BTreeMap;

const TEXT: &str = "MATCH (n:Item) RETURN n.id AS id, n.value AS value";
const SNAPSHOT_ROOT: &str = "DRASI_QUERY_PLUGIN_SNAPSHOT_ROOT";
const ROWS: u64 = 260;

fn node(id: u64, value: serde_json::Value, time: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", &id.to_string()),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(json!({"id":id, "value":value})),
    }
}

fn reader(query: &NativeComponentProxy) -> Result<Arc<dyn SnapshotProvider>> {
    query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("indexed query snapshot reader missing")
}

fn rows(snapshot: &OutputSnapshot) -> Result<BTreeMap<u64, (RecordId, i64)>> {
    let mut result = BTreeMap::new();
    for record in snapshot.records().values() {
        let decoded = QueryChangeCodec::decode_row(record)?;
        let values = QueryChangeCodec::row_values_to_json(&decoded.values);
        let id = values["id"].as_u64().context("missing result id")?;
        let value = values["value"].as_i64().context("missing result value")?;
        anyhow::ensure!(
            result
                .insert(id, (record.identity().clone(), value))
                .is_none(),
            "duplicate result id"
        );
    }
    Ok(result)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit fixture launched by the snapshot cases"]
async fn indexed_snapshot_exit_child() -> Result<()> {
    let path =
        PathBuf::from(std::env::var_os(SNAPSHOT_ROOT).context("snapshot child needs a directory")?);
    let mut query = create(&path, TEXT, 1)?;
    query.start().await?;
    let changes: Vec<_> = (0..ROWS)
        .map(|id| SourceChange::Insert {
            element: node(id, json!(id), 1),
        })
        .collect();
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_changes(
                &changes,
                StreamId::try_new("source/out")?,
                1,
                None,
            )?,
        })
        .await?;
    query.delivery_completed(&output).await?;
    assert_eq!(
        rows(&reader(&query)?.snapshot().await?)?.len(),
        usize::try_from(ROWS)?
    );
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: node(1, json!(1001), 2),
            },
            2,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let output = query
        .transform(input(
            SourceChange::Delete {
                metadata: node(2, json!(2), 3).get_metadata().clone(),
            },
            3,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let snapshot = reader(&query)?.snapshot().await?;
    assert_eq!(snapshot.cursor().sequence(), 3);
    let actual = rows(&snapshot)?;
    assert_eq!(actual.len(), usize::try_from(ROWS - 1)?);
    assert_eq!(actual[&1].1, 1001);
    assert!(!actual.contains_key(&2));
    std::process::exit(87)
}

async fn persistent_snapshots() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let child = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::snapshots::indexed_snapshot_exit_child",
                "--ignored",
                "--nocapture",
            ])
            .env(SNAPSHOT_ROOT, directory.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("snapshot child did not reach its committed projection")??;
    anyhow::ensure!(
        child.status.code() == Some(87),
        "snapshot child failed ({}):\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let mut query = create(directory.path(), TEXT, 2)?;
    assert!(reader(&query)?.snapshot().await.is_err());
    query.start().await?;
    let initial = reader(&query)?.snapshot().await?;
    assert!(initial.cursor().producer().persistent());
    assert_eq!(initial.cursor().sequence(), 3);
    let initial_rows = rows(&initial)?;
    assert_eq!(initial_rows.len(), usize::try_from(ROWS - 1)?);
    for id in 0..ROWS {
        if id == 2 {
            assert!(!initial_rows.contains_key(&id));
        } else {
            assert_eq!(
                initial_rows[&id].1,
                if id == 1 { 1001 } else { i64::try_from(id)? }
            );
        }
    }
    let replayed = query.continue_transform().await?;
    assert_eq!(replayed.len(), 1);
    assert_eq!(QueryChangeCodec::query_sequence(&replayed[0].envelope)?, 3);
    assert_eq!(replayed[0].envelope.system().sequence(), 4);
    let progress =
        GraphProducerProgress::from_envelope(&replayed[0].envelope)?.context("replay producer")?;
    assert_eq!(progress.identity(), initial.cursor().producer());
    let replay_snapshot = reader(&query)?.snapshot().await?;
    assert_eq!(replay_snapshot.cursor(), initial.cursor());
    assert_eq!(
        rows(&replay_snapshot)?,
        initial_rows,
        "replay must not apply committed mutations twice"
    );
    query.delivery_completed(&replayed).await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: node(ROWS, json!(ROWS), 4),
            },
            4,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let current = reader(&query)?.snapshot().await?;
    assert_eq!(current.cursor().sequence(), 4);
    assert_eq!(output[0].envelope.system().sequence(), 5);
    let current_rows = rows(&current)?;
    assert_eq!(current_rows.len(), usize::try_from(ROWS)?);
    for (id, value) in initial_rows {
        assert_eq!(current_rows[&id], value);
    }
    let old_reader = reader(&query)?;
    query.stop().await?;
    assert!(old_reader.snapshot().await.is_err());
    drop(query);
    let mut query = create(directory.path(), TEXT, 3)?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let reopened = reader(&query)?.snapshot().await?;
    assert_eq!(reopened.cursor(), current.cursor());
    assert_eq!(rows(&reopened)?, current_rows);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_snapshots_survive_process_exit_and_reopen_current_thread() -> Result<()> {
    persistent_snapshots().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_snapshots_survive_process_exit_and_reopen_multi_thread() -> Result<()> {
    persistent_snapshots().await
}

#[tokio::test(flavor = "current_thread")]
async fn missing_or_corrupt_live_rows_cannot_become_an_empty_snapshot() -> Result<()> {
    for missing in [true, false] {
        let directory = tempfile::tempdir()?;
        let mut query = create(directory.path(), TEXT, 1)?;
        query.start().await?;
        let output = query
            .transform(input(
                SourceChange::Insert {
                    element: node(1, json!(10), 1),
                },
                1,
            )?)
            .await?;
        query.delivery_completed(&output).await?;
        let snapshot = reader(&query)?.snapshot().await?;
        let record = snapshot
            .records()
            .values()
            .next()
            .context("persisted row")?;
        let signature = QueryChangeCodec::decode_row(record)?
            .signature
            .context("stored signature")?;
        query.stop().await?;
        drop(query);
        let owner = ComputationTransaction::try_new(
            provider(directory.path())?
                .create_indexes(GRAPH, COMPONENT)
                .await?,
        )?;
        owner
            .run(async {
                owner
                    .resources()
                    .live_results_writer()
                    .expect("atomic live results")
                    .apply_mutations(
                        COMPONENT,
                        &[RowMutation {
                            row_signature: signature,
                            data: if missing {
                                None
                            } else {
                                Some(b"invalid graph row")
                            },
                        }],
                    )
                    .await?;
                Ok(())
            })
            .await?;
        owner.shutdown().await?;
        drop(owner);
        let mut query = create(directory.path(), TEXT, 2)?;
        assert!(
            query.start().await.is_err(),
            "damaged projection must not be published"
        );
        assert!(reader(&query)?.snapshot().await.is_err());
        query.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_mutation_hides_snapshot_until_committed_rows_are_restored() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let text = "MATCH (n:Item) RETURN n.id AS id, abs(n.value) AS value";
    let mut query = create(directory.path(), text, 1)?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: node(1, json!(-10), 1),
            },
            1,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let initial = reader(&query)?.snapshot().await?;
    assert!(query
        .transform(input(
            SourceChange::Replace {
                element: node(1, json!("invalid"), 2)
            },
            2
        )?)
        .await
        .is_err());
    assert!(reader(&query)?.snapshot().await.is_err());
    query.stop().await?;
    drop(query);
    let mut query = create(directory.path(), text, 2)?;
    query.start().await?;
    let restored = reader(&query)?.snapshot().await?;
    assert_eq!(restored.cursor(), initial.cursor());
    assert_eq!(rows(&restored)?, rows(&initial)?);
    query.stop().await?;
    Ok(())
}
