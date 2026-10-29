// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL fixtures"]
async fn repeated_and_failed_starts_remove_owned_volume_count_and_data() -> Result<()> {
    let directory = Database::evidence_directory().join(format!(
        "teardown-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
    ));
    std::fs::create_dir_all(&directory)?;
    let mut populated_bytes = 0u64;
    for legacy in [false, true] {
        let mut database = Database::start_recorded(false, legacy, &[], Some(&directory)).await?;
        database.initialize().await?;
        let bytes: u64 = database.sql(
            "SELECT COALESCE(SUM(DATA_LENGTH+INDEX_LENGTH),0) FROM information_schema.TABLES WHERE TABLE_SCHEMA='test'",
        ).await?.parse()?;
        anyhow::ensure!(bytes > 0, "fixture did not write volume-backed table data");
        populated_bytes += bytes;
        let volumes = database.volumes.clone();
        anyhow::ensure!(!volumes.is_empty(), "fixture recorded no owned volume");
        database.cleanup()?;
        for name in volumes {
            anyhow::ensure!(
                Command::new("podman")
                    .args(["volume", "exists", &name])
                    .status()?
                    .code()
                    == Some(1),
                "normal fixture teardown leaked volume {name}"
            );
        }
    }
    let failed = Database::start_recorded(
        false,
        false,
        &["--drasi-intentionally-invalid-fixture-option=1"],
        Some(&directory),
    )
    .await;
    anyhow::ensure!(
        failed.is_err(),
        "invalid database startup unexpectedly succeeded"
    );
    let mut records = 0usize;
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let proof: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
            anyhow::ensure!(
                proof["removed"] == true,
                "fixture cleanup was not completed"
            );
            anyhow::ensure!(proof["remaining_owned_volume_count"] == 0);
            anyhow::ensure!(proof["remaining_owned_volume_bytes"] == 0);
            for volume in proof["volumes"]
                .as_array()
                .context("recorded owned volumes")?
            {
                let name = volume.as_str().context("recorded volume name")?;
                anyhow::ensure!(
                    Command::new("podman")
                        .args(["volume", "exists", name])
                        .status()?
                        .code()
                        == Some(1),
                    "failed-start teardown leaked volume {name}"
                );
            }
            records += 1;
        }
    }
    anyhow::ensure!(
        records == 3,
        "normal/failed startup ownership evidence is incomplete"
    );
    std::fs::write(
        directory.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "recorded_fixtures":records, "populated_table_bytes":populated_bytes,
            "remaining_owned_volume_count":0, "remaining_owned_volume_bytes":0,
        }))?,
    )?;
    Ok(())
}
