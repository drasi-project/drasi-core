// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

const ROOT: &str = "DRASI_SCHEDULING_CRASH_ROOT";
const PHASE: &str = "DRASI_SCHEDULING_CRASH_PHASE";
const FLAVOR: &str = "DRASI_SCHEDULING_RUNTIME";
const LOADED: &str = "DRASI_SCHEDULING_LOADED";

#[test]
#[ignore = "subprocess fixture invoked by the standalone scheduling restart contracts"]
fn scheduled_process_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("missing scheduling crash directory")?);
    let phase = std::env::var(PHASE)?;
    let loaded = std::env::var(LOADED)? == "true";
    let runtime = match std::env::var(FLAVOR)?.as_str() {
        "current_thread" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
        "multi_thread" => tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
        _ => anyhow::bail!("unsupported scheduling runtime"),
    };
    runtime.block_on(async {
        let clock = Clock::new(if phase.starts_with("request") {
            1000
        } else {
            2000
        });
        let signals = Arc::new(CommitSignals::default());
        let provider = Arc::new(ObservedProvider {
            inner: provider(&root)?,
            signals: signals.clone(),
        });
        let mut component = create(loaded, provider, clock, false, false)?;
        component.start().await?;
        assert!(component.continue_transform().await?.is_empty());
        signals.exit.store(
            if phase.ends_with("before") { 1 } else { 2 },
            Ordering::Release,
        );
        if phase.starts_with("request") {
            component.transform(input("one", "request", 7, 1)?).await?;
        } else {
            component.on_wakeup().await?;
        }
        anyhow::bail!("scheduling process did not exit at the armed commit boundary")
    })
}

async fn crash(root: &Path, loaded: bool, flavor: &str, phase: &str) -> Result<()> {
    let output = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "restart::scheduled_process_worker",
                "--ignored",
                "--nocapture",
            ])
            .env(ROOT, root)
            .env(PHASE, phase)
            .env(FLAVOR, flavor)
            .env(LOADED, loaded.to_string())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("scheduled process did not reach its storage commit")??;
    let expected = if phase.ends_with("before") { 81 } else { 82 };
    anyhow::ensure!(
        output.status.code() == Some(expected),
        "scheduled child {phase}/{flavor}/loaded={loaded} failed: {output:?}"
    );
    Ok(())
}

async fn restart_contract(loaded: bool, flavor: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    crash(directory.path(), loaded, flavor, "request-before").await?;
    let state = stored(directory.path()).await?;
    assert_eq!(state.next_due, None);
    assert!(state.element.is_none());
    assert_eq!(state.completed, 0);
    assert!(state.outputs.is_empty());

    crash(directory.path(), loaded, flavor, "request-after").await?;
    let state = stored(directory.path()).await?;
    assert_eq!(state.next_due, Some(2000));
    assert!(state.element.is_some());
    assert_eq!(state.completed, 0);
    assert!(state.outputs.is_empty());

    crash(directory.path(), loaded, flavor, "due-before").await?;
    let state = stored(directory.path()).await?;
    assert_eq!(
        state.next_due,
        Some(2000),
        "failed completion retains due work"
    );
    assert_eq!(state.completed, 0);
    assert_eq!(state.head, Some(0));
    assert!(state.outputs.is_empty());

    crash(directory.path(), loaded, flavor, "due-after").await?;
    let state = stored(directory.path()).await?;
    assert_eq!(
        state.next_due, None,
        "committed completion is not re-executed"
    );
    assert_eq!(state.completed, 1);
    assert_eq!(state.head, Some(1));
    assert_eq!(state.outputs.len(), 1);
    assert_eq!(state.outputs[0].0, 1);
    let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
    let committed = codec.decode(&state.outputs[0].1)?;
    let original = input("one", "request", 7, 1)?;
    let clock = Clock::new(10_000);

    let mut component = create(
        loaded,
        provider(directory.path())?,
        clock.clone(),
        false,
        false,
    )?;
    component.start().await?;
    let replay = component.continue_transform().await?;
    assert_eq!(replay.len(), 1);
    assert_output(&replay[0], &original.envelope, 2000, 1)?;
    assert_eq!(
        GraphProducerProgress::from_envelope(&replay[0].envelope)?,
        GraphProducerProgress::from_envelope(&committed)?,
        "replay preserves the exact committed logical identity"
    );
    assert!(replay[0].envelope.system().sequence() > committed.system().sequence());
    component.stop().await?;
    drop(component);

    let mut component = create(loaded, provider(directory.path())?, clock, false, false)?;
    component.start().await?;
    let replay_again = component.continue_transform().await?;
    assert_eq!(replay_again.len(), 1);
    assert_eq!(
        GraphProducerProgress::from_envelope(&replay_again[0].envelope)?,
        GraphProducerProgress::from_envelope(&committed)?,
    );
    component.delivery_completed(&replay_again).await?;
    assert!(component.on_wakeup().await?.is_empty());
    component.stop().await?;
    drop(component);
    let final_state = stored(directory.path()).await?;
    assert_eq!(
        final_state.completed, 1,
        "repeated reconstruction cannot commit completion twice"
    );
    assert_eq!(final_state.next_due, None);
    assert_eq!(final_state.head, Some(1));

    let mut component = create(
        loaded,
        provider(directory.path())?,
        Clock::new(20_000),
        false,
        false,
    )?;
    component.start().await?;
    assert!(
        component.continue_transform().await?.is_empty(),
        "confirmed output must not replay"
    );
    assert!(component.on_wakeup().await?.is_empty());
    component.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_process_restart_current_thread() -> Result<()> {
    for loaded in [false, true] {
        restart_contract(loaded, "current_thread").await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_process_restart_multi_thread() -> Result<()> {
    for loaded in [false, true] {
        restart_contract(loaded, "multi_thread").await?;
    }
    Ok(())
}
