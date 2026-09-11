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

use drasi_core::interface::SessionError;

use std::sync::{Arc, Mutex, MutexGuard};

use crate::IndexDb;
use async_trait::async_trait;
use drasi_core::interface::{
    CacheGeneration, IndexError, RollbackSupport, SessionControl, SessionTracker,
};
use rocksdb::{Direction, IteratorMode, Transaction, WriteBatchWithTransaction};

/// Groups the active transaction and nesting depth under a single mutex.
///
/// Invariants (enforced internally by begin/commit/rollback):
/// - `depth == 0` ↔ `txn.is_none()` (no active session)
/// - `depth > 0`  ↔ `txn.is_some()` (session active)
///
/// Fields are private; external access goes through `with_txn()` so crate-level
/// code cannot desynchronize `depth` and `txn`.
struct SessionInner {
    txn: Option<Transaction<'static, IndexDb>>,
    depth: u32,
}

/// Shared session state for RocksDB session-scoped transactions.
///
/// Holds the shared `Arc<IndexDb>` and a `SessionInner` containing
/// the optional active `Transaction` and a nesting depth counter.
/// All three RocksDB index types (element, result, future_queue) share a single
/// `Arc<RocksDbSessionState>` so that a session transaction spans all indexes atomically.
///
/// `SessionGuard` owns the root outcome. Explicit child leases share the active
/// transaction without calling the backend lifecycle again.
///
/// # Safety
///
/// The `SessionInner.txn` field stores a `Transaction<'static, IndexDb>`
/// where the lifetime has been transmuted from the DB borrow lifetime to `'static`.
/// This is sound because:
///
/// 1. The `Arc<IndexDb>` prevents the DB from being deallocated while
///    any clone of the Arc exists.
/// 2. The `Drop` impl on `RocksDbSessionState` clears the transaction before struct fields
///    are dropped, ensuring the transaction is released before the Arc can be decremented.
/// 3. All holders of `Arc<RocksDbSessionState>` (the three index structs + `RocksDbSessionControl`)
///    are grouped in `IndexSet` and dropped together.
pub struct RocksDbSessionState {
    db: Arc<IndexDb>,
    inner: Mutex<SessionInner>,
    tracker: SessionTracker,
}

impl RocksDbSessionState {
    pub fn new(db: Arc<IndexDb>) -> Self {
        Self {
            db,
            tracker: SessionTracker::default(),
            inner: Mutex::new(SessionInner {
                txn: None,
                depth: 0,
            }),
        }
    }

    pub(crate) fn operation(self: &Arc<Self>) -> Result<RocksDbSessionOperation, IndexError> {
        Ok(RocksDbSessionOperation {
            state: self.clone(),
            generation: self.tracker.cache_generation()?,
        })
    }

    /// Begin a new session-scoped transaction, or nest into an existing one.
    ///
    /// - If no transaction is active (depth == 0), creates a real
    ///   `IndexDb::transaction()` and sets depth to 1.
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
        // SAFETY: The Arc<IndexDb> guarantees the DB outlives the
        // transaction. We clear the txn in Drop before the Arc is released.
        // See the safety comment on the struct.
        let txn: Transaction<'static, IndexDb> = unsafe { std::mem::transmute(txn) };
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
        f: impl FnOnce(&Transaction<'_, IndexDb>) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        let guard = self.lock()?;
        match guard.txn.as_ref() {
            Some(txn) => f(txn),
            None => Err(IndexError::other(SessionStateError(
                "operation requires an active session".to_string(),
            ))),
        }
    }

    /// Try `txn_fn` against the active session transaction. If no session is
    /// active, fall back to `db_fn` which operates on the DB directly.
    ///
    /// Read methods use this so they see uncommitted staged writes when a
    /// session is active, yet still work at startup before any session exists.
    pub(crate) fn with_txn_or_db<R>(
        &self,
        txn_fn: impl FnOnce(&Transaction<'_, IndexDb>) -> Result<R, IndexError>,
        db_fn: impl FnOnce(&IndexDb) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        let guard = self.lock()?;
        match guard.txn.as_ref() {
            Some(txn) => txn_fn(txn),
            None => db_fn(&self.db),
        }
    }
}

#[derive(Clone)]
pub(crate) struct RocksDbSessionOperation {
    state: Arc<RocksDbSessionState>,
    generation: CacheGeneration,
}

impl RocksDbSessionOperation {
    pub(crate) fn clear_ranges(
        &self,
        prefixes: &[(&str, &[u8])],
        keys: &[(&str, &[u8])],
    ) -> Result<(), IndexError> {
        self.clear_ranges_or(prefixes, keys, |db| {
            let mut batch = WriteBatchWithTransaction::<true>::default();
            for &(name, prefix) in prefixes {
                let cf = db.cf_handle(name).ok_or(IndexError::CorruptedData)?;
                let mode = if prefix.is_empty() {
                    IteratorMode::Start
                } else {
                    IteratorMode::From(prefix, Direction::Forward)
                };
                for key in collect_prefix_keys(db.iterator_cf(&cf, mode), prefix)? {
                    batch.delete_cf(&cf, key);
                }
            }
            for &(name, key) in keys {
                let cf = db.cf_handle(name).ok_or(IndexError::CorruptedData)?;
                batch.delete_cf(&cf, key);
            }
            db.write(batch).map_err(IndexError::other)
        })
    }

