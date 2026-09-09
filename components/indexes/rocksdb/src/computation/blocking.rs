// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll, Waker},
};

use drasi_core::interface::IndexError;
use tokio::{sync::oneshot, task::JoinHandle};

type Job = JoinHandle<Result<(), IndexError>>;

#[derive(Default)]
struct Work {
    jobs: Vec<Job>,
    failures: Vec<IndexError>,
}

impl Work {
    fn record(&mut self, result: Result<Result<(), IndexError>, tokio::task::JoinError>) {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.failures.push(error),
            Err(error) => self.failures.push(IndexError::other(error)),
        }
    }

    fn reap_ready(&mut self) {
        let mut context = Context::from_waker(Waker::noop());
        let mut index = 0;
        while index < self.jobs.len() {
            match Pin::new(&mut self.jobs[index]).poll(&mut context) {
                Poll::Ready(result) => {
                    drop(self.jobs.swap_remove(index));
                    self.record(result);
                }
                Poll::Pending => index += 1,
            }
        }
    }
}

/// Owns blocking jobs independently of the awaiter receiving each result.
/// Interrupted shutdown returns pending join handles to the owner.
#[derive(Default)]
pub(super) struct BlockingScope {
    cancelled: Arc<AtomicBool>,
    work: Mutex<Work>,
    shutdown_lock: tokio::sync::Mutex<()>,
}

impl BlockingScope {
    fn lock(&self) -> Result<MutexGuard<'_, Work>, IndexError> {
        self.work.lock().map_err(|error| {
            IndexError::other(std::io::Error::other(format!(
                "computation blocking-work registry poisoned: {error}"
            )))
        })
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) async fn run<T, F>(&self, operation: F) -> Result<T, IndexError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, IndexError> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        {
            let mut work = self.lock()?;
            work.reap_ready();
            if self.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            let cancellation = self.cancelled.clone();
            work.jobs.push(tokio::task::spawn_blocking(move || {
                if cancellation.load(Ordering::Acquire) {
                    let _ = sender.send(Err(cancelled()));
                    return Ok(());
                }
                match sender.send(operation()) {
                    Ok(()) | Err(Ok(_)) => Ok(()),
                    Err(Err(error)) => Err(error),
                }
            }));
        }
        let result = receiver.await.map_err(IndexError::other)?;
        self.lock()?.reap_ready();
        result
    }

    /// Keep an existing backend future alive until its own blocking work joins.
    /// This is provider I/O ownership, not a detached graph processing worker.
    pub(super) async fn run_async<T, F>(&self, operation: F) -> Result<T, IndexError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, IndexError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        {
            let mut work = self.lock()?;
            work.reap_ready();
            if self.cancelled.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            let cancellation = self.cancelled.clone();
            work.jobs.push(tokio::spawn(async move {
                if cancellation.load(Ordering::Acquire) {
                    let _ = sender.send(Err(cancelled()));
                    return Ok(());
                }
                match sender.send(operation.await) {
                    Ok(()) | Err(Ok(_)) => Ok(()),
                    Err(Err(error)) => Err(error),
                }
            }));
        }
        let result = receiver.await.map_err(IndexError::other)?;
        self.lock()?.reap_ready();
        result
    }

    pub(super) async fn shutdown(&self) -> Result<(), IndexError> {
        self.cancel();
        self.quiesce().await
    }

    pub(super) async fn quiesce(&self) -> Result<(), IndexError> {
        let _shutdown = self.shutdown_lock.lock().await;
        loop {
            let jobs = std::mem::take(&mut self.lock()?.jobs);
            if jobs.is_empty() {
                break;
            }
            let mut pass = CleanupPass { scope: self, jobs };
            while let Some(job) = pass.jobs.last_mut() {
                let result = job.await;
                drop(pass.jobs.pop());
                self.lock()?.record(result);
            }
        }
        let failures = std::mem::take(&mut self.lock()?.failures);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(IndexError::other(BlockingFailures(failures)))
        }
    }
}

fn cancelled() -> IndexError {
    IndexError::other(std::io::Error::other(
        "computation resource scope is closed",
    ))
}

struct CleanupPass<'a> {
    scope: &'a BlockingScope,
    jobs: Vec<Job>,
}

impl Drop for CleanupPass<'_> {
    fn drop(&mut self) {
        if self.jobs.is_empty() {
            return;
        }
        let mut work = match self.scope.work.lock() {
            Ok(work) => work,
            Err(error) => {
                log::error!("Retaining cleanup handles in a poisoned work registry: {error}");
                error.into_inner()
            }
        };
        work.jobs.append(&mut self.jobs);
    }
}

#[derive(Debug)]
pub(super) struct BlockingFailures(pub(super) Vec<IndexError>);

impl std::fmt::Display for BlockingFailures {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "computation blocking work failed: {:?}", self.0)
    }
}

impl std::error::Error for BlockingFailures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.first().map(|error| error as _)
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_await_and_interrupted_cleanup_keep_actual_job_ownership() {
        let scope = BlockingScope::default();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut operation = Box::pin(scope.run(move || {
            entered_tx.send(()).expect("report blocking-job entry");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(IndexError::other)?;
            Ok(11)
        }));
        tokio::select! {
            result = &mut operation => panic!("blocking job must still be waiting: {result:?}"),
            result = entered_rx => result.expect("job entered"),
        }
        drop(operation);
        let mut cleanup = Box::pin(scope.shutdown());
        assert!(futures::poll!(&mut cleanup).is_pending());
        drop(cleanup);
        assert_eq!(scope.lock().expect("retained registry").jobs.len(), 1);
        release_tx.send(()).expect("release owned blocking job");
        scope.shutdown().await.expect("await actual completion");
        assert!(scope.lock().expect("drained registry").jobs.is_empty());
        assert!(scope.run(|| Ok(())).await.is_err());
        scope
            .shutdown()
            .await
            .expect("idempotent completed cleanup");
    }

    #[tokio::test]
    async fn failure_after_awaiter_cancellation_is_reported_by_cleanup() {
        let scope = BlockingScope::default();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut operation = Box::pin(scope.run(move || {
            entered_tx.send(()).expect("job entry");
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(IndexError::other)?;
            Err::<(), _>(IndexError::CorruptedData)
        }));
        tokio::select! {
            result = &mut operation => panic!("blocking job must still be waiting: {result:?}"),
            result = entered_rx => result.expect("job entered"),
        }
        drop(operation);
        release_tx.send(()).expect("release failed job");
        let error = scope
            .shutdown()
            .await
            .expect_err("retain background failure");
        assert!(error.to_string().contains("CorruptedData"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn registered_async_backend_future_joins_its_original_blocking_task() {
        let scope = BlockingScope::default();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let mut operation = Box::pin(scope.run_async(async move {
            tokio::task::spawn_blocking(move || {
                entered_tx.send(()).expect("original backend task entry");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(IndexError::other)?;
                observed.store(true, Ordering::Release);
                Ok(())
            })
            .await
            .map_err(IndexError::other)?
        }));
        tokio::select! {
            result = &mut operation => panic!("backend task must still be waiting: {result:?}"),
            result = entered_rx => result.expect("original backend task entered"),
        }
        drop(operation);
        let mut cleanup = Box::pin(scope.shutdown());
        assert!(futures::poll!(&mut cleanup).is_pending());
        assert!(!finished.load(Ordering::Acquire));
        release_tx.send(()).expect("release original backend task");
        cleanup.await.expect("join original backend I/O");
        assert!(finished.load(Ordering::Acquire));
    }
}
