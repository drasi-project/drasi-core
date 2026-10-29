// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use std::{collections::BTreeSet, path::PathBuf};

struct OwnedState {
    path: PathBuf,
    removed: bool,
}
impl OwnedState {
    fn new(db: &Database) -> Result<Self> {
        let requested = PathBuf::from(
            std::env::var_os("DRASI_MSSQL_TEST_STATE_ROOT")
                .context("persistent SQL Server tests require DRASI_MSSQL_TEST_STATE_ROOT")?,
        );
        let root = requested.canonicalize()?;
        ensure!(
            requested == root && root.is_dir(),
            "state root must be an existing canonical directory"
        );
        ensure!(
            ![
                std::path::Path::new("/tmp"),
                std::path::Path::new("/private/tmp"),
                std::path::Path::new("/var/tmp")
            ]
            .iter()
            .any(|prefix| root.starts_with(prefix)),
            "use an owned project/session state directory, not a temporary directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                std::fs::metadata(&root)?.permissions().mode() & 0o077 == 0,
                "state root must be owner-only"
            );
        }
        let path = root.join(&db.schema);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .context("create a new, never-reused owned state directory")?;
        Ok(Self {
            path,
            removed: false,
        })
    }
    fn provider(&self) -> Result<Arc<dyn ComputationIndexProvider>> {
        Ok(Arc::new(RocksDbComputationProvider::new(
            &self.path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )))
    }
    fn remove(&mut self) -> Result<()> {
        if !self.removed {
            std::fs::remove_dir_all(&self.path)?;
            self.removed = true;
        }
        Ok(())
    }
}
impl Drop for OwnedState {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("owned SQL Server qualification state cleanup failed: {error}");
        }
    }
}
fn position(owner: &QuerySourceProgress) -> Result<bytes::Bytes> {
    owner
        .snapshot()
        .checkpoints
        .get(&SourceProgressKey::Source("items".into()))
        .context("actual query source checkpoint is missing")?
        .source_position
        .clone()
        .context("actual CDC checkpoint is missing")
}
fn lsn_hex(position: &[u8]) -> Result<String> {
    let cursor: Value = serde_json::from_slice(position)?;
    cursor["lsn"]
        .as_array()
        .context("real checkpoint LSN")?
        .iter()
        .map(|byte| {
            byte.as_u64()
                .filter(|v| *v < 256)
                .map(|v| format!("{v:02X}"))
                .context("checkpoint LSN byte")
        })
        .collect()
}

