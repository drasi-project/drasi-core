// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Cancellation-safe joining of workers retained by their component owner.

use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

#[derive(Debug, thiserror::Error)]
#[error("the previous worker must be joined before another worker can be started")]
pub struct WorkerAlreadyOwned;

/// Acquires ownership before spawning; even a finished worker must be joined first.
pub async fn spawn_owned_worker<F>(
    slot: &RwLock<Option<JoinHandle<F::Output>>>,
    future: F,
) -> Result<(), WorkerAlreadyOwned>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    register_owned_worker(slot, || tokio::spawn(future)).await
}

/// Acquires ownership before submitting blocking work to the runtime.
pub async fn spawn_owned_blocking_worker<F, T>(
    slot: &RwLock<Option<JoinHandle<T>>>,
    work: F,
) -> Result<(), WorkerAlreadyOwned>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    register_owned_worker(slot, || tokio::task::spawn_blocking(work)).await
}

async fn register_owned_worker<T: Send + 'static>(
    slot: &RwLock<Option<JoinHandle<T>>>,
    spawn: impl FnOnce() -> JoinHandle<T> + Send,
) -> Result<(), WorkerAlreadyOwned> {
    let mut slot = slot.write().await;
    if slot.is_some() {
        return Err(WorkerAlreadyOwned);
    }
    *slot = Some(spawn());
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkerCompletion<T> {
    Absent,
    Completed(T),
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerCleanupError {
    #[error("worker did not stop within {timeout:?}; ownership is retained and joining is still required")]
    TimedOut { timeout: Duration },
    #[error("worker failed while joining: {0}")]
    Join(#[source] tokio::task::JoinError),
}

async fn wait<T>(
    task: &mut JoinHandle<T>,
    timeout: Duration,
) -> Result<WorkerCompletion<T>, WorkerCleanupError> {
    match tokio::time::timeout(timeout, &mut *task).await {
        Ok(Ok(value)) => Ok(WorkerCompletion::Completed(value)),
        Ok(Err(error)) if error.is_cancelled() => Ok(WorkerCompletion::Cancelled),
        Ok(Err(error)) => Err(WorkerCleanupError::Join(error)),
        Err(_) => Err(WorkerCleanupError::TimedOut { timeout }),
    }
}

/// Removes a worker only after its exit has been observed.
///
/// Cancellation or timeout retains the handle for the next cleanup attempt.
/// A timeout requests abort, but does not claim that abort has completed.
pub async fn join_owned_worker<T>(
    slot: &mut Option<JoinHandle<T>>,
    timeout: Duration,
) -> Result<WorkerCompletion<T>, WorkerCleanupError> {
    let result = join_owned_worker_gracefully(slot, timeout).await;
    if matches!(result, Err(WorkerCleanupError::TimedOut { .. })) {
        if let Some(task) = slot.as_ref() {
            task.abort();
        }
    }
    result
}

/// Waits without aborting on timeout, retaining the owner for another join.
///
/// Use when the worker must finish draining children or submitted I/O, such as
/// an HTTP server's connections or a processor awaiting a blocking operation.
pub async fn join_owned_worker_gracefully<T>(
    slot: &mut Option<JoinHandle<T>>,
    timeout: Duration,
) -> Result<WorkerCompletion<T>, WorkerCleanupError> {
    let Some(task) = slot.as_mut() else {
        return Ok(WorkerCompletion::Absent);
    };
    let result = wait(task, timeout).await;
    if !matches!(result, Err(WorkerCleanupError::TimedOut { .. })) {
        *slot = None;
    }
    result
}

/// Requests cancellation and retains ownership until the worker has exited.
pub async fn cancel_owned_worker<T>(
    slot: &mut Option<JoinHandle<T>>,
    timeout: Duration,
) -> Result<WorkerCompletion<T>, WorkerCleanupError> {
    if let Some(task) = slot.as_ref() {
        task.abort();
    }
    join_owned_worker(slot, timeout).await
}

/// Cancels and joins forwarders without draining their ownership before awaiting.
///
/// Successfully joined handles are removed immediately, so cancelled cleanup
/// never polls an already-consumed join handle on retry.
pub async fn cancel_owned_workers(
    tasks: &mut Vec<JoinHandle<()>>,
    timeout: Duration,
) -> Result<(), WorkerCleanupError> {
    for task in tasks.iter() {
        task.abort();
    }
    while let Some(task) = tasks.last_mut() {
        let result = wait(task, timeout).await;
        if !matches!(result, Err(WorkerCleanupError::TimedOut { .. })) {
            tasks.pop();
        }
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::sync::{oneshot, Notify};

    struct Exit(Arc<AtomicUsize>);
    impl Drop for Exit {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn registration_rejects_even_a_finished_unjoined_worker() {
        let slot = RwLock::new(None);
        let (finished, completion) = oneshot::channel();
        spawn_owned_worker(&slot, async move { finished.send(()).unwrap() })
            .await
            .unwrap();
        completion.await.unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let new_worker = {
            let starts = starts.clone();
            async move {
                starts.fetch_add(1, Ordering::SeqCst);
            }
        };
        assert!(spawn_owned_worker(&slot, new_worker).await.is_err());
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        join_owned_worker(&mut *slot.write().await, Duration::from_secs(1))
            .await
            .unwrap();
        spawn_owned_worker(&slot, {
            let starts = starts.clone();
            async move {
                starts.fetch_add(1, Ordering::SeqCst);
            }
        })
        .await
        .unwrap();
        join_owned_worker(&mut *slot.write().await, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_registration_does_not_spawn_an_unowned_worker() {
        let slot = RwLock::new(None);
        let locked = slot.write().await;
        let starts = Arc::new(AtomicUsize::new(0));
        {
            let registration = spawn_owned_worker(&slot, {
                let starts = starts.clone();
                async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                }
            });
            tokio::pin!(registration);
            assert!(futures::poll!(&mut registration).is_pending());
        }
        drop(locked);
        assert!(slot.read().await.is_none());
        assert_eq!(starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelled_join_retains_the_worker_until_a_later_join() {
        let release = Arc::new(Notify::new());
        let exited = Arc::new(AtomicUsize::new(0));
        let (started, ready) = oneshot::channel();
        let mut slot = Some(tokio::spawn({
            let release = release.clone();
            let exited = exited.clone();
            async move {
                let _exit = Exit(exited);
                started.send(()).unwrap();
                release.notified().await;
                42
            }
        }));
        ready.await.unwrap();
        {
            let cleanup = join_owned_worker(&mut slot, Duration::from_secs(1));
            tokio::pin!(cleanup);
            assert!(futures::poll!(&mut cleanup).is_pending());
        }
        assert!(slot.is_some());
        assert_eq!(exited.load(Ordering::SeqCst), 0);
        release.notify_one();
        assert_eq!(
            join_owned_worker(&mut slot, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Completed(42),
        );
        assert!(slot.is_none());
        assert_eq!(exited.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_registration_never_submits_before_ownership() {
        let slot = RwLock::new(None);
        let calls = Arc::new(AtomicUsize::new(0));
        let (submitted, submission) = oneshot::channel();
        let locked = slot.write().await;
        {
            let registration = spawn_owned_blocking_worker(&slot, {
                let calls = calls.clone();
                move || {
                    let _ = submitted.send(());
                    calls.fetch_add(1, Ordering::SeqCst)
                }
            });
            tokio::pin!(registration);
            assert!(futures::poll!(&mut registration).is_pending());
        }
        drop(locked);
        assert!(tokio::time::timeout(Duration::from_secs(1), submission)
            .await
            .unwrap()
            .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(slot.read().await.is_none());

        spawn_owned_blocking_worker(&slot, {
            let calls = calls.clone();
            move || calls.fetch_add(1, Ordering::SeqCst)
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !slot.read().await.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            spawn_owned_blocking_worker(&slot, || panic!("replacement must not run"))
                .await
                .is_err()
        );
        assert_eq!(
            join_owned_worker_gracefully(&mut *slot.write().await, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Completed(0),
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timeout_retains_ownership_until_abort_is_joined() {
        let mut slot = Some(tokio::spawn(std::future::pending::<()>()));
        assert!(matches!(
            join_owned_worker(&mut slot, Duration::from_millis(10)).await,
            Err(WorkerCleanupError::TimedOut { .. })
        ));
        assert!(slot.is_some());
        assert_eq!(
            join_owned_worker(&mut slot, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Cancelled,
        );
        assert!(slot.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn graceful_timeout_preserves_the_worker_until_its_children_finish() {
        let (release, drain) = oneshot::channel();
        let mut slot = Some(tokio::spawn(async move {
            drain.await.unwrap();
            42
        }));
        assert!(matches!(
            join_owned_worker_gracefully(&mut slot, Duration::from_millis(10)).await,
            Err(WorkerCleanupError::TimedOut { .. })
        ));
        assert!(!slot.as_ref().unwrap().is_finished());
        release.send(()).unwrap();
        assert_eq!(
            join_owned_worker_gracefully(&mut slot, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Completed(42),
        );
        assert!(slot.is_none());
    }

    #[tokio::test]
    async fn unabortable_work_cannot_be_reported_as_released() {
        let (started, ready) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut slot = Some(tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        }));
        ready.await.unwrap();
        assert!(matches!(
            join_owned_worker(&mut slot, Duration::from_millis(10)).await,
            Err(WorkerCleanupError::TimedOut { .. })
        ));
        assert!(!slot.as_ref().unwrap().is_finished());
        release.send(()).unwrap();
        assert_eq!(
            join_owned_worker(&mut slot, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Completed(()),
        );
        assert!(slot.is_none());
    }

    #[tokio::test]
    async fn panic_is_typed_and_joined_handles_are_never_polled_twice() {
        let mut slot = Some(tokio::spawn(async { panic!("injected worker panic") }));
        assert!(matches!(
            join_owned_worker(&mut slot, Duration::from_secs(1)).await,
            Err(WorkerCleanupError::Join(error)) if error.is_panic()
        ));
        assert!(slot.is_none());
        assert_eq!(
            join_owned_worker(&mut slot, Duration::from_secs(1))
                .await
                .unwrap(),
            WorkerCompletion::Absent,
        );
    }

    #[tokio::test]
    async fn cancelled_forwarders_are_joined_before_releasing_their_handles() {
        let exited = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let (started, ready) = oneshot::channel();
            tasks.push(tokio::spawn({
                let exited = exited.clone();
                async move {
                    let _exit = Exit(exited);
                    started.send(()).unwrap();
                    std::future::pending::<()>().await;
                }
            }));
            ready.await.unwrap();
        }
        cancel_owned_workers(&mut tasks, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(tasks.is_empty());
        assert_eq!(exited.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn interrupted_group_cleanup_retains_unfinished_workers() {
        let (started, ready) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut tasks = vec![tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        })];
        ready.await.unwrap();
        {
            let cleanup = cancel_owned_workers(&mut tasks, Duration::from_secs(1));
            tokio::pin!(cleanup);
            assert!(futures::poll!(&mut cleanup).is_pending());
        }
        assert_eq!(tasks.len(), 1);
        release.send(()).unwrap();
        cancel_owned_workers(&mut tasks, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn group_join_failure_releases_only_the_observed_worker() {
        let (started, ready) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut tasks = vec![tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        })];
        ready.await.unwrap();
        let failed = tokio::spawn(async { panic!("injected forwarder panic") });
        while !failed.is_finished() {
            tokio::task::yield_now().await;
        }
        tasks.push(failed);
        assert!(matches!(
            cancel_owned_workers(&mut tasks, Duration::from_secs(1)).await,
            Err(WorkerCleanupError::Join(error)) if error.is_panic()
        ));
        assert_eq!(tasks.len(), 1);
        release.send(()).unwrap();
        cancel_owned_workers(&mut tasks, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(tasks.is_empty());
    }
}
