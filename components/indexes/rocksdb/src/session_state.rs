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

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use drasi_core::interface::{IndexError, SessionControl};
use rocksdb::{OptimisticTransactionDB, Transaction};

/// Groups the active transaction and nesting depth under a single mutex.
///
/// Invariants (enforced internally by begin/commit/rollback):
/// - `depth == 0` ↔ `txn.is_none()` (no active session)
/// - `depth > 0`  ↔ `txn.is_some()` (session active)
///
/// Fields are private; external access goes through `with_txn()` so crate-level
/// code cannot desynchronize `depth` and `txn`.
struct SessionInner {
    txn: Option<Transaction<'static, OptimisticTransactionDB>>,
    depth: u32,
}

/// Shared session state for RocksDB session-scoped transactions.
///
/// Holds the shared `Arc<OptimisticTransactionDB>` and a `SessionInner` containing
/// the optional active `Transaction` and a nesting depth counter.
/// All three RocksDB index types (element, result, future_queue) share a single
/// `Arc<RocksDbSessionState>` so that a session transaction spans all indexes atomically.
///
/// # Nested Transactions
///
/// Supports nesting via a depth counter. Only the outermost `begin()`/`commit()` pair
/// creates and commits the real RocksDB transaction. Inner calls are no-ops that
/// increment/decrement the depth counter. `rollback()` at any depth aborts the real
/// transaction and resets depth to 0.
///
/// This enables drasi-lib to wrap core's `process_source_change()` in an outer
/// `begin()`/`commit()` pair while core uses an inner pair, with both writing
/// into the same atomic transaction.
///
/// # Safety
///
/// The `SessionInner.txn` field stores a `Transaction<'static, OptimisticTransactionDB>`
/// where the lifetime has been transmuted from the DB borrow lifetime to `'static`.
/// This is sound because:
///
/// 1. The `Arc<OptimisticTransactionDB>` prevents the DB from being deallocated while
///    any clone of the Arc exists.
/// 2. The `Drop` impl on `RocksDbSessionState` clears the transaction before struct fields
///    are dropped, ensuring the transaction is released before the Arc can be decremented.
/// 3. All holders of `Arc<RocksDbSessionState>` (the three index structs + `RocksDbSessionControl`)
///    are grouped in `IndexSet` and dropped together.
pub struct RocksDbSessionState {
    db: Arc<OptimisticTransactionDB>,
    inner: Mutex<SessionInner>,
}

impl RocksDbSessionState {
    pub fn new(db: Arc<OptimisticTransactionDB>) -> Self {
        Self {
            db,
            inner: Mutex::new(SessionInner {
                txn: None,
                depth: 0,
            }),
        }
    }

    /// Begin a new session-scoped transaction, or nest into an existing one.
    ///
    /// - If no transaction is active (depth == 0), creates a real
    ///   `OptimisticTransactionDB::transaction()` and sets depth to 1.
    /// - If a transaction is already active (depth > 0), increments depth
    ///   without creating a new transaction (nested no-op).
    pub(crate) fn begin(&self) -> Result<(), IndexError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;

        if guard.depth > 0 {
            guard.depth += 1;
            log::trace!("session begin: depth {} (nested no-op)", guard.depth);
            return Ok(());
        }

