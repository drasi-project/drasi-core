// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Accounting for the opt-in, finite feedback drain.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    task::Poll,
};
use tokio::sync::Notify;

use super::PipeError;

tokio::task_local! {
    static PROCESSING: Arc<Activity>;
    static FEEDBACK: ();
}

#[derive(Default)]
struct State {
    epoch: u64,
    active: usize,
    draining: bool,
    sealed: bool,
}

#[derive(Default)]
pub(crate) struct Activity {
    state: Mutex<State>,
    pub(crate) changed: Notify,
}

pub(crate) struct Work(Arc<Activity>);

impl Drop for Work {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("feedback activity poisoned");
        state.active -= 1;
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("feedback epoch exhausted");
        drop(state);
        self.0.changed.notify_waiters();
    }
}

impl Activity {
    pub(crate) async fn scope<T>(self: &Arc<Self>, future: impl Future<Output = T>) -> T {
        PROCESSING.scope(self.clone(), future).await
    }

    pub(crate) fn current() -> Option<Arc<Self>> {
        PROCESSING.try_with(Clone::clone).ok()
    }

    pub(crate) fn is_current(&self) -> bool {
        PROCESSING
            .try_with(|current| std::ptr::eq(current.as_ref(), self))
            .unwrap_or(false)
    }

    pub(crate) fn feedback_requested() -> bool {
        FEEDBACK.try_with(|_| ()).is_ok()
    }