    pub(crate) fn clear_ranges_or(
        &self,
        prefixes: &[(&str, &[u8])],
        keys: &[(&str, &[u8])],
        standalone: impl FnOnce(&IndexDb) -> Result<(), IndexError>,
    ) -> Result<(), IndexError> {
        self.with_txn_or_db(
            |txn| {
                for &(name, prefix) in prefixes {
                    let cf = self
                        .state
                        .db
                        .cf_handle(name)
                        .ok_or(IndexError::CorruptedData)?;
                    let mode = if prefix.is_empty() {
                        IteratorMode::Start
                    } else {
                        IteratorMode::From(prefix, Direction::Forward)
                    };
                    for key in collect_prefix_keys(txn.iterator_cf(&cf, mode), prefix)? {
                        txn.delete_cf(&cf, key).map_err(IndexError::other)?;
                    }
                }
                for &(name, key) in keys {
                    let cf = self
                        .state
                        .db
                        .cf_handle(name)
                        .ok_or(IndexError::CorruptedData)?;
                    txn.delete_cf(&cf, key).map_err(IndexError::other)?;
                }
                Ok(())
            },
            standalone,
        )
    }

    pub(crate) fn with_txn<R>(
        &self,
        operation: impl FnOnce(&Transaction<'_, IndexDb>) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        self.state
            .tracker
            .with_generation(self.generation, || self.state.with_txn(operation))
    }

    pub(crate) fn with_txn_or_db<R>(
        &self,
        txn: impl FnOnce(&Transaction<'_, IndexDb>) -> Result<R, IndexError>,
        db: impl FnOnce(&IndexDb) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        self.state
            .tracker
            .with_generation(self.generation, || self.state.with_txn_or_db(txn, db))
    }
}

fn collect_prefix_keys(
    iter: impl Iterator<Item = Result<(Box<[u8]>, Box<[u8]>), rocksdb::Error>>,
    prefix: &[u8],
) -> Result<Vec<Box<[u8]>>, IndexError> {
    let mut keys = Vec::new();
    for item in iter {
        let (key, _) = item.map_err(IndexError::other)?;
        if !key.starts_with(prefix) {
            break;
        }
        keys.push(key);
    }
    Ok(keys)
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
/// Transaction creation is synchronous so cancellation cannot leave a detached
/// begin operation. Commit runs on the blocking pool inside the owned finalizer.
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
    fn root_tracker(&self) -> Option<&SessionTracker> {
        Some(&self.state.tracker)
    }

    fn rollback_support(&self) -> RollbackSupport {
        RollbackSupport::Complete
    }

    async fn begin(&self) -> Result<(), IndexError> {
        self.state.begin()
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
    use crate::open_unified_db;
    use crate::RocksIndexOptions;

    fn setup_test_db() -> (tempfile::TempDir, Arc<IndexDb>) {
        let dir = tempfile::TempDir::new().expect("failed to create temp dir");
        let options = RocksIndexOptions::new(false, false, crate::RocksDbMemoryBudget::default());
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

    #[tokio::test]
    async fn delayed_operation_cannot_write_into_a_new_root() {
        use drasi_core::interface::SessionGuard;

        let (_dir, db) = setup_test_db();
        let state = Arc::new(RocksDbSessionState::new(db.clone()));
        let control = Arc::new(RocksDbSessionControl::new(state.clone()));
        let old = SessionGuard::begin(control.clone()).await.unwrap();
        let operation = state.operation().unwrap();
        drop(old);
        let newer = SessionGuard::begin(control).await.unwrap();
        let db_copy = db.clone();
        let result = tokio::task::spawn_blocking(move || {
            let cf = db_copy.cf_handle("metadata").unwrap();
            operation.with_txn(|txn| {
                txn.put_cf(&cf, b"stale", b"write")
                    .map_err(IndexError::other)
            })
        })
        .await
        .unwrap();
        assert_eq!(
            result.unwrap_err(),
            IndexError::other(SessionError::StaleCacheGeneration)
        );
        newer.commit().await.unwrap();
        assert!(db
            .get_cf(&db.cf_handle("metadata").unwrap(), b"stale")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn root_administrative_reset_is_atomic() {
        use drasi_core::interface::{
            AccumulatorIndex, CheckpointStore, ElementArchiveIndex, ElementIndex, FutureQueue,
            LiveResultsWriter, OutboxWriter, RootOutcome, RowMutation, SessionGuard,
        };

        for commit in [false, true] {
            let dir = tempfile::TempDir::new().unwrap();
            let options =
                RocksIndexOptions::new(true, false, crate::RocksDbMemoryBudget::default());
            let db = open_unified_db(dir.path().to_str().unwrap(), "test", &options).unwrap();
            let state = Arc::new(RocksDbSessionState::new(db.clone()));
            let control = Arc::new(RocksDbSessionControl::new(state.clone()));
            let elements = crate::element_index::RocksDbElementIndex::new(
                db.clone(),
                options.clone(),
                state.clone(),
            );
            let results = crate::result_index::RocksDbResultIndex::new(
                db.clone(),
                state.clone(),
                options.clone(),
            );
            let queue =
                crate::future_queue::RocksDbFutureQueue::new(db.clone(), state.clone(), options);
            let checkpoints = crate::RocksDbCheckpointStore::new(db.clone(), state.clone());
            let outbox = crate::RocksDbOutboxWriter::new_with_session(db.clone(), state.clone());
            let live = crate::RocksDbLiveResultsWriter::new_with_session(db.clone(), state.clone());
            let families = [
                "elements",
                "slots",
                "inbound",
                "outbound",
                "partial",
                "archive",
                "values",
                "sorted-sets",
                "fqueue",
                "findex",
            ];
            for name in families {
                db.put_cf(&db.cf_handle(name).unwrap(), b"committed", b"original")
                    .unwrap();
            }
            let baseline = SessionGuard::begin(control.clone()).await.unwrap();
            checkpoints
                .stage_checkpoint("original", 7, Some(&bytes::Bytes::from_static(b"position")))
                .await
                .unwrap();
            checkpoints.write_config_hash(7).await.unwrap();
            checkpoints.write_result_sequence("test", 7).await.unwrap();
            outbox.append("test", 7, b"original").await.unwrap();
            live.apply_mutations(
                "test",
                &[RowMutation {
                    row_signature: 7,
                    data: Some(b"original"),
                }],
            )
            .await
            .unwrap();
            outbox.append("other", 1, b"untouched").await.unwrap();
            live.apply_mutations(
                "other",
                &[RowMutation {
                    row_signature: 1,
                    data: Some(b"untouched"),
                }],
            )
            .await
            .unwrap();
            baseline.commit().await.unwrap();

            let root = SessionGuard::begin(control).await.unwrap();
            root.mark_dirty().unwrap();
            assert!(matches!(
                outbox.trim_to_capacity("test", 0).await,
                Err(IndexError::NotSupported)
            ));
            state
                .operation()
                .unwrap()
                .with_txn(|txn| {
                    for name in families {
                        txn.put_cf(&db.cf_handle(name).unwrap(), b"tentative", b"discard")
                            .map_err(IndexError::other)?;
                    }
                    Ok(())
                })
                .unwrap();
            checkpoints
                .stage_checkpoint("tentative", 9, None)
                .await
                .unwrap();
            outbox.append("test", 9, b"discard").await.unwrap();
            live.apply_mutations(
                "test",
                &[RowMutation {
                    row_signature: 9,
                    data: Some(b"discard"),
                }],
            )
            .await
            .unwrap();

            ElementIndex::clear(&elements).await.unwrap();
            ElementArchiveIndex::clear(&elements).await.unwrap();
            results.clear().await.unwrap();
            queue.clear().await.unwrap();
            checkpoints.clear_checkpoints().await.unwrap();
            outbox.clear("test").await.unwrap();
            live.clear("test").await.unwrap();

            for name in families {
                let cf = db.cf_handle(name).unwrap();
                assert_eq!(
                    db.get_cf(&cf, b"committed").unwrap(),
                    Some(b"original".to_vec()),
                    "{name} bypassed the root"
                );
                state
                    .operation()
                    .unwrap()
                    .with_txn(|txn| {
                        assert!(txn.get_cf(&cf, b"committed").unwrap().is_none(), "{name}");
                        assert!(txn.get_cf(&cf, b"tentative").unwrap().is_none(), "{name}");
                        Ok(())
                    })
                    .unwrap();
            }
            assert!(checkpoints.read_all_checkpoints().await.unwrap().is_empty());
            assert_eq!(checkpoints.read_config_hash().await.unwrap(), None);
            assert_eq!(
                checkpoints.read_result_sequence("test").await.unwrap(),
                Some(7)
            );
            assert_eq!(
                outbox.read_from("test", 0).await.unwrap(),
                vec![(7, b"original".to_vec())]
            );
            assert_eq!(
                live.read_snapshot("test").await.unwrap(),
                vec![(7, b"original".to_vec())]
            );
            checkpoints
                .stage_checkpoint("replacement", 88, None)
                .await
                .unwrap();
            checkpoints.write_config_hash(88).await.unwrap();
            checkpoints.write_result_sequence("test", 88).await.unwrap();
            assert_eq!(checkpoints.read_config_hash().await.unwrap(), Some(88));
            assert_eq!(
                checkpoints.read_result_sequence("test").await.unwrap(),
                Some(88)
            );
            outbox.append("test", 88, b"replacement").await.unwrap();
            live.apply_mutations(
                "test",
                &[RowMutation {
                    row_signature: 88,
                    data: Some(b"replacement"),
                }],
            )
            .await
            .unwrap();
            if commit {
                root.commit().await.unwrap();
            } else {
                assert_eq!(root.rollback().unwrap(), RootOutcome::RolledBack);
            }
            for name in families {
                let cf = db.cf_handle(name).unwrap();
                assert_eq!(
                    db.get_cf(&cf, b"committed").unwrap(),
                    (!commit).then(|| b"original".to_vec()),
                    "{name}"
                );
                assert!(db.get_cf(&cf, b"tentative").unwrap().is_none(), "{name}");
            }
            let expected = if commit {
                (88, b"replacement".to_vec())
            } else {
                (7, b"original".to_vec())
            };
            assert_eq!(
                checkpoints.read_config_hash().await.unwrap(),
                Some(expected.0)
            );
            assert_eq!(
                checkpoints.read_result_sequence("test").await.unwrap(),
                Some(expected.0)
            );
            let checkpoint = checkpoints
                .read_checkpoint(if commit { "replacement" } else { "original" })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(checkpoint.sequence, expected.0);
            if !commit {
                assert_eq!(
                    checkpoint.source_position,
                    Some(bytes::Bytes::from_static(b"position"))
                );
            }
            assert!(checkpoints
                .read_checkpoint("tentative")
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                outbox.read_from("test", 0).await.unwrap(),
                vec![expected.clone()]
            );
            assert_eq!(live.read_snapshot("test").await.unwrap(), vec![expected]);
            assert_eq!(
                outbox.read_from("other", 0).await.unwrap(),
                vec![(1, b"untouched".to_vec())]
            );
            assert_eq!(
                live.read_snapshot("other").await.unwrap(),
                vec![(1, b"untouched".to_vec())]
            );
        }
    }

    #[tokio::test]
    async fn root_rollback_invalidates_every_cache_and_checkpoint() {
        use drasi_core::{
            evaluation::functions::aggregation::ValueAccumulator,
            index_cache::{
                cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
                shadowed_future_queue::ShadowedFutureQueue,
            },
            interface::{
                AccumulatorIndex, CheckpointStore, ElementIndex, FutureQueue, LazySortedSetStore,
                OutboxWriter, PushType, ResultKey, ResultOwner, RootOutcome, SessionGuard,
            },
            models::{Element, ElementMetadata, ElementReference},
        };
        use futures::StreamExt;
        use ordered_float::OrderedFloat;

        let (_dir, db) = setup_test_db();
        let options = RocksIndexOptions::new(false, false, crate::RocksDbMemoryBudget::default());
        let state = Arc::new(RocksDbSessionState::new(db.clone()));
        let control: Arc<dyn SessionControl> = Arc::new(RocksDbSessionControl::new(state.clone()));
        let raw = Arc::new(crate::element_index::RocksDbElementIndex::new(
            db.clone(),
            options.clone(),
            state.clone(),
        ));
        let elements =
            CachedElementIndex::new_with_session(raw.clone(), 8, control.clone()).unwrap();
        let results = CachedResultIndex::new_with_session(
            Arc::new(crate::result_index::RocksDbResultIndex::new(
                db.clone(),
                state.clone(),
                options.clone(),
            )),
            8,
            control.clone(),
        )
        .unwrap();
        let queue = ShadowedFutureQueue::new_with_session(
            Arc::new(crate::future_queue::RocksDbFutureQueue::new(
                db.clone(),
                state.clone(),
                options,
            )),
            control.clone(),
        );
        let outbox = crate::RocksDbOutboxWriter::new_with_session(db.clone(), state.clone());
        let checkpoints = crate::RocksDbCheckpointStore::new(db, state);
        let reference = ElementReference::new("source", "edge");
        let from = ElementReference::new("source", "a");
        let to = ElementReference::new("source", "b");
        let edge = Element::Relation {
            metadata: ElementMetadata {
                reference: reference.clone(),
                labels: Arc::new([Arc::from("R")]),
                effective_from: 1,
            },
            in_node: from.clone(),
            out_node: to.clone(),
            properties: Default::default(),
        };
        let key = ResultKey::InputHash(1);
        let owner = ResultOwner::Function(1);
        let value = OrderedFloat(7.0);

        let baseline = SessionGuard::begin(control.clone()).await.unwrap();
        results.increment_value_count(1, value, 10).await.unwrap();
        queue
            .push(PushType::Always, 1, 1, &reference, 1, 10)
            .await
            .unwrap();
        queue
            .push(PushType::Always, 1, 2, &reference, 1, 20)
            .await
            .unwrap();
        baseline.commit().await.unwrap();

        let mut outer = SessionGuard::begin(control.clone()).await.unwrap();
        let root = outer.root();
        let child = outer.child(&control).unwrap();
        elements.set_element(&edge, &vec![0]).await.unwrap();
        results
            .set(
                key.clone(),
                owner.clone(),
                Some(ValueAccumulator::Count { value: 42 }),
            )
            .await
            .unwrap();
        results.increment_value_count(1, value, 5).await.unwrap();
        assert_eq!(results.get_value_count(1, value).await.unwrap(), 15);
        assert_eq!(queue.pop().await.unwrap().unwrap().due_time, 10);
        assert_eq!(queue.peek_due_time().await.unwrap(), Some(20));
        checkpoints
            .stage_checkpoint("source", 2, None)
            .await
            .unwrap();
        outbox.append("test", 2, b"tentative").await.unwrap();
        assert!(outbox.read_from("test", 0).await.unwrap().is_empty());
        let _provisional = child.complete(()).unwrap();
        let child = outer.child(&control).unwrap();
        assert!(elements.get_element(&reference).await.unwrap().is_some());
        assert!(elements
            .get_slot_element_by_ref(0, &reference)
            .await
            .unwrap()
            .is_some());
        let inbound = elements
            .get_slot_elements_by_inbound(0, &from)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(inbound.len(), 1);
        assert!(inbound[0].is_ok());
        let outbound = elements
            .get_slot_elements_by_outbound(0, &to)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(outbound.len(), 1);
        assert!(outbound[0].is_ok());
        let _second = child.complete(()).unwrap();
        let mut delayed = elements
            .get_slot_elements_by_inbound(0, &from)
            .await
            .unwrap();
        drop(outer);
        assert_eq!(root.outcome(), RootOutcome::RolledBack);

        let retry = SessionGuard::begin(control).await.unwrap();
        assert_eq!(
            delayed.next().await.unwrap().unwrap_err(),
            IndexError::other(SessionError::StaleCacheGeneration)
        );
        assert!(raw.get_element(&reference).await.unwrap().is_none());
        assert!(elements.get_element(&reference).await.unwrap().is_none());
        assert!(elements
            .get_slot_element_by_ref(0, &reference)
            .await
            .unwrap()
            .is_none());
        assert!(elements
            .get_slot_elements_by_inbound(0, &from)
            .await
            .unwrap()
            .next()
            .await
            .is_none());
        assert!(elements
            .get_slot_elements_by_outbound(0, &to)
            .await
            .unwrap()
            .next()
            .await
            .is_none());
        assert!(results.get(&key, &owner).await.unwrap().is_none());
        assert_eq!(results.get_value_count(1, value).await.unwrap(), 10);
        assert_eq!(queue.peek_due_time().await.unwrap(), Some(10));
        assert!(outbox.read_from("test", 0).await.unwrap().is_empty());
        assert!(checkpoints
            .read_checkpoint("source")
            .await
            .unwrap()
            .is_none());
        elements.set_element(&edge, &vec![0]).await.unwrap();
        retry.commit().await.unwrap();
    }
}
