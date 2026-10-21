// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use std::{future::Future, time::Duration};
use tokio::task::JoinHandle;

/// Retain SQL work across cancellation of its caller. Tiberius has no public
/// ATTENTION API: a client timeout must not be mistaken for server cancellation.
pub struct Work<T> {
    task: Option<JoinHandle<Result<T>>>,
}
impl<T: Send + 'static> Default for Work<T> {
    fn default() -> Self {
        Self { task: None }
    }
}
impl<T: Send + 'static> Work<T> {
    pub fn pending(&self) -> bool {
        self.task.is_some()
    }
    pub fn start(
        &mut self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<()> {
        anyhow::ensure!(self.task.is_none(), "SQL Server work is already owned");
        self.task = Some(tokio::spawn(future));
        Ok(())
    }
    pub async fn wait(&mut self, timeout: Duration) -> Result<T> {
        let result = tokio::time::timeout(timeout, self.task.as_mut().context("SQL Server work is absent")?)
            .await.context("SQL Server work timed out; retained work requires awaited cleanup; server-side cancellation is not established")?;
        self.task = None;
        result.context("SQL Server worker failed")?
    }
    pub async fn stop(&mut self, timeout: Duration) -> Result<()> {
        if self.pending() {
            self.wait(timeout).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn retained_work() -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut work = Work::default();
        work.start(async move {
            rx.await?;
            Ok(())
        })?;
        assert!(work.stop(Duration::from_millis(1)).await.is_err());
        assert!(work.pending());
        tx.send(()).unwrap();
        work.stop(Duration::from_secs(1)).await?;
        assert!(!work.pending());
        Ok(())
    }
    #[tokio::test]
    async fn retains_work_current_thread() -> Result<()> {
        retained_work().await
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retains_work_multi_thread() -> Result<()> {
        retained_work().await
    }
}