        // Depth is 0 — create real transaction
        debug_assert!(
            guard.txn.is_none(),
            "depth==0 but txn is Some — invariant violated"
        );
        let txn = self.db.transaction();
        // SAFETY: The Arc<OptimisticTransactionDB> guarantees the DB outlives the
        // transaction. We clear the txn in Drop before the Arc is released.
        // See the safety comment on the struct.
        let txn: Transaction<'static, OptimisticTransactionDB> =
            unsafe { std::mem::transmute(txn) };
        guard.txn = Some(txn);
        guard.depth = 1;
        log::trace!("session begin: depth 1 (real transaction)");
        Ok(())
    }

    /// Commit the session-scoped transaction, or decrement nesting depth.
    ///
    /// - If depth > 1, decrements depth (nested no-op).
    /// - If depth == 1, takes ownership of the real transaction and commits it.
    /// - If depth == 0, returns an error (no active transaction).
    pub(crate) fn commit(&self) -> Result<(), IndexError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;

        match guard.depth {
            0 => Err(IndexError::other(SessionStateError(
                "commit() called with no active transaction".to_string(),
            ))),
            1 => {
                // Real commit — depth 1 → 0
                let txn = guard.txn.take().ok_or_else(|| {
                    IndexError::other(SessionStateError(
                        "commit() at depth 1 but no transaction present — invariant violated"
                            .to_string(),
                    ))
                })?;
                guard.depth = 0;
                log::trace!("session commit: depth 1→0 (real commit)");
                txn.commit().map_err(IndexError::other)
            }
            n => {
                // Nested commit — just decrement depth
                guard.depth = n - 1;
                log::trace!("session commit: depth {n}→{} (nested no-op)", n - 1);
                Ok(())
            }
        }
    }

    /// Roll back the session-scoped transaction.
    ///
    /// At any depth > 0, resets depth to 0 and drops the real transaction
    /// (triggering implicit rollback). At depth 0, this is a no-op returning Ok.
    /// The no-op behavior at depth 0 is important for `SessionGuard::Drop` —
    /// after an inner rollback already cleared the transaction, the outer
    /// guard's drop must not error.
    pub(crate) fn rollback(&self) -> Result<(), IndexError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;

        if guard.depth == 0 {
            return Ok(());
        }

        let prev_depth = guard.depth;
        guard.depth = 0;
        let _ = guard.txn.take(); // Drop triggers implicit rollback
        log::trace!("session rollback: depth {prev_depth}→0");
        Ok(())
    }

    /// Lock the session inner state. Private; crate-level callers use `with_txn()`.
    fn lock(&self) -> Result<MutexGuard<'_, SessionInner>, IndexError> {
        self.inner
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))
    }

    /// Execute `f` against the active session transaction.
    /// Returns an error if no session is active.
    pub(crate) fn with_txn<R>(
        &self,
        f: impl FnOnce(&Transaction<'_, OptimisticTransactionDB>) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        let guard = self.lock()?;
        match guard.txn.as_ref() {
            Some(txn) => f(txn),
            None => Err(IndexError::other(SessionStateError(
                "operation requires an active session".to_string(),
            ))),
        }
    }
}

impl Drop for RocksDbSessionState {
    fn drop(&mut self) {
        // Clear the transaction before struct fields (including the Arc<DB>) are dropped.
        // This is load-bearing: Rust drops fields in declaration order, so without this,
        // `db` would drop before `inner`, causing use-after-free if this is the last Arc<DB>.
        //
        // Use get_mut() since &mut self guarantees exclusive access. If the mutex is
        // poisoned (a thread panicked while holding the lock), we MUST still clear the
        // transaction to uphold the safety invariant of the 'static transmute.
        // Drop must never panic (panic-in-Drop can double-panic and abort the
        // process, and would skip the cleanup below — which for RocksDB is
        // load-bearing for the 'static transmute safety invariant).
        let inner = match self.inner.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };

        if inner.depth > 0 {
            log::warn!(
                "RocksDbSessionState dropped with depth {} — rolling back",
                inner.depth
            );
            inner.depth = 0;
        }
        if inner.txn.is_some() {
            log::warn!("RocksDbSessionState dropped with active transaction — rolling back");
        }
        let _ = inner.txn.take();
    }
}

/// SessionControl implementation backed by `RocksDbSessionState`.
///
/// Wraps `Arc<RocksDbSessionState>` and delegates begin/commit/rollback.
/// `begin` and `commit` use `spawn_blocking` to bridge async → sync since
/// RocksDB operations may involve I/O.
pub struct RocksDbSessionControl {
    state: Arc<RocksDbSessionState>,
}

impl RocksDbSessionControl {
    pub fn new(state: Arc<RocksDbSessionState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl SessionControl for RocksDbSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || state.begin())
            .await
            .map_err(IndexError::other)?
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || state.commit())
            .await
            .map_err(IndexError::other)?
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.state.rollback()
    }
}

#[derive(Debug)]
struct PoisonError(String);

impl std::fmt::Display for PoisonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mutex poisoned: {}", self.0)
    }
}

impl std::error::Error for PoisonError {}

#[derive(Debug)]
struct SessionStateError(String);

impl std::fmt::Display for SessionStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session state error: {}", self.0)
    }
}

