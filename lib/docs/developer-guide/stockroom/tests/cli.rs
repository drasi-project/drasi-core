// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::process::Command;

#[cfg(unix)]
#[path = "../src/cli.rs"]
mod terminal;

#[cfg(unix)]
#[test]
fn ctrl_c_is_retained_while_the_application_is_busy() -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};

    if std::env::var("DRASI_BOOK_SIGNAL_CHILD").as_deref() == Ok("1") {
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let mut shutdown = terminal::Shutdown::new()?;
                println!("SIGNAL_RECEIVER_READY");
                std::io::stdout().flush()?;
                tokio::time::sleep(Duration::from_secs(1)).await;
                tokio::time::timeout(Duration::from_secs(2), shutdown.requested()).await??;
                Ok(())
            });
    }

    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                if let Err(error) = self.0.kill() {
                    eprintln!("signal test child termination failed: {error}");
                }
                if let Err(error) = self.0.wait() {
                    eprintln!("signal test child join failed: {error}");
                }
            }
        }
    }
    let mut child = OwnedChild(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "ctrl_c_is_retained_while_the_application_is_busy", "--nocapture"])
            .env("DRASI_BOOK_SIGNAL_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .spawn()?,
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let mut output = std::io::BufReader::new(stdout);
    let mut ready = false;
    loop {
        let mut line = String::new();
        if output.read_line(&mut line)? == 0 {
            break;
        }
        if line.contains("SIGNAL_RECEIVER_READY") {
            ready = true;
            break;
        }
    }
    anyhow::ensure!(ready, "signal test child did not become ready");
    anyhow::ensure!(
        Command::new("kill")
            .args(["-INT", &child.0.id().to_string()])
            .status()?
            .success(),
        "failed to signal the owned test child"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait()? {
            anyhow::ensure!(status.success(), "signal test child failed: {status}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "signal was lost while busy");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn duplicate_sku_has_one_concise_root_cause() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("inventory.json"),
        r#"[
        {"sku":"bolts","quantity":3,"minimum":5,"warehouse":"west"},
        {"sku":"bolts","quantity":2,"minimum":5,"warehouse":"west"}
    ]"#,
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_stockroom"))
        .current_dir(directory.path())
        .env("RUST_LOG", "warn")
        .output()?;
    let error = String::from_utf8(output.stderr)?;
    assert!(!output.status.success());
    assert_eq!(
        error.matches("duplicate record key: bolts").count(),
        1,
        "{error}"
    );
    assert!(error.len() < 512, "{error}");
    Ok(())
}

#[test]
fn keygen_command_refuses_to_replace_a_key() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_stockroom"))
            .current_dir(directory.path())
            .args(["keygen", "stockroom.key"])
            .output()
    };
    assert!(run()?.status.success());
    let first = std::fs::read(directory.path().join("stockroom.key"))?;
    assert_eq!(first.len(), 32);
    let second = run()?;
    assert!(!second.status.success());
    assert_eq!(
        std::fs::read(directory.path().join("stockroom.key"))?,
        first
    );
    Ok(())
}
