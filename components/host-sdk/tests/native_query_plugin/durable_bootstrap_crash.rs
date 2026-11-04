// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::io::Write;

const ROOT: &str = "DRASI_QUERY_PLUGIN_BOOTSTRAP_CRASH_ROOT";
const PHASE: &str = "DRASI_QUERY_PLUGIN_BOOTSTRAP_CRASH_PHASE";
const INTENT: &str = "BOOTSTRAP-STATE:";

#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit fixture launched by the bootstrap crash cases"]
async fn indexed_bootstrap_exit_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("bootstrap crash root missing")?);
    let phase = std::env::var(PHASE)?;
    let database = root.join("source.sqlite");
    let indexes = root.join("indexes");
    let graph = if phase == "partial" {
        let gate = super::super::lifecycle::Gate::new(false);
        let graph = Graph::with_gate(&database, &indexes, 1, Some(gate.clone())).await?;
        {
            let start = graph.query.start();
            tokio::pin!(start);
            tokio::select! {
                result = &mut start => anyhow::bail!("bootstrap did not pause after its first row: {result:?}"),
                _ = gate.reached.notified() => {}
            }
            assert!(!graph.progress.snapshot().bootstrap_complete);
            let state = graph
                .bootstrap_state
                .read()
                .await?
                .context("missing real initialization intent")?;
            println!("\n{INTENT}{}", std::str::from_utf8(&state)?);
            std::io::stdout().flush()?;
            std::process::exit(88);
        }
    } else {
        anyhow::ensure!(phase == "complete", "unknown bootstrap crash phase");
        Graph::new(&database, &indexes, 1).await?
    };
    graph.query.start().await?;
    assert_eq!(total(&graph.snapshot().await?)?, 30.0);
    let state = graph
        .bootstrap_state
        .read()
        .await?
        .context("missing completed handover")?;
    println!("\n{INTENT}{}", std::str::from_utf8(&state)?);
    std::io::stdout().flush()?;
    std::process::exit(89)
}

async fn recover_after_exit(phase: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let child = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::bootstrap::coordinated::crashes::indexed_bootstrap_exit_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, directory.path())
            .env(PHASE, phase)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        child.status.code() == Some(if phase == "partial" { 88 } else { 89 }),
        "bootstrap child failed: {}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let stdout = String::from_utf8(child.stdout)?;
    let saved: serde_json::Value = serde_json::from_str(
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(INTENT))
            .context("missing child handover evidence")?,
    )?;
    assert_eq!(saved["completed"], phase == "complete");
    let graph = if phase == "partial" {
        let gate = super::super::lifecycle::Gate::new(false);
        gate.release();
        Graph::with_gate(&database, &indexes, 2, Some(gate)).await?
    } else {
        Graph::new(&database, &indexes, 2).await?
    };
    let recovered = async {
        graph.start().await?;
        assert_eq!(total(&graph.snapshot().await?)?, 30.0);
        assert_eq!(graph.snapshot().await?.cursor().sequence(), 0);
        assert_eq!(
            graph.progress.snapshot().reset_generation,
            u64::from(phase == "partial")
        );
        let completed: serde_json::Value = serde_json::from_slice(
            &graph
                .bootstrap_state
                .read()
                .await?
                .context("missing recovered handover")?,
        )?;
        assert_eq!(completed["completed"], true);
        assert_eq!(completed["epoch"], saved["epoch"]);
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":18}}]))
            .await?;
        let output = graph.output(0).await?;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(30.0),
            38.0,
            1,
        )?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    recovered?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_partial_process_exit_recovers_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), recover_after_exit("partial")).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_bootstrap_partial_process_exit_recovers_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), recover_after_exit("partial")).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_completed_process_exit_recovers_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), recover_after_exit("complete")).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_bootstrap_completed_process_exit_recovers_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), recover_after_exit("complete")).await?
}
