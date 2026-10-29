// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use futures_util::StreamExt;

fn scope() -> drasi_computation_plugin_sdk::Scope {
    drasi_computation_plugin_sdk::Scope {
        instance_id: "mssql-coverage".into(),
        graph_id: ComponentBatch::GRAPH_ID.into(),
        generation: 1,
    }
}
fn snapshot_rows(changes: &[drasi_core::models::SourceChange]) -> Vec<(String, i64)> {
    use drasi_core::models::{ElementValue, SourceChange};
    let mut rows = changes
        .iter()
        .filter_map(|change| match change {
            SourceChange::Insert { element } => {
                let value = match element.get_property("value") {
                    ElementValue::Integer(value) => *value,
                    _ => return None,
                };
                Some((
                    element.get_metadata().reference.element_id.to_string(),
                    value,
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

pub(super) async fn least_privilege(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    ensure!(db.reader_sql("SET NOCOUNT ON; SELECT CONCAT(IS_MEMBER(N'db_owner'),N'/',IS_SRVROLEMEMBER(N'sysadmin'),N'/',HAS_PERMS_BY_NAME(N'{{schema_name}}.ReadOnlyItems',N'OBJECT',N'INSERT'),N'/',HAS_PERMS_BY_NAME(N'cdc',N'SCHEMA',N'SELECT'))")? == "0/0/0/0",
        "standalone reader is overprivileged; least-privilege proof would be invalid");
    ensure!(db.application_sql("SET NOCOUNT ON; SELECT CONCAT(IS_MEMBER(N'db_owner'),N'/',IS_SRVROLEMEMBER(N'sysadmin'),N'/',HAS_PERMS_BY_NAME(N'{{schema_name}}.Items',N'OBJECT',N'INSERT'),N'/',HAS_PERMS_BY_NAME(DB_NAME(),N'DATABASE',N'CREATE TABLE'))")? == "0/0/0/0",
        "normal native SQL Server credentials must not be administrator or data writer");
    ensure!(db.sql("SET NOCOUNT ON; SELECT is_tracked_by_cdc FROM sys.tables WHERE object_id=OBJECT_ID(N'{{schema_name}}.ReadOnlyItems')")? == "0","standalone fixture unexpectedly uses CDC");
    let native = plugins(dynamic)?;
    let factory = native[1]
        .bootstrap_factories()
        .iter()
        .find(|f| f.metadata().implementation.name.as_ref() == "drasi.mssql/snapshot")
        .context("read-only factory")?;
    for labels in [json!(["ReadOnlyItems"]), json!(["Unmatched"])] {
        let provider = factory.create_provider(id("snapshot"),
            json!({"source":"items","stream":stream("items"),"settings":db.read_only_settings(),"labels":labels}),
            Some(scope()),None)?;
        let proof = async {
            provider.prepare().await?;
            let mut snapshot = provider.snapshot().await?;
            ensure!(
                snapshot.watermarks.is_empty(),
                "standalone read-only snapshot fabricated a CDC watermark"
            );
            let mut changes = Vec::new();
            while let Some(envelope) = snapshot.changes.next().await {
                changes.extend(GraphChangeCodec::decode_changes(&envelope?)?);
            }
            provider.complete_snapshot().await?;
            if labels[0] == "ReadOnlyItems" {
                ensure!(
                    snapshot_rows(&changes)
                        == vec![
                            (format!("{}.ReadOnlyItems:1", db.schema), 10),
                            (format!("{}.ReadOnlyItems:2", db.schema), 20)
                        ],
                    "standalone read-only snapshot lost identities or values"
                );
            } else {
                ensure!(changes.is_empty(), "unmatched labels were not filtered");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = provider.stop().await;
        proof?;
        cleanup?;
    }
    let provider = factory.create_provider(
        id("snapshot"),
        json!({
            "source":"items","stream":stream("items"),"settings":db.read_only_settings(),
        }),
        Some(scope()),
        None,
    )?;
    provider.prepare().await?;
    let mut snapshot = provider.snapshot().await?;
    ensure!(
        snapshot.changes.next().await.transpose()?.is_some(),
        "expected first standalone row"
    );
    provider.stop().await?;
    ensure!(
        snapshot
            .changes
            .next()
            .await
            .context("retired snapshot silently became successful EOF")?
            .is_err(),
        "retired snapshot emitted a successful row"
    );
    db.remove()
}

const HOLD_MARKER: &str = "
SET NOCOUNT ON;
BEGIN TRANSACTION;
SELECT TOP(1) token FROM {{schema}}.DrasiBarrier WITH(TABLOCKX,HOLDLOCK);
DECLARE @until datetime2=DATEADD(second,30,SYSUTCDATETIME());
WHILE NOT EXISTS(SELECT 1 FROM {{schema}}.Signals WITH(READUNCOMMITTED) WHERE name=N'release-marker')
BEGIN
 IF SYSUTCDATETIME()>@until BEGIN ROLLBACK; THROW 51010,'owned marker lock deadline',1; END;
 WAITFOR DELAY '00:00:00.025';
END;
COMMIT;";
const MARKER_LOCKED: &str = "SET NOCOUNT ON; SELECT CASE WHEN EXISTS(SELECT 1 FROM sys.dm_tran_locks WHERE resource_database_id=DB_ID() AND resource_type=N'OBJECT' AND resource_associated_entity_id=OBJECT_ID(N'{{schema_name}}.DrasiBarrier') AND request_status=N'GRANT' AND request_mode=N'X') THEN 1 ELSE 0 END";
const SOURCE_LOCKED: &str = "SET NOCOUNT ON; SELECT CASE WHEN EXISTS(SELECT 1 FROM sys.dm_tran_locks WHERE resource_database_id=DB_ID() AND resource_type=N'OBJECT' AND resource_associated_entity_id=OBJECT_ID(N'{{schema_name}}.Items') AND request_status=N'GRANT' AND request_mode=N'S') THEN 1 ELSE 0 END";

pub(super) async fn concurrent_cut_and_cleanup(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let native = plugins(dynamic)?;
    let factory = native[1]
        .bootstrap_factories()
        .iter()
        .find(|f| f.metadata().implementation.name.as_ref() == "drasi.mssql/bootstrap")
        .context("coordinated factory")?;
    let owner = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("snapshot"),
    )?);
    let mut settings = db.settings();
    settings["connection"]["tables"] = json!([
        format!("{}.Items", db.schema),
        format!("{}.Companion", db.schema)
    ]);
    let provider = factory.create_provider(
        id("snapshot"),
        json!({"source":"items","stream":stream("items"),"settings":settings}),
        Some(scope()),
        Some(owner),
    )?;
    provider.prepare().await?;
    let mut holder = db.spawn_sql(HOLD_MARKER)?;
    db.wait_sql(MARKER_LOCKED, "1").await?;
    let proof = async {
        let mut snapshot=provider.snapshot().await?;
        ensure!(snapshot.watermarks.is_empty(),"snapshot published a cut before streaming cleanup");
        let mut pending = Box::pin(snapshot.changes.next());
        tokio::select! {
            _ = &mut pending => anyhow::bail!("snapshot emitted before held marker was released"),
            observed = db.wait_sql(SOURCE_LOCKED,"1") => observed?,
        }
        let mut writer = db.spawn_sql("BEGIN TRAN; UPDATE {{schema}}.Items SET value=999 WHERE id=1; ROLLBACK;
            BEGIN TRAN; UPDATE {{schema}}.Items SET value=110 WHERE id=1;
            UPDATE {{schema}}.Companion SET value=110 WHERE id=1;
            INSERT INTO {{schema}}.Items VALUES(3,30,N'concurrent');
            INSERT INTO {{schema}}.Companion VALUES(3,30);
            DELETE FROM {{schema}}.Items WHERE id=2; DELETE FROM {{schema}}.Companion WHERE id=2; COMMIT;")?;
        db.wait_sql("SET NOCOUNT ON; SELECT CASE WHEN EXISTS(SELECT 1 FROM sys.dm_tran_locks WHERE resource_database_id=DB_ID() AND resource_type=N'OBJECT' AND resource_associated_entity_id=OBJECT_ID(N'{{schema_name}}.Items') AND request_status=N'WAIT') THEN 1 ELSE 0 END","1").await?;
        db.sql("INSERT INTO {{schema}}.Signals VALUES(N'release-marker')")?;
        holder.finish().await?;
        let first=tokio::time::timeout(Duration::from_secs(25),pending).await?.context("first snapshot row")??;
        let mut changes=GraphChangeCodec::decode_changes(&first)?;
        while let Some(envelope) = snapshot.changes.next().await { changes.extend(GraphChangeCodec::decode_changes(&envelope?)?); }
        let watermarks=provider.complete_snapshot().await?;
        let watermark = watermarks.first().context("missing completed real CDC cut")?.position.as_ref().context("missing CDC cursor")?;
        let cursor: Value = serde_json::from_slice(watermark)?;
        let lsn = cursor["lsn"].as_array().context("CDC LSN missing")?.iter()
            .map(|byte| byte.as_u64().filter(|v|*v<256).map(|v|format!("{v:02X}")).context("invalid LSN byte"))
            .collect::<Result<String>>()?;
        ensure!(snapshot_rows(&changes)==vec![
            (format!("{}.Companion:1",db.schema),10),(format!("{}.Companion:2",db.schema),20),
            (format!("{}.Items:1",db.schema),10),(format!("{}.Items:2",db.schema),20),
        ],"snapshot did not retain the pre-writer multi-table state");
        writer.finish().await?;
        for table in ["Items","Companion"] {
            db.wait_sql(&format!("SET NOCOUNT ON; SELECT COUNT(*) FROM cdc.[{}_{}_CT] WHERE __$start_lsn>0x{lsn} AND __$operation IN(1,2,4)",db.schema,table),"3").await?;
        }
        ensure!(db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.DrasiBarrier")?=="0","coordinated snapshot leaked its marker");
        Ok::<_,anyhow::Error>(())
    }.await;
    // Always release our blocker before awaiting native cleanup, including failures.
    db.sql("IF NOT EXISTS(SELECT 1 FROM {{schema}}.Signals WHERE name=N'release-marker') INSERT INTO {{schema}}.Signals VALUES(N'release-marker')")?;
    let holder_cleanup = if holder.pending() {
        holder.finish().await.map(|_| ())
    } else {
        Ok(())
    };
    let cleanup = provider.stop().await;
    proof?;
    holder_cleanup?;
    cleanup?;

    let owner = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("limited"),
    )?);
    let mut limited_settings = db.settings();
    limited_settings["max_rows"] = json!(1);
    let limited = factory.create_provider(
        id("limited"),
        json!({
            "source":"items","stream":stream("items"),"settings":limited_settings,
        }),
        Some(scope()),
        Some(owner),
    )?;
    limited.prepare().await?;
    let mut snapshot = limited.snapshot().await?;
    let mut count = 0;
    while let Some(row) = snapshot.changes.next().await {
        row?;
        count += 1;
    }
    ensure!(
        count == 2,
        "one-row buffering became a total snapshot limit"
    );
    ensure!(
        limited.complete_snapshot().await?.len() == 1,
        "completed streaming snapshot lost its real boundary"
    );
    limited.stop().await?;
    ensure!(
        db.sql(SOURCE_LOCKED)? == "0",
        "streaming snapshot retained table locks after successful cleanup"
    );
    ensure!(
        db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.DrasiBarrier")? == "0",
        "streaming snapshot did not delete its owned barrier"
    );

    db.sql("DELETE FROM {{schema}}.Signals WHERE name=N'release-marker'")?;
    let owner = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("cancelled"),
    )?);
    let mut settings = db.settings();
    settings["shutdown_timeout_ms"] = json!(50);
    let cancelled = factory.create_provider(
        id("cancelled"),
        json!({
            "source":"items","stream":stream("items"),"settings":settings,
        }),
        Some(scope()),
        Some(owner),
    )?;
    cancelled.prepare().await?;
    let mut holder = db.spawn_sql(HOLD_MARKER)?;
    db.wait_sql(MARKER_LOCKED, "1").await?;
    let mut cancelled_stream = cancelled.snapshot().await?;
    {
        let mut pending = Box::pin(cancelled_stream.changes.next());
        tokio::select! {
            _ = &mut pending => anyhow::bail!("snapshot did not retain blocked work"),
            observed = db.wait_sql(SOURCE_LOCKED,"1") => observed?,
        }
        drop(cancelled_stream);
    }
    let failed_stop = tokio::time::timeout(Duration::from_secs(2), cancelled.stop()).await?;
    ensure!(
        failed_stop.is_err(),
        "cancelled blocked snapshot reported a successful stop"
    );
    ensure!(
        db.sql(SOURCE_LOCKED)? == "1",
        "test did not observe retained server-side snapshot locks"
    );
    db.sql("INSERT INTO {{schema}}.Signals VALUES(N'release-marker')")?;
    holder.finish().await?;
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if cancelled.stop().await.is_ok() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    ensure!(
        db.sql(SOURCE_LOCKED)? == "0",
        "successful stop left actual table locks"
    );
    ensure!(
        db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.DrasiBarrier")? == "0",
        "cancelled snapshot left a marker"
    );
    db.remove()
}

pub(super) async fn large_snapshots_and_tiny_cdc_pages(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    db.sql(
        "SET NOCOUNT ON; BEGIN TRAN; DECLARE @i int=3;
        WHILE @i<=200 BEGIN
          INSERT INTO {{schema}}.Items VALUES(@i,@i,NULL);
          INSERT INTO {{schema}}.Companion VALUES(@i,@i);
          INSERT INTO {{schema}}.ReadOnlyItems VALUES(@i,@i,NULL);
          SET @i=@i+1;
        END; COMMIT;",
    )?;
    let native = plugins(dynamic)?;
    for coordinated in [false, true] {
        let name = if coordinated {
            "drasi.mssql/bootstrap"
        } else {
            "drasi.mssql/snapshot"
        };
        let factory = native[1]
            .bootstrap_factories()
            .iter()
            .find(|f| f.metadata().implementation.name.as_ref() == name)
            .context("snapshot factory")?;
        let mut settings = if coordinated {
            db.settings()
        } else {
            db.read_only_settings()
        };
        settings["max_rows"] = json!(1);
        settings["max_bytes"] = json!(1024);
        let owner = coordinated
            .then(|| QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id("large")).map(Arc::new))
            .transpose()?;
        let provider = factory.create_provider(
            id("large"),
            json!({"source":"items","stream":stream("items"),"settings":settings}),
            Some(scope()),
            owner,
        )?;
        provider.prepare().await?;
        let mut snapshot = provider.snapshot().await?;
        ensure!(snapshot.watermarks.is_empty(), "premature snapshot cut");
        let mut count = 0;
        while let Some(envelope) = snapshot.changes.next().await {
            envelope?;
            count += 1;
        }
        ensure!(
            count == 200,
            "bounded snapshot lost rows from a larger dataset"
        );
        let cut = provider.complete_snapshot().await?;
        ensure!(
            cut.len() == usize::from(coordinated),
            "incorrect snapshot completion boundary"
        );
        provider.stop().await?;
    }
    let mut settings = db.settings();
    settings["max_rows"] = json!(1);
    settings["max_bytes"] = json!(1024);
    settings["connection"]["tables"] = json!([
        format!("{}.Items", db.schema),
        format!("{}.Companion", db.schema)
    ]);
    let harness = Harness::new(
        dynamic,
        settings,
        Arc::new(InMemoryComputationProvider),
        true,
    )
    .await?;
    let proof = async {
        harness.start().await?;
        db.sql(
            "SET NOCOUNT ON; BEGIN TRAN; DECLARE @i int=201;
            WHILE @i<=400 BEGIN INSERT INTO {{schema}}.Companion VALUES(@i,@i);
              INSERT INTO {{schema}}.Items VALUES(@i,@i,NULL); SET @i=@i+1; END;
            UPDATE {{schema}}.Items SET id=1001 WHERE id=1;
            UPDATE {{schema}}.Items SET value=222 WHERE id=2;
            DELETE FROM {{schema}}.Items WHERE id=3; COMMIT;",
        )?;
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let a = rows(&harness.results[0])?;
                let b = rows(&harness.results[1])?;
                if a.len() == 399 && b.len() == 400 {
                    ensure!(
                        !a.iter().any(|row| row["id"] == 1 || row["id"] == 3),
                        "rekey/delete old keys survived"
                    );
                    ensure!(
                        a.iter().any(|row| row["id"] == 1001 && row["value"] == 10),
                        "split rekey lost its after image"
                    );
                    ensure!(
                        a.iter().any(|row| row["id"] == 2 && row["value"] == 222),
                        "update pair was split incorrectly"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = harness.shutdown().await;
    proof?;
    cleanup?;

    db.sql(
        "ALTER TABLE {{schema}}.ReadOnlyItems ALTER COLUMN note nvarchar(max) NULL;
        UPDATE {{schema}}.ReadOnlyItems SET note=REPLICATE(N'界',2000) WHERE id=1;",
    )?;
    let factory = native[1]
        .bootstrap_factories()
        .iter()
        .find(|f| f.metadata().implementation.name.as_ref() == "drasi.mssql/snapshot")
        .context("read-only factory")?;
    let mut settings = db.read_only_settings();
    settings["max_rows"] = json!(1);
    settings["max_bytes"] = json!(1024);
    let provider = factory.create_provider(
        id("oversized"),
        json!({"source":"items","stream":stream("items"),"settings":settings}),
        Some(scope()),
        None,
    )?;
    provider.prepare().await?;
    let mut snapshot = provider.snapshot().await?;
    let mut failed = false;
    while let Some(row) = snapshot.changes.next().await {
        if let Err(error) = row {
            ensure!(
                format!("{error:#}").contains("max_bytes"),
                "unexpected oversized-record failure"
            );
            failed = true;
            break;
        }
    }
    ensure!(failed, "oversized individual record was silently truncated");
    ensure!(
        provider.complete_snapshot().await.is_err(),
        "failed snapshot published completion"
    );
    provider.stop().await?;
    db.remove()
}
