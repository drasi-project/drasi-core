// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use drasi_lib::{computation::v1::*, DrasiLib};
use rand::RngCore;
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, sync::watch};

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

pub(super) struct Health {
    inspector: ComputationInspector,
    changes: watch::Receiver<Arc<ComputationInspection>>,
    sequence: u64,
}

impl Health {
    pub(super) fn new(core: &DrasiLib) -> anyhow::Result<Self> {
        let inspector = core.computation_control()?.inspector();
        let mut changes = inspector.subscribe();
        let initial = changes.borrow_and_update().clone();
        Self::inspect(&initial)?;
        Ok(Self {
            inspector,
            changes,
            sequence: initial.sequence,
        })
    }

    fn inspect(snapshot: &ComputationInspection) -> anyhow::Result<()> {
        let failures: Vec<_> = snapshot
            .observed
            .components
            .iter()
            .filter_map(|(id, component)| {
                component.failure.as_ref().map(|failure| {
                    anyhow::Error::new(GraphError::Reported {
                        cause: failure.cause.clone(),
                    })
                    .context(format!("component '{id}' failed"))
                })
            })
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                drasi_lib::error::OperationFailures::new("Stockroom component failure", failures)
                    .into(),
            )
        }
    }

    pub(super) fn check(&mut self) -> anyhow::Result<()> {
        drop(self.changes.borrow_and_update());
        // A watch can coalesce a failure followed by cleanup; inspect every retained publication.
        for snapshot in self
            .inspector
            .history(self.sequence)
            .context("component observation history was lost; stop and inspect the graph")?
        {
            Self::inspect(&snapshot)?;
            self.sequence = snapshot.sequence;
        }
        Ok(())
    }

    pub(super) async fn failed(&mut self) -> anyhow::Result<()> {
        loop {
            self.check()?;
            self.changes
                .changed()
                .await
                .context("component observation closed")?;
        }
    }
}

pub(super) async fn wait_started(
    core: &DrasiLib,
    id: &str,
    deadline: Duration,
) -> anyhow::Result<()> {
    let component = core.computation_component(id)?;
    match tokio::time::timeout(deadline, component.wait_started()).await {
        Ok(result) => result.with_context(|| format!("start component '{id}'")),
        Err(error) => {
            let waiting = component
                .observed()
                .with_context(|| format!("inspect component '{id}' after readiness timeout"))?
                .waiting_reason()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            Err(anyhow::Error::new(error)
                .context(format!("wait for component '{id}' readiness{waiting}")))
        }
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
            return;
        }
        if let Some(report) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<LifecycleReportError>())
        {
            let failures: Vec<_> = match report {
                LifecycleReportError::Start(report) => report
                    .components
                    .iter()
                    .filter_map(|(id, outcome)| {
                        if let StartOutcome::StartFailed(failure) = outcome {
                            Some((format!("start component '{id}'"), failure.cause.clone()))
                        } else {
                            None
                        }
                    })
                    .collect(),
                LifecycleReportError::Stop(report) => report
                    .components
                    .iter()
                    .filter_map(|(id, outcome)| {
                        if let StopOutcome::StopFailed(failure) = outcome {
                            Some((format!("stop component '{id}'"), failure.cause.clone()))
                        } else {
                            None
                        }
                    })
                    .collect(),
                LifecycleReportError::Reconciliation(report) => report
                    .failures
                    .iter()
                    .map(|failure| ("reconcile graph".to_owned(), failure.cause.clone()))
                    .collect(),
                _ => Vec::new(),
            };
            if !failures.is_empty() {
                for (context, cause) in failures {
                    collect(
                        &anyhow::Error::new(GraphError::Reported { cause }).context(context),
                        messages,
                        seen,
                    );
                }
                return;
            }
        }
        let root = error.root_cause().to_string();
        let context = error.chain().find_map(|cause| {
            cause.source()?;
            let message = cause.to_string();
            // Framework displays may already embed the root cause several times.
            (!message.contains(&root)).then_some(message)
        });
        let message = context.map_or_else(|| root.clone(), |context| format!("{context}: {root}"));
        if seen.insert(message.clone()) {
            messages.push(message);
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
        assert_eq!(
            error_messages(&error),
            ["start: duplicate record key: bolts"]
        );
        let group = drasi_lib::error::OperationFailures::new(
            "work and cleanup",
            vec![
                error,
                anyhow::anyhow!("shutdown failed"),
                anyhow::anyhow!("duplicate record key: bolts").context("start"),
            ],
        );
        assert_eq!(
            error_messages(&group.into()),
            ["start: duplicate record key: bolts", "shutdown failed"]
        );
    }

    #[test]
    fn equal_causes_keep_distinct_operation_targets() {
        let group = drasi_lib::error::OperationFailures::new(
            "destinations failed",
            vec![
                anyhow::anyhow!("connection refused").context("webhook A"),
                anyhow::anyhow!("connection refused").context("webhook B"),
            ],
        );
        assert_eq!(
            error_messages(&group.into()),
            ["webhook A: connection refused", "webhook B: connection refused"]
        );
    }

    #[test]
    fn lifecycle_reports_preserve_every_failed_component() -> anyhow::Result<()> {
        let mut components = std::collections::BTreeMap::new();
        for name in ["first", "second"] {
            let id = ComponentId::try_new(name)?;
            components.insert(
                id.clone(),
                StartOutcome::StartFailed(ComponentFailure {
                    phase: FailurePhase::Activation,
                    disposition: FailureDisposition::Terminal,
                    cause: Arc::new(GraphError::Component {
                        component: id,
                        operation: "start",
                        source: anyhow::anyhow!("connection refused"),
                    }),
                    timestamp: std::time::SystemTime::now().into(),
                }),
            );
        }
        let error = LifecycleReportError::Start(StartReport {
            revision: GraphRevision(0),
            summary: OperationSummary::CompletedWithFailures,
            components,
        });
        assert_eq!(
            error_messages(&error.into()),
            [
                "start component 'first': connection refused",
                "start component 'second': connection refused"
            ]
        );
        Ok(())
    }
}
