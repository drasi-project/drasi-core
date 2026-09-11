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

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, MutexGuard, Weak,
};

use async_trait::async_trait;

use super::IndexError;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

/// Transaction failures carried by the existing `IndexError::Other` variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SessionError {
    #[error("another transaction is active")]
    SessionBusy,
    #[error("transaction outcome requires recovery")]
    SessionFenced,
    #[error("the operation does not belong to an active transaction")]
    InvalidSession,
    #[error("the transaction changed while the operation was pending")]
    StaleCacheGeneration,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RootId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootOutcome {
    Active,
    Committing,
    Committed,
    RolledBack,
    RequiresRebuild,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RollbackSupport {
    Complete,
    None,
}

#[derive(Debug)]
struct RootState {
    outcome: RootOutcome,
    dirty: bool,
    child_active: bool,
}

#[derive(Clone, Debug)]
pub struct RootHandle {
    id: RootId,
    state: Arc<Mutex<RootState>>,
}

impl RootHandle {
    pub fn id(&self) -> RootId {
        self.id
    }

    pub fn outcome(&self) -> RootOutcome {
        match self.state.lock() {
            Ok(state) => state.outcome,
            Err(_) => RootOutcome::Indeterminate,
        }
    }

    /// Recover the receipt after an awaiting commit caller was cancelled.
    pub fn commit_receipt(&self) -> Result<CommitReceipt, IndexError> {
        match self.outcome() {
            RootOutcome::Committed => Ok(CommitReceipt { root: self.clone() }),
            RootOutcome::Active | RootOutcome::Committing => {
                Err(IndexError::other(SessionError::SessionBusy))
            }
            RootOutcome::RolledBack => Err(IndexError::other(SessionError::InvalidSession)),
            RootOutcome::RequiresRebuild | RootOutcome::Indeterminate => {
                Err(IndexError::other(SessionError::SessionFenced))
            }
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, RootState>, IndexError> {
        self.state
            .lock()
            .map_err(|_| IndexError::other(SessionError::SessionFenced))
    }
}

/// One tracker must be shared by the control and all indexes in an index set.
#[derive(Clone, Default)]
pub struct SessionTracker {
    current: Arc<Mutex<Option<RootHandle>>>,
}

type ControlTracker = (Weak<dyn SessionControl>, SessionTracker);
static CONTROL_TRACKERS: Mutex<Vec<ControlTracker>> = Mutex::new(Vec::new());

#[doc(hidden)]
pub fn session_tracker(control: &Arc<dyn SessionControl>) -> Result<SessionTracker, IndexError> {
    if let Some(tracker) = control.root_tracker() {
        return Ok(tracker.clone());
    }
    // Existing control implementations have no tracker field. Keep their
    // sidecar tied to the Arc's lifetime, not a process-wide shared transaction.
    let weak = Arc::downgrade(control);
    let mut trackers = CONTROL_TRACKERS
        .lock()
        .map_err(|_| IndexError::other(SessionError::SessionFenced))?;
    trackers.retain(|(control, _)| control.strong_count() != 0);
    if let Some((_, tracker)) = trackers.iter().find(|(control, _)| control.ptr_eq(&weak)) {
        return Ok(tracker.clone());
    }
    let tracker = SessionTracker::default();
    trackers.push((weak, tracker.clone()));
    Ok(tracker)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheGeneration(Option<(RootId, RootOutcome)>);

impl CacheGeneration {
    pub(crate) fn untracked() -> Self {
        Self(None)
    }
}

impl SessionTracker {
    pub fn current_root(&self) -> Result<Option<RootHandle>, IndexError> {
        Ok(self
            .current
            .lock()
            .map_err(|_| IndexError::other(SessionError::SessionFenced))?
            .clone())
    }

    pub fn cache_generation(&self) -> Result<CacheGeneration, IndexError> {
        match self.current_root()? {
            None => Ok(CacheGeneration(None)),
            Some(root) => match root.outcome() {
                outcome @ (RootOutcome::Active
                | RootOutcome::Committed
                | RootOutcome::RolledBack) => Ok(CacheGeneration(Some((root.id(), outcome)))),
                RootOutcome::Committing => Err(IndexError::other(SessionError::SessionBusy)),
                RootOutcome::RequiresRebuild | RootOutcome::Indeterminate => {
                    Err(IndexError::other(SessionError::SessionFenced))
                }
            },
        }
    }

    pub fn mark_dirty(&self) -> Result<(), IndexError> {
        let root = self
            .current_root()?
            .ok_or(IndexError::other(SessionError::InvalidSession))?;
        let mut state = root.lock()?;
        if state.outcome != RootOutcome::Active {
            return Err(IndexError::other(SessionError::InvalidSession));
        }
        state.dirty = true;
        Ok(())
    }

    /// Run a synchronous backend operation only in the generation that dispatched it.
    pub fn with_generation<T>(
        &self,
        generation: CacheGeneration,
        operation: impl FnOnce() -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        let current = self
            .current
            .lock()
            .map_err(|_| IndexError::other(SessionError::SessionFenced))?;
        let state = current.as_ref().map(RootHandle::lock).transpose()?;
        let actual = CacheGeneration(
            current
                .as_ref()
                .zip(state.as_ref())
                .map(|(root, state)| (root.id(), state.outcome)),
        );
        if actual != generation {
            return Err(IndexError::other(SessionError::StaleCacheGeneration));
        }
        operation()
    }

    fn begin(&self) -> Result<RootHandle, IndexError> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| IndexError::other(SessionError::SessionFenced))?;
        if let Some(root) = current.as_ref() {
            match root.outcome() {
                RootOutcome::Active | RootOutcome::Committing => {
                    return Err(IndexError::other(SessionError::SessionBusy))
                }
                RootOutcome::RequiresRebuild | RootOutcome::Indeterminate => {
                    return Err(IndexError::other(SessionError::SessionFenced));
                }
                RootOutcome::Committed | RootOutcome::RolledBack => {}
            }
        }
        let root = RootHandle {
            id: RootId(NEXT_ROOT.fetch_add(1, Ordering::Relaxed)),
            state: Arc::new(Mutex::new(RootState {
                outcome: RootOutcome::Active,
                dirty: false,
                child_active: false,
            })),
        };
        *current = Some(root.clone());
        Ok(root)
    }
}

/// Backend lifecycle control for session-scoped transactions.
#[async_trait]
pub trait SessionControl: Send + Sync {
    async fn begin(&self) -> Result<(), IndexError>;
    async fn commit(&self) -> Result<(), IndexError>;
    fn rollback(&self) -> Result<(), IndexError>;

    #[doc(hidden)]
    fn root_tracker(&self) -> Option<&SessionTracker> {
        None
    }

    #[doc(hidden)]
    fn rollback_support(&self) -> RollbackSupport {
        RollbackSupport::None
    }

    #[doc(hidden)]
    fn commit_failure_outcome(&self) -> RootOutcome {
        RootOutcome::Indeterminate
    }
}

/// No-op lifecycle control for nontransactional memory indexes.
#[derive(Default)]
pub struct NoOpSessionControl;

#[async_trait]
impl SessionControl for NoOpSessionControl {
    fn rollback_support(&self) -> RollbackSupport {
        RollbackSupport::None
    }

    async fn begin(&self) -> Result<(), IndexError> {
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        Ok(())
    }
}

/// Only a successful root commit can construct a receipt.
#[derive(Debug)]
pub struct CommitReceipt {
    root: RootHandle,
}

impl CommitReceipt {
    pub fn root_id(&self) -> RootId {
        self.root.id()
    }
}

#[derive(Debug)]
pub struct Provisional<T> {
    root: RootHandle,
    value: T,
}

impl<T> Provisional<T> {
    pub fn root_id(&self) -> RootId {
        self.root.id()
    }

    pub fn into_committed(self, receipt: &CommitReceipt) -> Result<T, IndexError> {
        if self.root.id() != receipt.root_id() || self.root.outcome() != RootOutcome::Committed {
            return Err(IndexError::other(SessionError::InvalidSession));
        }
        Ok(self.value)
    }
}

pub struct SessionGuard {
    control: Arc<dyn SessionControl>,
    root: RootHandle,
}

impl SessionGuard {
    pub async fn begin(control: Arc<dyn SessionControl>) -> Result<Self, IndexError> {
        let root = session_tracker(&control)?.begin()?;
        let guard = Self { control, root };
        if let Err(error) = guard.control.begin().await {
            if error.session_error() == Some(SessionError::SessionFenced) {
                guard.root.lock()?.outcome = RootOutcome::Indeterminate;
            }
            return Err(error);
        }
        Ok(guard)
    }

    pub fn root(&self) -> RootHandle {
        self.root.clone()
    }

    pub fn mark_dirty(&self) -> Result<(), IndexError> {
        let mut state = self.root.lock()?;
        if state.outcome != RootOutcome::Active {
            return Err(IndexError::other(SessionError::InvalidSession));
        }
        state.dirty = true;
        Ok(())
    }

    /// Abort this root and expose its outcome or the backend rollback error.
    ///
    /// Only `RolledBack` means complete restoration. Dirty nontransactional
    /// state returns `RequiresRebuild` even if backend cleanup itself succeeded.
    pub fn rollback(self) -> Result<RootOutcome, IndexError> {
        self.abort()?;
        Ok(self.root.outcome())
    }

    pub fn child<'a>(
        &'a mut self,
        control: &Arc<dyn SessionControl>,
    ) -> Result<ChildSession<'a>, IndexError> {
        if !Arc::ptr_eq(
            &session_tracker(&self.control)?.current,
            &session_tracker(control)?.current,
        ) {
            return Err(IndexError::other(SessionError::InvalidSession));
        }
        {
            let mut state = self.root.lock()?;
            if state.outcome != RootOutcome::Active || state.child_active {
                return Err(IndexError::other(SessionError::InvalidSession));
            }
            state.child_active = true;
        }
        Ok(ChildSession {
            parent: self,
            completed: false,
        })
    }

    pub async fn commit(self) -> Result<(), IndexError> {
        self.commit_with_receipt().await.map(|_| ())
    }

    pub async fn commit_with_receipt(self) -> Result<CommitReceipt, IndexError> {
        {
            let mut state = self.root.lock()?;
            if state.outcome != RootOutcome::Active || state.child_active {
                return Err(IndexError::other(SessionError::InvalidSession));
            }
            state.outcome = RootOutcome::Committing;
        }
        let commit = async move {
            let finalizer = CommitFinalizer { guard: self };
            let result = finalizer.guard.control.commit().await;
            let mut cleanup_failed = false;
            if result.is_err() {
                if let Err(error) = finalizer.guard.control.rollback() {
                    log::error!("Failed commit cleanup failed: {error}");
                    cleanup_failed = true;
                }
            }
            let rollback_complete = !cleanup_failed
                && (!finalizer.guard.root.lock()?.dirty
                    || finalizer.guard.control.rollback_support() == RollbackSupport::Complete);
            let outcome = match &result {
                Ok(()) => RootOutcome::Committed,
                Err(_) => match finalizer.guard.control.commit_failure_outcome() {
                    RootOutcome::RolledBack if rollback_complete => RootOutcome::RolledBack,
                    RootOutcome::RequiresRebuild | RootOutcome::RolledBack => {
                        RootOutcome::RequiresRebuild
                    }
                    RootOutcome::Active
                    | RootOutcome::Committing
                    | RootOutcome::Committed
                    | RootOutcome::Indeterminate => RootOutcome::Indeterminate,
                },
            };
            finalizer.guard.root.lock()?.outcome = outcome;
            result?;
            finalizer.guard.root.commit_receipt()
        };
        match tokio::runtime::Handle::try_current() {
            // Keep dispatched commits alive after caller cancellation on Tokio.
            Ok(runtime) => runtime.spawn(commit).await.map_err(IndexError::other)?,
            // Other executors poll inline; cancellation leaves an indeterminate root.
            Err(_) => commit.await,
        }
    }

    fn abort(&self) -> Result<(), IndexError> {
        let mut state = self.root.lock()?;
        if state.outcome != RootOutcome::Active {
            return Ok(());
        }
        let rollback = self.control.rollback();
        state.outcome = match (&rollback, state.dirty, self.control.rollback_support()) {
            (Ok(()), false, _) | (Ok(()), true, RollbackSupport::Complete) => {
                RootOutcome::RolledBack
            }
            _ => RootOutcome::RequiresRebuild,
        };
        rollback
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Err(error) = self.abort() {
            log::error!("Root {:?} rollback failed: {error}", self.root.id());
        }
    }
}