impl std::error::Error for SessionStateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element_index::RocksIndexOptions;
    use crate::open_unified_db;

    fn setup_test_db() -> (tempfile::TempDir, Arc<OptimisticTransactionDB>) {
        let dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let options = RocksIndexOptions {
            archive_enabled: false,
            direct_io: false,
        };
        let db = open_unified_db(dir.path().to_str().expect("path"), "test", &options)
            .expect("failed to open db");
        (dir, db)
    }

    #[test]
    fn single_begin_commit() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("begin");
        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put");
        state.commit().expect("commit");

        let val = db.get_cf(&cf, b"key1").expect("get").expect("should exist");
        assert_eq!(val, b"val1");
    }

    #[test]
    fn single_begin_rollback() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("begin");
        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put");
        state.rollback().expect("rollback");

        assert!(db.get_cf(&cf, b"key1").expect("get").is_none());
    }

    #[test]
    fn nested_begin_commit() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("outer begin"); // depth 0→1
        state.begin().expect("inner begin"); // depth 1→2

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put during inner");

        state.commit().expect("inner commit"); // depth 2→1 (no-op)

        // Write checkpoint between inner commit and outer commit
        state
            .with_txn(|txn| {
                txn.put_cf(&cf, b"checkpoint", b"cp1")
                    .map_err(IndexError::other)
            })
            .expect("put checkpoint");

        state.commit().expect("outer commit"); // depth 1→0 (real commit)

        assert_eq!(
            db.get_cf(&cf, b"key1").expect("get").expect("some"),
            b"val1"
        );
        assert_eq!(
            db.get_cf(&cf, b"checkpoint").expect("get").expect("some"),
            b"cp1"
        );
    }

    #[test]
    fn outer_rollback_after_inner_commit() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("outer begin");
        state.begin().expect("inner begin");

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put");

        state.commit().expect("inner commit"); // depth 2→1, no-op
        state.rollback().expect("outer rollback"); // depth 1→0, real rollback

        assert!(db.get_cf(&cf, b"key1").expect("get").is_none());
    }

    #[test]
    fn inner_rollback_aborts_all() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("outer begin"); // depth 0→1
        state.begin().expect("inner begin"); // depth 1→2

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put");

        state.rollback().expect("inner rollback"); // any depth → 0, real rollback

        // Transaction is gone. with_txn should error.
        assert!(state.with_txn(|_txn| Ok(())).is_err());
        // Commit at depth 0 should error.
        assert!(state.commit().is_err());
        // Rollback at depth 0 is a no-op.
        assert!(state.rollback().is_ok());
        // Data should not be persisted.
        assert!(db.get_cf(&cf, b"key1").expect("get").is_none());
    }

    #[test]
    fn triple_nesting() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("begin 1"); // depth 1
        state.begin().expect("begin 2"); // depth 2
        state.begin().expect("begin 3"); // depth 3

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put");

        state.commit().expect("commit 3"); // depth 3→2
        state.commit().expect("commit 2"); // depth 2→1
        state.commit().expect("commit 1"); // depth 1→0 (real)

        assert_eq!(
            db.get_cf(&cf, b"key1").expect("get").expect("some"),
            b"val1"
        );
    }

    #[test]
    fn writes_between_inner_and_outer_commit() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db.clone());
        let cf = db.cf_handle("metadata").expect("cf");

        state.begin().expect("outer begin");
        state.begin().expect("inner begin");

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key1", b"val1").map_err(IndexError::other))
            .expect("put key1");

        state.commit().expect("inner commit"); // no-op

        state
            .with_txn(|txn| txn.put_cf(&cf, b"key2", b"val2").map_err(IndexError::other))
            .expect("put key2");

        state.commit().expect("outer commit"); // real commit

        assert_eq!(
            db.get_cf(&cf, b"key1").expect("get").expect("some"),
            b"val1"
        );
        assert_eq!(
            db.get_cf(&cf, b"key2").expect("get").expect("some"),
            b"val2"
        );
    }

    #[test]
    fn commit_at_depth_zero_errors() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db);
        assert!(state.commit().is_err());
    }

    #[test]
    fn rollback_at_depth_zero_is_noop() {
        let (_dir, db) = setup_test_db();
        let state = RocksDbSessionState::new(db);
        assert!(state.rollback().is_ok());
    }
}
