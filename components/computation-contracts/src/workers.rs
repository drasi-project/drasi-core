// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Cancellation-safe joining of workers retained by their component owner.

use std::{
    cell::RefCell,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Receives the bounded message of a panic in an owned worker.
///
/// The owner of the work that registers a worker (for a native plugin, the SDK
/// instance whose operation is being polled) installs it with
/// [`observe_worker_panics`]. It runs on the panicking worker's thread after
/// the unwind was caught and must not block.
pub type WorkerPanicObserver = Arc<dyn Fn(&str) + Send + Sync>;

thread_local! {
    static OBSERVER: RefCell<Option<WorkerPanicObserver>> = const { RefCell::new(None) };
}

/// Restores the previously installed observer when dropped.
#[must_use = "the observer is installed only while the scope is alive"]
pub struct WorkerPanicScope {
    previous: Option<WorkerPanicObserver>,
}

impl Drop for WorkerPanicScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = OBSERVER.try_with(|current| *current.borrow_mut() = previous);
    }
}

/// Installs `observer` on this thread while the returned scope is alive.
///
/// Every worker registered through this module while the scope is active
/// captures the observer, and so does every worker those workers register in
/// turn. When such a worker panics, the observer receives the panic message
/// before the unwind continues, so joining the worker still reports
/// [`WorkerCleanupError::Join`] with a panic. Without an observer a worker
/// panic is seen only when the worker is joined.
pub fn observe_worker_panics(observer: WorkerPanicObserver) -> WorkerPanicScope {
    WorkerPanicScope {
        previous: OBSERVER
            .try_with(|current| current.borrow_mut().replace(observer))
            .ok()
            .flatten(),
    }
}

fn current_observer() -> Option<WorkerPanicObserver> {
    OBSERVER
        .try_with(|current| current.borrow().clone())
        .ok()
        .flatten()
}

fn enter(observer: &Option<WorkerPanicObserver>) -> Option<WorkerPanicScope> {
    observer.clone().map(observe_worker_panics)
}

/// A bounded, owned description of a panic payload.
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

fn report(observer: &Option<WorkerPanicObserver>, payload: Box<dyn std::any::Any + Send>) -> ! {
    if let Some(observer) = observer {
        let message = panic_message(payload.as_ref());
        // A failing observer must not replace the original panic.
        let _ = catch_unwind(AssertUnwindSafe(|| observer(&message)));
    }
    resume_unwind(payload)
}

/// Reports a panic of the wrapped worker to the observer captured at registration.
struct Observed<F> {
    future: Pin<Box<F>>,
    observer: Option<WorkerPanicObserver>,
}

impl<F: Future> Future for Observed<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _scope = enter(&this.observer);
        match catch_unwind(AssertUnwindSafe(|| this.future.as_mut().poll(context))) {
            Ok(poll) => poll,
            Err(payload) => report(&this.observer, payload),
        }
    }
}

fn observed<F: Future>(future: F) -> Observed<F> {
    Observed {
        future: Box::pin(future),
        observer: current_observer(),
    }
}

fn observed_blocking<F, T>(work: F) -> impl FnOnce() -> T
where
    F: FnOnce() -> T,
{
    let observer = current_observer();
    move || {
        let _scope = enter(&observer);
        match catch_unwind(AssertUnwindSafe(work)) {
            Ok(value) => value,
            Err(payload) => report(&observer, payload),
        }
    }
}

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
    let future = observed(future);
    register_owned_worker(slot, || tokio::spawn(future)).await
}

/// Registers in an already available ownership slot without suspending.
///
/// Intended for an owner's short registration/commit section. A busy slot is
/// rejected before work is submitted, just like an occupied one.
pub fn try_spawn_owned_worker<F>(
    slot: &RwLock<Option<JoinHandle<F::Output>>>,
    future: F,
) -> Result<(), WorkerAlreadyOwned>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut slot = slot.try_write().map_err(|_| WorkerAlreadyOwned)?;
    if slot.is_some() {
        return Err(WorkerAlreadyOwned);
    }
    *slot = Some(tokio::spawn(observed(future)));
    Ok(())
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
    let work = observed_blocking(work);
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

/// Upper bound for observing an abort requested after a worker ignored its
/// cooperative stop. Async workers normally exit at their next suspension.
const ABORT_JOIN_GRACE: Duration = Duration::from_secs(1);