struct CommitFinalizer {
    guard: SessionGuard,
}

impl Drop for CommitFinalizer {
    fn drop(&mut self) {
        if let Ok(mut state) = self.guard.root.lock() {
            if state.outcome == RootOutcome::Committing {
                state.outcome = RootOutcome::Indeterminate;
            }
        }
    }
}

/// A mutable parent borrow prevents committing while a child is still running.
pub struct ChildSession<'a> {
    parent: &'a mut SessionGuard,
    completed: bool,
}

impl ChildSession<'_> {
    pub fn mark_dirty(&self) -> Result<(), IndexError> {
        self.parent.mark_dirty()
    }

    pub fn complete<T>(mut self, value: T) -> Result<Provisional<T>, IndexError> {
        {
            let mut state = self.parent.root.lock()?;
            if state.outcome != RootOutcome::Active {
                return Err(IndexError::other(SessionError::InvalidSession));
            }
            state.child_active = false;
        }
        self.completed = true;
        Ok(Provisional {
            root: self.parent.root(),
            value,
        })
    }
}

impl Drop for ChildSession<'_> {
    fn drop(&mut self) {
        if !self.completed {
            if let Err(error) = self.parent.abort() {
                log::error!("Child rollback failed: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct MockSessionControl {
        tracker: SessionTracker,
        calls: Mutex<Vec<&'static str>>,
        fail_commit: bool,
        fail_rollback: bool,
        nontransactional: bool,
        pause_commit: bool,
        dispatched: Notify,
        finish: Notify,
    }

    #[async_trait]
    impl SessionControl for MockSessionControl {
        fn root_tracker(&self) -> Option<&SessionTracker> {
            Some(&self.tracker)
        }

        fn rollback_support(&self) -> RollbackSupport {
            if self.nontransactional {
                RollbackSupport::None
            } else {
                RollbackSupport::Complete
            }
        }

        async fn begin(&self) -> Result<(), IndexError> {
            self.calls.lock().unwrap().push("begin");
            Ok(())
        }

        async fn commit(&self) -> Result<(), IndexError> {
            self.calls.lock().unwrap().push("commit");
            self.dispatched.notify_one();
            if self.pause_commit {
                self.finish.notified().await;
            }
            if self.fail_commit {
                Err(IndexError::CorruptedData)
            } else {
                Ok(())
            }
        }

        fn rollback(&self) -> Result<(), IndexError> {
            self.calls.lock().unwrap().push("rollback");
            if self.fail_rollback {
                Err(IndexError::IOError)
            } else {
                Ok(())
            }
        }

        fn commit_failure_outcome(&self) -> RootOutcome {
            RootOutcome::RolledBack
        }
    }

    #[tokio::test]
    async fn begin_calls_control_begin() {
        let control = Arc::new(MockSessionControl::default());
        let _guard = SessionGuard::begin(control.clone()).await.unwrap();
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin"]);
    }

    #[tokio::test]
    async fn commit_suppresses_rollback_on_drop() {
        let control = Arc::new(MockSessionControl::default());
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let root = guard.root();
        let receipt = guard.commit_with_receipt().await.unwrap();
        assert_eq!(receipt.root_id(), root.id());
        assert_eq!(root.outcome(), RootOutcome::Committed);
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "commit"]);
    }

    #[tokio::test]
    async fn drop_without_commit_triggers_rollback() {
        let control = Arc::new(MockSessionControl::default());
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let root = guard.root();
        drop(guard);
        assert_eq!(root.outcome(), RootOutcome::RolledBack);
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "rollback"]);
    }

    #[tokio::test]
    async fn explicit_rollback_reports_outcome_and_runs_once() {
        let control = Arc::new(MockSessionControl::default());
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        guard.mark_dirty().unwrap();
        assert_eq!(guard.rollback().unwrap(), RootOutcome::RolledBack);
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "rollback"]);
        SessionGuard::begin(control)
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn explicit_rollback_propagates_cleanup_failure_and_fences() {
        let control = Arc::new(MockSessionControl {
            fail_rollback: true,
            ..Default::default()
        });
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let root = guard.root();
        assert!(matches!(guard.rollback(), Err(IndexError::IOError)));
        assert_eq!(root.outcome(), RootOutcome::RequiresRebuild);
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "rollback"]);
        assert!(matches!(
            SessionGuard::begin(control).await,
            Err(error) if error.session_error() == Some(SessionError::SessionFenced)
        ));
    }

    #[tokio::test]
    async fn explicit_dirty_memory_rollback_reports_rebuild() {
        let guard = SessionGuard::begin(Arc::new(NoOpSessionControl))
            .await
            .unwrap();
        guard.mark_dirty().unwrap();
        assert_eq!(guard.rollback().unwrap(), RootOutcome::RequiresRebuild);
    }

    #[tokio::test]
    async fn explicit_stale_rollback_does_not_abort_a_new_root() {
        let control: Arc<dyn SessionControl> = Arc::new(MockSessionControl::default());
        let mut old = SessionGuard::begin(control.clone()).await.unwrap();
        drop(old.child(&control).unwrap());
        let newer = SessionGuard::begin(control).await.unwrap();
        assert_eq!(old.rollback().unwrap(), RootOutcome::RolledBack);
        assert_eq!(newer.root().outcome(), RootOutcome::Active);
        newer.commit().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_commit_before_dispatch_rolls_back() {
        let control = Arc::new(MockSessionControl::default());
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let root = guard.root();
        drop(guard.commit());
        assert_eq!(root.outcome(), RootOutcome::RolledBack);
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "rollback"]);
    }

    #[tokio::test]
    async fn failed_commit_still_triggers_rollback() {
        let control = Arc::new(MockSessionControl {
            fail_commit: true,
            ..Default::default()
        });
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let root = guard.root();
        assert!(guard.commit().await.is_err());
        assert_eq!(root.outcome(), RootOutcome::RolledBack);
        assert_eq!(
            *control.calls.lock().unwrap(),
            vec!["begin", "commit", "rollback"]
        );
    }

    #[tokio::test]
    async fn nontransactional_failed_commit_cannot_claim_complete_rollback() {
        let control = Arc::new(MockSessionControl {
            fail_commit: true,
            nontransactional: true,
            ..Default::default()
        });
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        guard.mark_dirty().unwrap();
        let root = guard.root();
        assert!(guard.commit().await.is_err());
        assert_eq!(root.outcome(), RootOutcome::RequiresRebuild);
        assert!(matches!(
            SessionGuard::begin(control).await,
            Err(error) if error.session_error() == Some(SessionError::SessionFenced)
        ));
    }

    #[tokio::test]
    async fn several_children_remain_provisional_until_matching_receipt() {
        let control: Arc<dyn SessionControl> = Arc::new(MockSessionControl::default());
        let mut outer = SessionGuard::begin(control.clone()).await.unwrap();
        let first = outer.child(&control).unwrap().complete(1).unwrap();
        let second = outer.child(&control).unwrap().complete(2).unwrap();
        assert_eq!(outer.root().outcome(), RootOutcome::Active);
        assert!(matches!(
            SessionGuard::begin(control.clone()).await,
            Err(error) if error.session_error() == Some(SessionError::SessionBusy)
        ));
        let receipt = outer.commit_with_receipt().await.unwrap();
        assert_eq!(first.into_committed(&receipt).unwrap(), 1);
        assert_eq!(second.into_committed(&receipt).unwrap(), 2);
    }

    #[tokio::test]
    async fn stale_guard_cannot_abort_new_root_or_release_rolled_back_output() {
        let control: Arc<dyn SessionControl> = Arc::new(MockSessionControl::default());
        let mut old = SessionGuard::begin(control.clone()).await.unwrap();
        let provisional = old.child(&control).unwrap().complete(1).unwrap();
        drop(old.child(&control).unwrap());
        assert_eq!(old.root().outcome(), RootOutcome::RolledBack);
        let newer = SessionGuard::begin(control.clone()).await.unwrap();
        let root = newer.root();
        drop(old);
        assert_eq!(root.outcome(), RootOutcome::Active);
        let receipt = newer.commit_with_receipt().await.unwrap();
        assert!(matches!(
            provisional.into_committed(&receipt),
            Err(error) if error.session_error() == Some(SessionError::InvalidSession)
        ));
    }

    #[tokio::test]
    async fn child_rejects_another_index_set() {
        let control: Arc<dyn SessionControl> = Arc::new(MockSessionControl::default());
        let other: Arc<dyn SessionControl> = Arc::new(MockSessionControl::default());
        let mut guard = SessionGuard::begin(control).await.unwrap();
        assert!(matches!(
            guard.child(&other),
            Err(error) if error.session_error() == Some(SessionError::InvalidSession)
        ));
    }

    #[tokio::test]
    async fn clean_memory_abort_allows_retry_but_dirty_abort_fences() {
        let control = Arc::new(NoOpSessionControl);
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        let clean = guard.root();
        drop(guard);
        assert_eq!(clean.outcome(), RootOutcome::RolledBack);
        let guard = SessionGuard::begin(control.clone()).await.unwrap();
        guard.mark_dirty().unwrap();
        let dirty = guard.root();
        drop(guard);
        assert_eq!(dirty.outcome(), RootOutcome::RequiresRebuild);
        assert!(matches!(
            SessionGuard::begin(control).await,
            Err(error) if error.session_error() == Some(SessionError::SessionFenced)
        ));
    }

    #[tokio::test]
    async fn cancellation_after_dispatch_leaves_owned_finalizer_in_charge() {
        let control = Arc::new(MockSessionControl {
            pause_commit: true,
            ..Default::default()
        });
        let mut guard = SessionGuard::begin(control.clone()).await.unwrap();
        let shared_control: Arc<dyn SessionControl> = control.clone();
        let provisional = guard.child(&shared_control).unwrap().complete(42).unwrap();
        let root = guard.root();
        let caller = tokio::spawn(guard.commit());
        control.dispatched.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(root.outcome(), RootOutcome::Committing);
        assert!(matches!(
            root.commit_receipt(),
            Err(error) if error.session_error() == Some(SessionError::SessionBusy)
        ));
        assert!(matches!(
            SessionGuard::begin(control.clone()).await,
            Err(error) if error.session_error() == Some(SessionError::SessionBusy)
        ));
        control.finish.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while root.outcome() == RootOutcome::Committing {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(root.outcome(), RootOutcome::Committed);
        assert_eq!(
            provisional
                .into_committed(&root.commit_receipt().unwrap())
                .unwrap(),
            42
        );
        assert_eq!(*control.calls.lock().unwrap(), vec!["begin", "commit"]);
        drop(SessionGuard::begin(control).await.unwrap());
    }
}
