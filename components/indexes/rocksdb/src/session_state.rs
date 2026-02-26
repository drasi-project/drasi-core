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

/// Shared session state for RocksDB session-scoped transactions.
///
/// Holds the shared `Arc<OptimisticTransactionDB>` and an optional active `Transaction`.
/// All three RocksDB index types (element, result, future_queue) share a single
/// `Arc<RocksDbSessionState>` so that a session transaction spans all indexes atomically.
///
/// # Safety
///
/// The `active_txn` field stores a `Transaction<'static, OptimisticTransactionDB>` where
/// the lifetime has been transmuted from the DB borrow lifetime to `'static`. This is
/// sound because:
///
/// 1. The `Arc<OptimisticTransactionDB>` prevents the DB from being deallocated while
///    any clone of the Arc exists.
/// 2. The `Drop` impl on `RocksDbSessionState` clears `active_txn` before struct fields
///    are dropped, ensuring the transaction is released before the Arc can be decremented.
/// 3. All holders of `Arc<RocksDbSessionState>` (the three index structs + `RocksDbSessionControl`)
///    are grouped in `IndexSet` and dropped together.
pub struct RocksDbSessionState {
    db: Arc<OptimisticTransactionDB>,
    active_txn: Mutex<Option<Transaction<'static, OptimisticTransactionDB>>>,
}

impl RocksDbSessionState {
    pub fn new(db: Arc<OptimisticTransactionDB>) -> Self {
        Self {
            db,
            active_txn: Mutex::new(None),
        }
    }

    /// Begin a new session-scoped transaction.
    ///
    /// Creates a new `OptimisticTransactionDB::transaction()` and stores it.
    /// Returns an error if a transaction is already active.
    pub(crate) fn begin(&self) -> Result<(), IndexError> {
        let txn = self.db.transaction();
        // SAFETY: The Arc<OptimisticTransactionDB> guarantees the DB outlives the
        // transaction. We clear active_txn in Drop before the Arc is released.
        // See the safety comment on the `active_txn` field.
        let txn: Transaction<'static, OptimisticTransactionDB> =
            unsafe { std::mem::transmute(txn) };
        let mut guard = self
            .active_txn
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
        if guard.is_some() {
            return Err(IndexError::other(SessionStateError(
                "begin() called while a transaction is already active".to_string(),
            )));
        }
        *guard = Some(txn);
        Ok(())
    }

    /// Commit the active session-scoped transaction.
    ///
    /// Takes ownership of the transaction from `active_txn` and calls `commit()`.
    pub(crate) fn commit(&self) -> Result<(), IndexError> {
        let mut guard = self
            .active_txn
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
        match guard.take() {
            Some(txn) => txn.commit().map_err(IndexError::other),
            None => Err(IndexError::other(SessionStateError(
                "commit() called with no active transaction".to_string(),
            ))),
        }
    }

    /// Roll back the active session-scoped transaction.
    ///
    /// Takes the transaction from `active_txn` and drops it (implicit rollback).
    /// This is synchronous and safe for use in `Drop` implementations.
    pub(crate) fn rollback(&self) -> Result<(), IndexError> {
        let mut guard = self
            .active_txn
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
        let _ = guard.take(); // Drop triggers implicit rollback
        Ok(())
    }

    /// Lock and return a guard providing access to the active transaction.
    ///
    /// Callers should check if the Option is Some (session active) or None (no session).
    pub(crate) fn lock(
        &self,
    ) -> Result<MutexGuard<'_, Option<Transaction<'static, OptimisticTransactionDB>>>, IndexError>
    {
        self.active_txn
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))
    }

    /// Execute `f` against the active session transaction if one exists,
    /// otherwise create an auto-commit transaction, run `f`, and commit.
    pub(crate) fn with_txn<R>(
        &self,
        f: impl FnOnce(&Transaction<'_, OptimisticTransactionDB>) -> Result<R, IndexError>,
    ) -> Result<R, IndexError> {
        let guard = self.lock()?;
        if let Some(txn) = guard.as_ref() {
            f(txn)
        } else {
            drop(guard);
            let txn = self.db.transaction();
            let result = f(&txn)?;
            txn.commit().map_err(IndexError::other)?;
            Ok(result)
        }
    }
}

impl Drop for RocksDbSessionState {
    fn drop(&mut self) {
        // Clear the transaction before struct fields (including the Arc<DB>) are dropped.
        // This is load-bearing: Rust drops fields in declaration order, so without this,
        // `db` would drop before `active_txn`, causing use-after-free if this is the last Arc<DB>.
        //
        // Use get_mut() since &mut self guarantees exclusive access. If the mutex is
        // poisoned (a thread panicked while holding the lock), we MUST still clear the
        // transaction to uphold the safety invariant of the 'static transmute.
        let inner = match self.active_txn.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        if inner.is_some() {
            log::warn!("RocksDbSessionState dropped with active transaction — rolling back");
        }
        let _ = inner.take();
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