/// Removes a worker only after its exit has been observed.
///
/// A worker that does not stop within `timeout` is aborted, and the abort is
/// joined within a bounded grace so released resources are actually dropped
/// when this returns. Cancellation, or work that cannot observe the abort
/// (such as blocking work), retains the handle for the next cleanup attempt.
pub async fn join_owned_worker<T>(
    slot: &mut Option<JoinHandle<T>>,
    timeout: Duration,
) -> Result<WorkerCompletion<T>, WorkerCleanupError> {
    let result = join_owned_worker_gracefully(slot, timeout).await;
    if !matches!(result, Err(WorkerCleanupError::TimedOut { .. })) {
        return result;
    }
    let Some(task) = slot.as_mut() else {
        return result;
    };
    task.abort();
    let joined = wait(task, timeout.min(ABORT_JOIN_GRACE)).await;
    if matches!(joined, Err(WorkerCleanupError::TimedOut { .. })) {
        return result;
    }
    *slot = None;
    joined
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
    async fn timeout_aborts_and_joins_a_worker_before_releasing_it() {
        let exits = Arc::new(AtomicUsize::new(0));
        let exit = Exit(exits.clone());
        let mut slot = Some(tokio::spawn(async move {
            let _exit = exit;
            std::future::pending::<()>().await;
        }));
        assert_eq!(
            join_owned_worker(&mut slot, Duration::from_millis(10))
                .await
                .unwrap(),
            WorkerCompletion::Cancelled,
        );
        assert!(slot.is_none());
        assert_eq!(
            exits.load(Ordering::SeqCst),
            1,
            "aborted worker must be joined"
        );
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

    fn recording_observer() -> (WorkerPanicObserver, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observer: WorkerPanicObserver = {
            let seen = seen.clone();
            Arc::new(move |message: &str| seen.lock().unwrap().push(message.to_owned()))
        };
        (observer, seen)
    }

    async fn join_panic<T: std::fmt::Debug>(slot: &RwLock<Option<JoinHandle<T>>>) {
        assert!(matches!(
            join_owned_worker(&mut *slot.write().await, Duration::from_secs(5)).await,
            Err(WorkerCleanupError::Join(error)) if error.is_panic()
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn worker_panic_reaches_the_observer_installed_at_registration() {
        let (observer, seen) = recording_observer();
        let slot = RwLock::new(None);
        let (release, wait) = oneshot::channel::<()>();
        {
            let _scope = observe_worker_panics(observer);
            spawn_owned_worker(&slot, async move {
                wait.await.unwrap();
                panic!("injected worker panic");
            })
            .await
            .unwrap();
        }
        // The observer was captured when the worker was registered, not
        // looked up when it panics.
        assert!(seen.lock().unwrap().is_empty());
        release.send(()).unwrap();
        join_panic(&slot).await;
        assert_eq!(*seen.lock().unwrap(), ["injected worker panic"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn workers_registered_by_an_observed_worker_inherit_its_observer() {
        let (observer, seen) = recording_observer();
        let outer = Arc::new(RwLock::new(None));
        let inner = Arc::new(RwLock::new(None::<JoinHandle<()>>));
        {
            let _scope = observe_worker_panics(observer);
            let inner = inner.clone();
            spawn_owned_worker(&outer, async move {
                spawn_owned_worker(&inner, async { panic!("nested worker panic") })
                    .await
                    .unwrap();
                try_spawn_owned_worker(&RwLock::new(None), async {}).unwrap();
            })
            .await
            .unwrap();
        }
        join_owned_worker(&mut *outer.write().await, Duration::from_secs(5))
            .await
            .unwrap();
        join_panic(&inner).await;
        assert_eq!(*seen.lock().unwrap(), ["nested worker panic"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_worker_panic_reaches_the_observer() {
        let (observer, seen) = recording_observer();
        let slot = RwLock::new(None);
        {
            let _scope = observe_worker_panics(observer);
            spawn_owned_blocking_worker::<_, ()>(&slot, || panic!("blocking worker panic"))
                .await
                .unwrap();
        }
        join_panic(&slot).await;
        assert_eq!(*seen.lock().unwrap(), ["blocking worker panic"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn workers_registered_outside_a_scope_are_unobserved() {
        let (observer, seen) = recording_observer();
        drop(observe_worker_panics(observer));
        let slot = RwLock::new(None);
        try_spawn_owned_worker(&slot, async { panic!("unobserved worker panic") }).unwrap();
        join_panic(&slot).await;
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn panic_messages_are_bounded_on_a_character_boundary() {
        let long = "é".repeat(2000);
        let message = panic_message(&long);
        assert!(message.len() <= 2048);
        assert!(long.starts_with(&message));
        assert_eq!(panic_message(&7_u8), "non-string panic payload");
    }
}
