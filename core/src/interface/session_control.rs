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

use std::sync::Arc;

use async_trait::async_trait;

use super::IndexError;

/// Backend-implemented lifecycle control for session-scoped transactions.
///
/// Implementations manage the begin/commit/rollback lifecycle for atomic
/// writes across all indexes during a single source-change processing.
/// Session state lives inside backend implementations, shared via `Arc`.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Begin a new session-scoped transaction.
    async fn begin(&self) -> Result<(), IndexError>;

    /// Commit the current session-scoped transaction.
    async fn commit(&self) -> Result<(), IndexError>;

    /// Roll back the current session-scoped transaction.
    ///
    /// This is synchronous to be safe for use in `Drop` implementations.
    /// Returns an error if the rollback itself fails (e.g., mutex poisoned).
    fn rollback(&self) -> Result<(), IndexError>;
}

/// No-op implementation of `SessionControl`.
///
/// All methods are no-ops that return `Ok(())`. Used for in-memory backends
/// and as the default when no session control is configured.
pub struct NoOpSessionControl;

#[async_trait]
impl SessionControl for NoOpSessionControl {
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

/// RAII guard for session-scoped transactions.
///
/// Created via `SessionGuard::begin()`, which calls `control.begin()`.
/// On drop, automatically calls `control.rollback()` unless `commit()`
/// was called first.
pub struct SessionGuard {
    control: Arc<dyn SessionControl>,
    committed: bool,
}

impl SessionGuard {
    /// Begin a new session, returning an RAII guard.
    ///
    /// Calls `control.begin()` and returns a guard that will automatically
    /// roll back on drop unless `commit()` is called.
    pub async fn begin(control: Arc<dyn SessionControl>) -> Result<Self, IndexError> {
        control.begin().await?;
        Ok(Self {
            control,
            committed: false,
        })
    }

    /// Commit the session-scoped transaction.
    ///
    /// Marks the guard as committed so that `Drop` will not roll back.
    /// The committed flag is set only after a successful commit — if
    /// `control.commit()` fails, `Drop` will still trigger rollback.
    pub async fn commit(mut self) -> Result<(), IndexError> {
        self.control.commit().await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.committed {
            if let Err(e) = self.control.rollback() {
                log::error!("Session rollback failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock that records every method call for assertion.
    struct MockSessionControl {
        calls: Mutex<Vec<&'static str>>,
        commit_result: Mutex<Option<Result<(), IndexError>>>,
    }

    impl MockSessionControl {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                commit_result: Mutex::new(None),
            }
        }

        fn fail_commit(error: IndexError) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                commit_result: Mutex::new(Some(Err(error))),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("lock poisoned").clone()
        }
    }

    #[async_trait]
    impl SessionControl for MockSessionControl {
        async fn begin(&self) -> Result<(), IndexError> {
            self.calls.lock().expect("lock poisoned").push("begin");
            Ok(())
        }

        async fn commit(&self) -> Result<(), IndexError> {
            self.calls.lock().expect("lock poisoned").push("commit");
            match self.commit_result.lock().expect("lock poisoned").take() {
                Some(result) => result,
                None => Ok(()),
            }
        }

        fn rollback(&self) -> Result<(), IndexError> {
            self.calls.lock().expect("lock poisoned").push("rollback");
            Ok(())
        }
    }

    #[tokio::test]
    async fn begin_calls_control_begin() {
        let mock = Arc::new(MockSessionControl::new());
        let _guard = SessionGuard::begin(mock.clone()).await.expect("begin");
        assert_eq!(mock.calls()[0], "begin");
    }

    #[tokio::test]
    async fn commit_suppresses_rollback_on_drop() {
        let mock = Arc::new(MockSessionControl::new());
        let guard = SessionGuard::begin(mock.clone()).await.expect("begin");
        guard.commit().await.expect("commit");
        assert_eq!(mock.calls(), vec!["begin", "commit"]);
    }

    #[tokio::test]
    async fn drop_without_commit_triggers_rollback() {
        let mock = Arc::new(MockSessionControl::new());
        let guard = SessionGuard::begin(mock.clone()).await.expect("begin");
        drop(guard);
        assert_eq!(mock.calls(), vec!["begin", "rollback"]);
    }

    #[tokio::test]
    async fn failed_commit_still_triggers_rollback() {
        let mock = Arc::new(MockSessionControl::fail_commit(IndexError::CorruptedData));
        let guard = SessionGuard::begin(mock.clone()).await.expect("begin");
        let result = guard.commit().await;
        assert!(result.is_err());
        assert_eq!(mock.calls(), vec!["begin", "commit", "rollback"]);
    }
}
