// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

const CRASH_ROOT: &str = "DRASI_QUERY_PLUGIN_FANOUT_ROOT";

#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit fixture launched by the partial fanout cases"]
async fn indexed_fanout_exit_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(CRASH_ROOT).context("missing fanout root")?);
    let gate = Arc::new(CommitGate::default());
    let mut fixture = Fixture::with_gate(
        &root,
        Some(
            input(
                SourceChange::Insert {
                    element: item(10, 1),
                },
                1,
            )?
            .envelope,
        ),
        Some(gate.clone()),
    )
    .await?;
    gate.armed.store(true, Ordering::Release);
    let run = fixture.graph.start()?;
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => anyhow::bail!("fanout completed before the second journal committed: {result:?}"),
        result = async {
            gate.entered.notified().await;
            loop {
                let changed = fixture.changed[0].notified();
                if !fixture.seen[0].lock().expect("result capture poisoned").is_empty() { break; }
                changed.await;
            }
            assert_eq!(fixture.channels[0].progress().await?.accepted, 1);
            Ok::<_, anyhow::Error>(())
        } => result?,
    }
    std::process::exit(90)
}

async fn partial_fanout() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let child = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "durable::destinations::crashes::indexed_fanout_exit_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CRASH_ROOT, root)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        child.status.code() == Some(90),
        "partial fanout child failed: {}\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let owner = ComputationTransaction::try_new(
        provider(&root.join("query"))?
            .create_indexes(GRAPH, COMPONENT)
            .await?,
    )?;
    let bindings = OutputBindings::load(
        owner
            .resources()
            .checkpoint_store()
            .expect("actual checkpoint store")
            .read_checkpoint(OUTPUT_BINDINGS_CHECKPOINT)
            .await?
            .as_ref(),
    )?;
    assert_eq!(bindings.destinations().len(), 2);
    owner.shutdown().await?;
    drop(owner);
    let mut pending = create(&root.join("query"), QUERY, 2)?;
    pending.start().await?;
    assert!(
        pending
            .bind_output_destinations(&OutputBindings::try_new(
                bindings.destinations()[..1].iter().cloned()
            )?)
            .await
            .is_err(),
        "pending fanout lost a destination"
    );
    let mut replacement = bindings.destinations().to_vec();
    replacement[1].journal = "ffffffff-ffff-4fff-8fff-ffffffffffff".parse()?;
    assert!(
        pending
            .bind_output_destinations(&OutputBindings::try_new(replacement)?)
            .await
            .is_err(),
        "matching names accepted a replacement journal while output was pending"
    );
    pending.stop().await?;
    drop(pending);
    let mut recovered = Fixture::new(root, None).await?;
    let run = tokio::time::timeout(Duration::from_secs(15), recovered.graph.start()?).await;
    if run.as_ref().is_ok_and(|result| result.is_ok()) {
        for channel in &recovered.channels {
            assert_eq!(
                channel.progress().await?.accepted,
                1,
                "fanout replay appended a duplicate"
            );
            assert_eq!(channel.progress().await?.processed["target"], 1);
        }
        assert_eq!(
            recovered.seen[1]
                .lock()
                .expect("result capture poisoned")
                .len(),
            1
        );
    }
    let stopped = recovered.graph.shutdown().await;
    run??;
    stopped?;
    drop(recovered);
    let mut confirmed = create(&root.join("query"), QUERY, 3)?;
    confirmed.start().await?;
    confirmed
        .bind_output_destinations(&OutputBindings::default())
        .await?;
    assert!(confirmed.continue_transform().await?.is_empty());
    confirmed.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_output_recovers_partial_fanout_without_redirecting_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), partial_fanout()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_output_recovers_partial_fanout_without_redirecting_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(50), partial_fanout()).await?
}