pub(super) async fn partial_persistent(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let mut state = OwnedState::new(&db)?;
    let harness = Harness::new(dynamic, db.settings(), state.provider()?, false).await?;
    let proof=async {
        harness.start().await?;
        wait_rows(&harness.results[0],json!([{"id":1,"value":10},{"id":2,"value":20}])).await?;
        harness.gate.blocked.store(true,Ordering::Release);
        db.sql("SET NOCOUNT ON; BEGIN TRANSACTION; DECLARE @id int=10;
            WHILE @id<510 BEGIN INSERT INTO {{schema}}.Items VALUES(@id,@id,NULL); SET @id=@id+1; END; COMMIT;")?;
        tokio::time::timeout(Duration::from_secs(25),async {
            loop {
                let partial=harness.owners.iter().all(|owner| {
                    position(owner).ok().and_then(|p|serde_json::from_slice::<Value>(&p).ok())
                        .is_some_and(|cursor|cursor["row"].is_array())
                });
                if partial && harness.gate.entered.load(Ordering::Acquire)>0 { return Ok::<_,anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        ensure!(harness.owners.iter().all(|owner|owner.snapshot().persistent),"test is not using actual persistent query progress");
        let source=harness.core.computation_component("items")?;
        let mut stopping=Box::pin(source.stop());
        let finished=tokio::select! {
            result=&mut stopping=>{result?;true},
            _=tokio::time::sleep(Duration::from_millis(20))=>false,
        };
        harness.gate.release();
        if !finished { tokio::time::timeout(Duration::from_secs(20),stopping).await??; }
        harness.core.stop().await?;
        for (result,owner) in harness.results.iter().zip(&harness.owners) {
            let count=rows(result)?.len();
            ensure!(count>2 && count<502,"failed to stop with an actual partially consumed CDC transaction: {count}");
            let cursor:Value=serde_json::from_slice(&position(owner)?)?;
            ensure!(cursor["row"].is_array(),"partial row checkpoint was replaced with a transaction boundary");
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = harness.shutdown().await;
    proof?;
    cleanup?;
    drop(harness);
    db.sql("BEGIN TRAN; UPDATE {{schema}}.Items SET value=888 WHERE id=13; ROLLBACK;
        BEGIN TRAN; UPDATE {{schema}}.Items SET value=999 WHERE id=10;
        UPDATE {{schema}}.Items SET id=1011 WHERE id=11; DELETE FROM {{schema}}.Items WHERE id=12; COMMIT;")?;
    let restored = Harness::new(dynamic, db.settings(), state.provider()?, false).await?;
    let proof = async {
        restored.start().await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if restored
                    .results
                    .iter()
                    .all(|result| rows(result).is_ok_and(|rows| rows.len() == 501))
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let expected: BTreeSet<i64> = [1, 2, 1011]
            .into_iter()
            .chain((10..510).filter(|id| *id != 11 && *id != 12))
            .collect();
        for result in &restored.results {
            let rows = rows(result)?;
            let actual = rows
                .iter()
                .map(|row| row["id"].as_i64().context("id"))
                .collect::<Result<BTreeSet<_>>>()?;
            ensure!(
                actual == expected,
                "persistent partial replay lost/duplicated identities"
            );
            for row in rows {
                let id = row["id"].as_i64().context("persisted query row id")?;
                let expected = match id {
                    1 => 10,
                    2 => 20,
                    10 => 999,
                    1011 => 11,
                    other => other,
                };
                ensure!(
                    row["value"] == expected,
                    "rollback/rekey/update semantics changed after reopening persistent indexes"
                );
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = restored.shutdown().await;
    proof?;
    cleanup?;
    drop(restored);
    state.remove()?;
    db.remove()
}

pub(super) async fn catalog_changes(dynamic: bool) -> Result<()> {
    for change in [
        "column",
        "capture",
        "keys",
        "retention",
        "retention-retained",
    ] {
        let mut db = Database::new().await?;
        let mut state = OwnedState::new(&db)?;
        let before = Harness::new(dynamic, db.settings(), state.provider()?, false).await?;
        before.start().await?;
        db.sql("INSERT INTO {{schema}}.Items VALUES(3,30,N'checkpoint')")?;
        wait_rows(
            &before.results[0],
            json!([{"id":1,"value":10},{"id":2,"value":20},{"id":3,"value":30}]),
        )
        .await?;
        wait_rows(
            &before.results[1],
            json!([{"id":1,"value":10},{"id":2,"value":20},{"id":3,"value":30}]),
        )
        .await?;
        before.core.stop().await?;
        let checkpoints = before
            .owners
            .iter()
            .map(|owner| position(owner))
            .collect::<Result<Vec<_>>>()?;
        before.shutdown().await?;
        drop(before);
        let mut settings = db.settings();
        match change {
            "column" => {
                db.sql("ALTER TABLE {{schema}}.Items ALTER COLUMN value bigint NOT NULL")?;
            }
            "capture" => {
                db.sql("EXEC sys.sp_cdc_disable_table @source_schema=N'{{schema_name}}',@source_name=N'Items',@capture_instance=N'all';
                    EXEC sys.sp_cdc_enable_table @source_schema=N'{{schema_name}}',@source_name=N'Items',@role_name=NULL,@supports_net_changes=0;
                    UPDATE {{schema}}.Items SET value=31 WHERE id=3;")?;
                db.regrant_items_capture()?;
            }
            "keys" => {
                settings["connection"]["table_keys"] =
                    json!([{"table":format!("{}.Items",db.schema),"key_columns":["id","value"]}])
            }
            "retention" => {
                db.sql("UPDATE {{schema}}.Items SET value=32 WHERE id=3;")?;
                db.wait_sql(&format!("SET NOCOUNT ON; SELECT COUNT(*) FROM cdc.[{}_Items_CT] WHERE id=3 AND value=32 AND __$operation=4",db.schema),"1").await?;
                db.sql("DECLARE @low binary(10)=sys.fn_cdc_get_max_lsn();
                    EXEC sys.sp_cdc_cleanup_change_table @capture_instance=N'{{schema_name}}_Items',@low_water_mark=@low,@threshold=5000;")?;
                let old = lsn_hex(&checkpoints[0])?;
                ensure!(db.sql(&format!("SET NOCOUNT ON; SELECT CASE WHEN sys.fn_cdc_get_min_lsn(N'{{{{schema_name}}}}_Items')>0x{old} THEN 1 ELSE 0 END"))?=="1",
                    "controlled cleanup did not create an actual CDC retention gap");
            }
            _ => {
                let low = lsn_hex(&checkpoints[0])?;
                db.sql(&format!("DECLARE @low binary(10)=0x{low};
                    EXEC sys.sp_cdc_cleanup_change_table @capture_instance=N'{{{{schema_name}}}}_Items',@low_water_mark=@low,@threshold=5000;"))?;
                ensure!(
                    db.sql(&format!(
                        "SET NOCOUNT ON; SELECT CASE WHEN EXISTS(
                    SELECT 1 FROM cdc.[{}_Items_CT] WHERE __$start_lsn=0x{low}) THEN 1 ELSE 0 END",
                        db.schema
                    ))? == "1",
                    "retained-cursor test accidentally removed its checkpoint row"
                );
            }
        }
        let after = Harness::new(dynamic, settings, state.provider()?, false).await?;
        let proof = async {
            // Failure can occur asynchronously after a successful lifecycle start.
            if change=="retention-retained" {
                after.start().await?;
                db.sql("INSERT INTO {{schema}}.Items VALUES(4,40,N'after-valid-cleanup')")?;
                for result in &after.results {
                    wait_rows(result,json!([{"id":1,"value":10},{"id":2,"value":20},{"id":3,"value":30},{"id":4,"value":40}])).await?;
                }
            } else {
                after.core.start().await?;
                after.failure(&["changed","gap"]).await?;
                for (owner, expected) in after.owners.iter().zip(&checkpoints) {
                    ensure!(
                        position(owner)?.as_ref() == expected.as_ref(),
                        "source silently skipped a gap or adopted a changed catalog"
                    );
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = after.shutdown().await;
        proof?;
        cleanup?;
        drop(after);
        state.remove()?;
        db.remove()?;
    }
    Ok(())
}
