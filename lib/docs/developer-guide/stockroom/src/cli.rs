// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use rand::RngCore;
use std::{collections::BTreeSet, path::Path};
use tokio::io::AsyncWriteExt;

pub(super) struct Shutdown {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
    #[cfg(windows)]
    signal: tokio::signal::windows::CtrlC,
}

impl Shutdown {
    pub(super) fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        #[cfg(windows)]
        let signal = tokio::signal::windows::ctrl_c()?;
        Ok(Self { signal })
    }

    pub(super) async fn requested(&mut self) -> anyhow::Result<()> {
        self.signal.recv().await.context("Ctrl-C listener closed")
    }
}

pub(super) fn error_messages(error: &anyhow::Error) -> Vec<String> {
    fn collect(error: &anyhow::Error, messages: &mut Vec<String>, seen: &mut BTreeSet<String>) {
        if let Some(group) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<drasi_lib::error::OperationFailures>())
        {
            for failure in group.failures() {
                collect(failure, messages, seen);
            }
        } else {
            let message = error.root_cause().to_string();
            if seen.insert(message.clone()) {
                messages.push(message);
            }
        }
    }
    let mut messages = Vec::new();
    collect(error, &mut messages, &mut BTreeSet::new());
    messages
}

pub(super) async fn keygen(path: &Path) -> anyhow::Result<()> {
    let mut key = [0; 32];
    rand::rngs::OsRng.try_fill_bytes(&mut key)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("create new key file {}", path.display()))?;
    file.write_all(&key).await?;
    file.sync_all().await?;
    println!(
        "Created {}. Keep it private and preserve it with your backups.",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keygen_is_private_and_never_overwrites() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("key");
        keygen(&path).await?;
        let original = tokio::fs::read(&path).await?;
        assert_eq!(original.len(), 32);
        assert!(keygen(&path).await.is_err());
        assert_eq!(tokio::fs::read(&path).await?, original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o077, 0);
        }
        Ok(())
    }

    #[test]
    fn errors_preserve_distinct_causes_without_repeating_context() {
        let error = anyhow::anyhow!("duplicate record key: bolts")
            .context("load")
            .context("start");
        assert_eq!(error_messages(&error), ["duplicate record key: bolts"]);
        let group = drasi_lib::error::OperationFailures::new(
            "work and cleanup",
            vec![
                error,
                anyhow::anyhow!("shutdown failed"),
                anyhow::anyhow!("duplicate record key: bolts"),
            ],
        );
        assert_eq!(
            error_messages(&group.into()),
            ["duplicate record key: bolts", "shutdown failed"]
        );
    }
}