    fn authorized(&self, state: &State) -> Result<(), PipeError> {
        let internal = Self::feedback_requested();
        if internal
            && !PROCESSING
                .try_with(|current| std::ptr::eq(current.as_ref(), self))
                .unwrap_or(false)
        {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "feedback admission belongs to another graph"
            )));
        }
        if state.sealed || state.draining && !internal {
            return Err(PipeError::Closed);
        }
        Ok(())
    }

    pub(crate) fn check_admission(&self) -> Result<(), PipeError> {
        self.authorized(&self.state.lock().expect("feedback activity poisoned"))
    }

    pub(crate) fn request(self: &Arc<Self>) -> Result<Work, PipeError> {
        let mut state = self.state.lock().expect("feedback activity poisoned");
        self.authorized(&state)?;
        Ok(self.add(&mut state))
    }

    fn add(self: &Arc<Self>, state: &mut State) -> Work {
        state.active = state
            .active
            .checked_add(1)
            .expect("feedback work exhausted");
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("feedback epoch exhausted");
        Work(self.clone())
    }

    pub(crate) fn enter(self: &Arc<Self>, scheduled: bool) -> Option<Work> {
        let mut state = self.state.lock().expect("feedback activity poisoned");
        if state.sealed || scheduled && state.draining {
            return None;
        }
        Some(self.add(&mut state))
    }

    /// Register before polling, not after dequeue. Pending polls release their
    /// reservation; a ready poll transfers it with its output until acknowledgement.
    /// If sealing wins, the underlying future is never polled again.
    pub(crate) async fn poll_work<T>(
        self: &Arc<Self>,
        scheduled: bool,
        future: impl Future<Output = T>,
    ) -> (T, Work) {
        tokio::pin!(future);
        futures::future::poll_fn(|cx| {
            let Some(work) = self.enter(scheduled) else {
                return Poll::Pending;
            };
            match future.as_mut().poll(cx) {
                Poll::Ready(value) => Poll::Ready((value, work)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
    }

    pub(crate) fn begin(&self) {
        self.state
            .lock()
            .expect("feedback activity poisoned")
            .draining = true;
        self.changed.notify_waiters();
    }

    pub(crate) fn draining(&self) -> bool {
        self.state
            .lock()
            .expect("feedback activity poisoned")
            .draining
    }

    pub(crate) async fn wait_draining(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.draining() {
                return;
            }
            changed.await;
        }
    }

    pub(crate) fn idle_epoch(&self) -> Option<u64> {
        let state = self.state.lock().expect("feedback activity poisoned");
        (state.active == 0).then_some(state.epoch)
    }

    pub(crate) fn seal(&self, epoch: u64) -> bool {
        let mut state = self.state.lock().expect("feedback activity poisoned");
        if state.active != 0 || state.epoch != epoch {
            return false;
        }
        state.sealed = true;
        true
    }

    pub(crate) fn resume(&self) {
        let mut state = self.state.lock().expect("feedback activity poisoned");
        state.draining = false;
        state.sealed = false;
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("feedback epoch exhausted");
        drop(state);
        self.changed.notify_waiters();
    }
}

pub(crate) async fn feedback<T>(
    future: impl Future<Output = Result<T, PipeError>>,
) -> Result<T, PipeError> {
    if Activity::current().is_none() {
        return Err(PipeError::Backend(anyhow::anyhow!(
            "feedback admission requires an opted-in graph processing callback"
        )));
    }
    FEEDBACK.scope((), future).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn cut_wins_before_dequeue_without_polling_or_consuming_input() {
        let activity = Arc::new(Activity::default());
        activity.begin();
        assert!(activity.seal(activity.idle_epoch().unwrap()));
        let polls = AtomicUsize::new(0);
        let read = activity.poll_work(false, async {
            polls.fetch_add(1, Ordering::SeqCst);
            "retained input"
        });
        tokio::pin!(read);
        assert!(futures::poll!(&mut read).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        activity.resume();
        let (value, work) = read.await;
        assert_eq!(value, "retained input");
        assert!(activity.idle_epoch().is_none());
        drop(work);
        assert!(activity.idle_epoch().is_some());
    }

    #[tokio::test]
    async fn dequeue_wins_and_owns_work_until_acknowledgement() {
        let activity = Arc::new(Activity::default());
        let epoch = activity.idle_epoch().unwrap();
        let (value, work) = activity
            .poll_work(false, async {
                assert!(
                    !activity.seal(epoch),
                    "the cut must lose even inside the dequeue poll"
                );
                1
            })
            .await;
        assert_eq!(value, 1);
        activity.begin();
        assert!(!activity.seal(epoch));
        assert!(activity.idle_epoch().is_none());
        drop(work);
        assert!(
            !activity.seal(epoch),
            "a stale idle sample cannot authorize sealing"
        );
        assert!(activity.seal(activity.idle_epoch().unwrap()));
    }

    #[tokio::test]
    async fn scheduled_work_is_fenced_before_poll_and_not_lost() {
        let activity = Arc::new(Activity::default());
        let polls = AtomicUsize::new(0);
        activity.begin();
        let wake = activity.poll_work(true, async {
            polls.fetch_add(1, Ordering::SeqCst);
        });
        tokio::pin!(wake);
        assert!(futures::poll!(&mut wake).is_pending());
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert!(activity.idle_epoch().is_some());
        activity.resume();
        let (_, work) = wake.await;
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        drop(work);
    }

    #[tokio::test]
    async fn pending_poll_and_cancelled_future_leave_no_phantom_work() {
        let activity = Arc::new(Activity::default());
        {
            let read = activity.poll_work(false, std::future::pending::<()>());
            tokio::pin!(read);
            assert!(futures::poll!(&mut read).is_pending());
            assert!(activity.idle_epoch().is_some());
        }
        activity.begin();
        assert!(activity.seal(activity.idle_epoch().unwrap()));
    }

    #[tokio::test]
    async fn feedback_is_not_external_cross_graph_or_inherited_by_spawn() {
        let activity = Arc::new(Activity::default());
        let other = Arc::new(Activity::default());
        assert!(feedback(async { activity.check_admission() })
            .await
            .is_err());
        activity
            .scope(async {
                activity.begin();
                assert!(matches!(activity.request(), Err(PipeError::Closed)));
                feedback(async {
                    let work = activity.request()?;
                    assert!(activity.idle_epoch().is_none());
                    assert!(other.request().is_err());
                    assert!(tokio::spawn(async { feedback(async { Ok(()) }).await })
                        .await
                        .unwrap()
                        .is_err());
                    drop(work);
                    Ok(())
                })
                .await
                .unwrap();
                assert!(activity.seal(activity.idle_epoch().unwrap()));
                assert!(feedback(async { activity.check_admission() })
                    .await
                    .is_err());
            })
            .await;
    }
}
