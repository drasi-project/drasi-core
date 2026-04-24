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
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use drasi_core::interface::{IndexError, SessionControl};
use redis::{aio::MultiplexedConnection, Pipeline, ToRedisArgs};

/// Result of checking the write buffer for a key/field.
///
/// Three-state enum to distinguish between "buffer has the value",
/// "key was explicitly deleted in buffer", and "buffer has no info".
#[derive(Debug)]
pub enum BufferReadResult<T> {
    /// Buffer has the value — use it directly.
    Found(T),
    /// Key was explicitly deleted in this session — return None to caller.
    KeyDeleted,
    /// Buffer has no info for this key — fall through to Redis.
    NotInBuffer,
}

/// State of a single key in the write buffer.
enum KeyState {
    /// Key was DEL'd in this session.
    Deleted,
    /// GET/SET string — always absolute values, never deltas.
    StringValue(Vec<u8>),
    /// HGET/HSET hash — `None` field value means field was deleted.
    Hash {
        fields: HashMap<String, Option<Vec<u8>>>,
    },
    /// SADD/SREM set — delta-based tracking.
    Set {
        added: HashSet<Vec<u8>>,
        removed: HashSet<Vec<u8>>,
    },
    /// ZADD/ZREM sorted set — delta-based tracking.
    SortedSet {
        added: HashMap<Vec<u8>, f64>,
        removed: HashSet<Vec<u8>>,
    },
}

/// Deltas for a buffered set, used by callers that need to merge with Redis.
#[derive(Debug)]
pub struct SetDeltas {
    pub added: HashSet<Vec<u8>>,
    pub removed: HashSet<Vec<u8>>,
    /// When true, the key was DEL'd then re-added within this session.
    /// Callers must not fall through to Redis for members not in `added`/`removed`.
    pub full_replace: bool,
}

/// Deltas for a buffered sorted set, used by callers that need to merge with Redis.
#[derive(Debug)]
pub struct SortedSetDeltas {
    pub added: HashMap<Vec<u8>, f64>,
    pub removed: HashSet<Vec<u8>>,
    /// When true, the key was DEL'd then re-added within this session.
    /// Callers must not fall through to Redis for members not in `added`/`removed`.
    pub full_replace: bool,
}

/// In-memory write buffer that intercepts all reads and writes during a session.
///
/// Models Redis data types with delta-based tracking. On commit, the buffer
/// is drained into a single atomic Redis pipeline (MULTI/EXEC).
pub struct WriteBuffer {
    keys: HashMap<String, KeyState>,
    /// Keys that were DEL'd at some point during this session. A subsequent
    /// write (set_add, hash_set, …) overwrites `KeyState::Deleted` in `keys`,
    /// but we still need to emit `DEL key` on commit so stale Redis members
    /// are removed before the new state is applied.
    cleared_keys: HashSet<String>,
}

impl WriteBuffer {
    fn new() -> Self {
        Self {
            keys: HashMap::new(),
            cleared_keys: HashSet::new(),
        }
    }

    // --- String operations ---

    pub fn string_set(&mut self, key: String, value: Vec<u8>) {
        self.keys.insert(key, KeyState::StringValue(value));
    }

    pub fn string_get(&self, key: &str) -> BufferReadResult<Vec<u8>> {
        match self.keys.get(key) {
            Some(KeyState::StringValue(v)) => BufferReadResult::Found(v.clone()),
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    // --- Hash operations ---

    pub fn hash_set(&mut self, key: String, field: &str, value: Vec<u8>) {
        match self.keys.get_mut(&key) {
            Some(KeyState::Hash { fields }) => {
                fields.insert(field.to_string(), Some(value));
            }
            _ => {
                let mut fields = HashMap::new();
                fields.insert(field.to_string(), Some(value));
                self.keys.insert(key, KeyState::Hash { fields });
            }
        }
    }

    pub fn hash_get(&self, key: &str, field: &str) -> BufferReadResult<Vec<u8>> {
        match self.keys.get(key) {
            Some(KeyState::Hash { fields }) => match fields.get(field) {
                Some(Some(v)) => BufferReadResult::Found(v.clone()),
                Some(None) => BufferReadResult::KeyDeleted, // field was deleted
                None => {
                    if self.cleared_keys.contains(key) {
                        // Key was DEL'd then re-added as hash — field not in the
                        // new hash means it doesn't exist.
                        BufferReadResult::KeyDeleted
                    } else {
                        BufferReadResult::NotInBuffer // field not in buffer
                    }
                }
            },
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    pub fn hash_del(&mut self, key: String, field: &str) {
        match self.keys.get_mut(&key) {
            Some(KeyState::Hash { fields }) => {
                fields.insert(field.to_string(), None);
            }
            _ => {
                let mut fields = HashMap::new();
                fields.insert(field.to_string(), None);
                self.keys.insert(key, KeyState::Hash { fields });
            }
        }
    }

    // --- Set operations ---

    pub fn set_add(&mut self, key: String, member: Vec<u8>) {
        match self.keys.get_mut(&key) {
            Some(KeyState::Set { added, removed }) => {
                removed.remove(&member);
                added.insert(member);
            }
            _ => {
                let mut added = HashSet::new();
                added.insert(member);
                self.keys.insert(
                    key,
                    KeyState::Set {
                        added,
                        removed: HashSet::new(),
                    },
                );
            }
        }
    }

    pub fn set_remove(&mut self, key: String, member: Vec<u8>) {
        match self.keys.get_mut(&key) {
            Some(KeyState::Set { added, removed }) => {
                if !added.remove(&member) {
                    removed.insert(member);
                }
            }
            Some(KeyState::Deleted) => {
                // Key was deleted — nothing to remove
            }
            _ => {
                let mut removed = HashSet::new();
                removed.insert(member);
                self.keys.insert(
                    key,
                    KeyState::Set {
                        added: HashSet::new(),
                        removed,
                    },
                );
            }
        }
    }

    /// Check if a member is in the buffer's added set.
    /// Returns `Found(true)` if added, `Found(false)` if removed or key deleted,
    /// `NotInBuffer` if the buffer has no info.
    pub fn set_is_member(&self, key: &str, member: &[u8]) -> BufferReadResult<bool> {
        match self.keys.get(key) {
            Some(KeyState::Set { added, removed }) => {
                if added.contains(member) {
                    BufferReadResult::Found(true)
                } else if removed.contains(member) {
                    BufferReadResult::Found(false)
                } else if self.cleared_keys.contains(key) {
                    // Key was DEL'd then re-added as a set — member not in
                    // added/removed means it doesn't exist in the new set.
                    BufferReadResult::Found(false)
                } else {
                    BufferReadResult::NotInBuffer
                }
            }
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    /// Get set deltas for merging with Redis results.
    /// Returns `None` if the key is not tracked as a set in the buffer.
    pub fn set_get_deltas(&self, key: &str) -> BufferReadResult<SetDeltas> {
        match self.keys.get(key) {
            Some(KeyState::Set { added, removed }) => BufferReadResult::Found(SetDeltas {
                added: added.clone(),
                removed: removed.clone(),
                full_replace: self.cleared_keys.contains(key),
            }),
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    // --- Sorted set operations ---

    pub fn zset_add(&mut self, key: String, member: Vec<u8>, score: f64) {
        match self.keys.get_mut(&key) {
            Some(KeyState::SortedSet { added, removed }) => {
                removed.remove(&member);
                added.insert(member, score);
            }
            _ => {
                let mut added = HashMap::new();
                added.insert(member, score);
                self.keys.insert(
                    key,
                    KeyState::SortedSet {
                        added,
                        removed: HashSet::new(),
                    },
                );
            }
        }
    }

    pub fn zset_remove(&mut self, key: String, member: Vec<u8>) {
        match self.keys.get_mut(&key) {
            Some(KeyState::SortedSet { added, removed }) => {
                if added.remove(&member).is_none() {
                    removed.insert(member);
                }
            }
            Some(KeyState::Deleted) => {
                // Key was deleted — nothing to remove
            }
            _ => {
                let mut removed = HashSet::new();
                removed.insert(member);
                self.keys.insert(
                    key,
                    KeyState::SortedSet {
                        added: HashMap::new(),
                        removed,
                    },
                );
            }
        }
    }

    /// Get sorted set deltas for merging with Redis results.
    pub fn zset_get_deltas(&self, key: &str) -> BufferReadResult<SortedSetDeltas> {
        match self.keys.get(key) {
            Some(KeyState::SortedSet { added, removed }) => {
                BufferReadResult::Found(SortedSetDeltas {
                    added: added.clone(),
                    removed: removed.clone(),
                    full_replace: self.cleared_keys.contains(key),
                })
            }
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    // --- Key-level operations ---

    pub fn del(&mut self, key: String) {
        self.cleared_keys.insert(key.clone());
        self.keys.insert(key, KeyState::Deleted);
    }

    pub fn is_deleted(&self, key: &str) -> bool {
        matches!(self.keys.get(key), Some(KeyState::Deleted))
    }

    /// Drain the buffer into a Redis pipeline for atomic execution.
    ///
    /// For keys that went through `Deleted` → new state within a single session,
    /// the `Deleted` variant was overwritten by the subsequent write. We track
    /// these in `cleared_keys` so we can emit `DEL key` before applying the new
    /// state, ensuring stale Redis members/fields are removed.
    pub fn drain_into_pipeline(&mut self, pipeline: &mut Pipeline) {
        let cleared = std::mem::take(&mut self.cleared_keys);

        for (key, state) in self.keys.drain() {
            match state {
                KeyState::Deleted => {
                    pipeline.del(&key).ignore();
                }
                KeyState::StringValue(v) => {
                    // SET is a full overwrite — no DEL needed even for cleared keys.
                    pipeline.cmd("SET").arg(&key).arg(v).ignore();
                }
                KeyState::Hash { fields } => {
                    // If this key was DEL'd then rewritten, clear stale fields first.
                    if cleared.contains(&key) {
                        pipeline.del(&key).ignore();
                    }

                    let mut set_fields: Vec<(String, Vec<u8>)> = Vec::new();
                    let mut del_fields: Vec<String> = Vec::new();

                    for (field, value) in fields {
                        match value {
                            Some(v) => set_fields.push((field, v)),
                            None => del_fields.push(field),
                        }
                    }

                    if !set_fields.is_empty() {
                        let mut cmd = redis::cmd("HSET");
                        cmd.arg(&key);
                        for (field, value) in set_fields {
                            cmd.arg(field).arg(value);
                        }
                        pipeline.add_command(cmd).ignore();
                    }

                    if !del_fields.is_empty() {
                        let mut cmd = redis::cmd("HDEL");
                        cmd.arg(&key);
                        for field in del_fields {
                            cmd.arg(field);
                        }
                        pipeline.add_command(cmd).ignore();
                    }
                }
                KeyState::Set { added, removed } => {
                    // If this key was DEL'd then rewritten, clear stale members first.
                    if cleared.contains(&key) {
                        pipeline.del(&key).ignore();
                    } else if !removed.is_empty() {
                        // SREM before SADD to handle re-add after remove
                        let mut cmd = redis::cmd("SREM");
                        cmd.arg(&key);
                        for member in &removed {
                            cmd.arg(member.to_redis_args());
                        }
                        pipeline.add_command(cmd).ignore();
                    }
                    if !added.is_empty() {
                        let mut cmd = redis::cmd("SADD");
                        cmd.arg(&key);
                        for member in &added {
                            cmd.arg(member.to_redis_args());
                        }
                        pipeline.add_command(cmd).ignore();
                    }
                }
                KeyState::SortedSet { added, removed } => {
                    // If this key was DEL'd then rewritten, clear stale members first.
                    if cleared.contains(&key) {
                        pipeline.del(&key).ignore();
                    } else if !removed.is_empty() {
                        // ZREM before ZADD
                        let mut cmd = redis::cmd("ZREM");
                        cmd.arg(&key);
                        for member in &removed {
                            cmd.arg(member.to_redis_args());
                        }
                        pipeline.add_command(cmd).ignore();
                    }
                    if !added.is_empty() {
                        let mut cmd = redis::cmd("ZADD");
                        cmd.arg(&key);
                        for (member, score) in &added {
                            cmd.arg(*score).arg(member.to_redis_args());
                        }
                        pipeline.add_command(cmd).ignore();
                    }
                }
            }
        }
    }
}

/// Groups the active write buffer and nesting depth under a single mutex.
///
/// Invariants (enforced internally by begin/commit/rollback):
/// - `depth == 0` ↔ `buffer.is_none()` (no active session)
/// - `depth > 0`  ↔ `buffer.is_some()` (session active)
///
/// External callers access the buffer through narrow inherent methods
/// (`as_ref`, `as_mut`, `is_some`, `is_none`). `buffer` and `depth` are
/// private so crate-level code cannot desynchronize them (e.g., by taking
/// or replacing the buffer without adjusting depth).
pub(crate) struct SessionInner {
    buffer: Option<WriteBuffer>,
    depth: u32,
}

impl SessionInner {
    /// Get a reference to the active buffer, if any.
    pub(crate) fn as_ref(&self) -> Option<&WriteBuffer> {
        self.buffer.as_ref()
    }

    /// Get a mutable reference to the active buffer, if any.
    ///
    /// Callers mutate the buffer's contents (via its methods), but cannot
    /// replace or take the buffer itself — preserving the `depth`/`buffer`
    /// invariant.
    pub(crate) fn as_mut(&mut self) -> Option<&mut WriteBuffer> {
        self.buffer.as_mut()
    }

    /// Whether a session is active (buffer present).
    pub(crate) fn is_some(&self) -> bool {
        self.buffer.is_some()
    }

    /// Whether no session is active (buffer absent).
    #[allow(dead_code)]
    pub(crate) fn is_none(&self) -> bool {
        self.buffer.is_none()
    }
}

/// Shared session state for Garnet session-scoped transactions.
///
/// Holds a `SessionInner` (write buffer + nesting depth) behind a `Mutex`.
/// All Garnet index types (element, result, future_queue) share a single
/// `Arc<GarnetSessionState>` so that a session transaction spans all indexes atomically.
///
/// # Nested Transactions
///
/// Supports nesting via a depth counter. Only the outermost `begin()`/`commit()` pair
/// creates and commits the real write buffer. Inner calls are no-ops that
/// increment/decrement the depth counter. `rollback()` at any depth drops the buffer
/// and resets depth to 0.
///
/// The `connection` field is used solely for commit (MULTI/EXEC pipeline execution).
/// Each index keeps its own connection for non-session Redis reads.
pub struct GarnetSessionState {
    connection: MultiplexedConnection,
    inner: Mutex<SessionInner>,
}

impl GarnetSessionState {
    pub fn new(connection: MultiplexedConnection) -> Self {
        Self {
            connection,
            inner: Mutex::new(SessionInner {
                buffer: None,
                depth: 0,
            }),
        }
    }

    /// Begin a new session-scoped transaction, or nest into an existing one.
    ///
    /// - If no session is active (depth == 0), creates a new `WriteBuffer`
    ///   and sets depth to 1.
    /// - If a session is already active (depth > 0), increments depth
    ///   without creating a new buffer (nested no-op).
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

        debug_assert!(
            guard.buffer.is_none(),
            "depth==0 but buffer is Some — invariant violated"
        );
        guard.buffer = Some(WriteBuffer::new());
        guard.depth = 1;
        log::trace!("session begin: depth 1 (real buffer)");
        Ok(())
    }

    /// Commit the session-scoped transaction, or decrement nesting depth.
    ///
    /// - If depth > 1, decrements depth (nested no-op).
    /// - If depth == 1, takes the buffer, drains it into an atomic Redis
    ///   pipeline (MULTI/EXEC), and executes it.
    /// - If depth == 0, returns an error (no active session).
    ///
    /// Note: the buffer is consumed before the pipeline executes. If the
    /// pipeline fails, the buffer cannot be retried — the caller must
    /// retry the entire source change.
    pub(crate) async fn commit(&self) -> Result<(), IndexError> {
        let mut buffer = {
            let mut guard = self
                .inner
                .lock()
                .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;

            match guard.depth {
                0 => {
                    return Err(IndexError::other(SessionStateError(
                        "commit() called with no active session".to_string(),
                    )));
                }
                1 => {
                    // Real commit — depth 1 → 0
                    let buf = guard.buffer.take().ok_or_else(|| {
                        IndexError::other(SessionStateError(
                            "commit() at depth 1 but no buffer present — invariant violated"
                                .to_string(),
                        ))
                    })?;
                    guard.depth = 0;
                    log::trace!("session commit: depth 1→0 (real commit)");
                    buf
                }
                n => {
                    // Nested commit — just decrement depth
                    guard.depth = n - 1;
                    log::trace!("session commit: depth {n}→{} (nested no-op)", n - 1);
                    return Ok(());
                }
            }
        }; // guard dropped here

        let mut pipeline = redis::pipe();
        pipeline.atomic();
        buffer.drain_into_pipeline(&mut pipeline);

        let mut con = self.connection.clone();
        pipeline
            .query_async::<_, ()>(&mut con)
            .await
            .map_err(IndexError::other)
    }

    /// Roll back the session-scoped transaction.
    ///
    /// At any depth > 0, resets depth to 0 and drops the write buffer.
    /// At depth 0, this is a no-op returning Ok.
    /// The no-op behavior at depth 0 is important for `SessionGuard::Drop` —
    /// after an inner rollback already cleared the buffer, the outer
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
        let _ = guard.buffer.take(); // Drop the buffer
        log::trace!("session rollback: depth {prev_depth}→0");
        Ok(())
    }

    /// Lock and return a guard providing access to the session inner state.
    ///
    /// The returned `SessionInner` implements `Deref<Target = Option<WriteBuffer>>`
    /// and `DerefMut`, so callers can use `guard.as_ref()`, `guard.as_mut()`,
    /// `guard.is_some()`, etc. directly.
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, SessionInner>, IndexError> {
        self.inner
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))
    }
}

impl Drop for GarnetSessionState {
    fn drop(&mut self) {
        // Drop must never panic (panic-in-Drop can double-panic and abort the
        // process). Use get_mut() since &mut self guarantees exclusive access.
        if let Ok(inner) = self.inner.get_mut() {
            if inner.depth > 0 {
                log::warn!(
                    "GarnetSessionState dropped with depth {} — rolling back",
                    inner.depth
                );
                inner.depth = 0;
            }
            if inner.buffer.is_some() {
                log::warn!("GarnetSessionState dropped with active session — rolling back");
            }
            let _ = inner.buffer.take();
        }
    }
}

/// `SessionControl` implementation backed by `GarnetSessionState`.
///
/// Unlike RocksDB (which uses `spawn_blocking`), Garnet's `begin`/`rollback`
/// are sync (in-memory only) and `commit` is naturally async (Redis pipeline).
/// No `spawn_blocking` needed.
pub struct GarnetSessionControl {
    state: Arc<GarnetSessionState>,
}

impl GarnetSessionControl {
    pub fn new(state: Arc<GarnetSessionState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl SessionControl for GarnetSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        self.state.begin()
    }

    async fn commit(&self) -> Result<(), IndexError> {
        self.state.commit().await
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
pub(crate) struct SessionStateError(pub(crate) String);

impl std::fmt::Display for SessionStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "session state error: {}", self.0)
    }
}

impl std::error::Error for SessionStateError {}

#[cfg(test)]
mod tests {
    use super::*;

    // --- WriteBuffer: String operations ---

    #[test]
    fn string_set_and_get() {
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"hello".to_vec());
        match buf.string_get("k1") {
            BufferReadResult::Found(v) => assert_eq!(v, b"hello"),
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn string_overwrite() {
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"v1".to_vec());
        buf.string_set("k1".into(), b"v2".to_vec());
        match buf.string_get("k1") {
            BufferReadResult::Found(v) => assert_eq!(v, b"v2"),
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn string_get_not_in_buffer() {
        let buf = WriteBuffer::new();
        assert!(matches!(
            buf.string_get("k1"),
            BufferReadResult::NotInBuffer
        ));
    }

    #[test]
    fn string_get_after_del_returns_key_deleted() {
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"v1".to_vec());
        buf.del("k1".into());
        assert!(matches!(buf.string_get("k1"), BufferReadResult::KeyDeleted));
    }

    // --- WriteBuffer: Hash operations ---

    #[test]
    fn hash_field_set_and_get() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"val".to_vec());
        match buf.hash_get("h1", "f1") {
            BufferReadResult::Found(v) => assert_eq!(v, b"val"),
            _ => panic!("expected Found"),
        }
        // Unknown field falls through
        assert!(matches!(
            buf.hash_get("h1", "f2"),
            BufferReadResult::NotInBuffer
        ));
    }

    #[test]
    fn hash_field_del() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"val".to_vec());
        buf.hash_del("h1".into(), "f1");
        assert!(matches!(
            buf.hash_get("h1", "f1"),
            BufferReadResult::KeyDeleted
        ));
    }

    #[test]
    fn hash_whole_key_del_then_field_set_creates_fresh() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"old".to_vec());
        buf.del("h1".into());
        buf.hash_set("h1".into(), "f2", b"new".to_vec());
        // f1 should not be in the fresh hash — returns KeyDeleted because
        // the key was cleared and f1 is not in the new hash's fields.
        assert!(matches!(
            buf.hash_get("h1", "f1"),
            BufferReadResult::KeyDeleted
        ));
        match buf.hash_get("h1", "f2") {
            BufferReadResult::Found(v) => assert_eq!(v, b"new"),
            _ => panic!("expected Found"),
        }
    }

    // --- WriteBuffer: Set operations ---

    #[test]
    fn set_add_and_membership() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"m1".to_vec());
        match buf.set_is_member("s1", b"m1") {
            BufferReadResult::Found(true) => {}
            _ => panic!("expected Found(true)"),
        }
        assert!(matches!(
            buf.set_is_member("s1", b"m2"),
            BufferReadResult::NotInBuffer
        ));
    }

    #[test]
    fn set_remove_and_membership() {
        let mut buf = WriteBuffer::new();
        buf.set_remove("s1".into(), b"m1".to_vec());
        match buf.set_is_member("s1", b"m1") {
            BufferReadResult::Found(false) => {}
            _ => panic!("expected Found(false)"),
        }
    }

    #[test]
    fn set_double_toggle_add_remove_add() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"m1".to_vec());
        buf.set_remove("s1".into(), b"m1".to_vec());
        // After add→remove, member should not be in added set
        assert!(matches!(
            buf.set_is_member("s1", b"m1"),
            BufferReadResult::NotInBuffer
        ));
        // Re-add
        buf.set_add("s1".into(), b"m1".to_vec());
        match buf.set_is_member("s1", b"m1") {
            BufferReadResult::Found(true) => {}
            _ => panic!("expected Found(true) after re-add"),
        }
    }

    #[test]
    fn set_del_then_add_creates_fresh() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"m1".to_vec());
        buf.del("s1".into());
        assert!(matches!(
            buf.set_is_member("s1", b"m1"),
            BufferReadResult::KeyDeleted
        ));
        buf.set_add("s1".into(), b"m2".to_vec());
        match buf.set_is_member("s1", b"m2") {
            BufferReadResult::Found(true) => {}
            _ => panic!("expected Found(true) after del+add"),
        }
        // Key should be tracked in cleared_keys so drain emits DEL before SADD
        assert!(buf.cleared_keys.contains("s1"));
    }

    #[test]
    fn set_get_deltas() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"a".to_vec());
        buf.set_add("s1".into(), b"b".to_vec());
        buf.set_remove("s1".into(), b"c".to_vec());
        match buf.set_get_deltas("s1") {
            BufferReadResult::Found(deltas) => {
                assert!(deltas.added.contains(b"a".as_slice()));
                assert!(deltas.added.contains(b"b".as_slice()));
                assert!(deltas.removed.contains(b"c".as_slice()));
            }
            _ => panic!("expected Found"),
        }
    }

    // --- WriteBuffer: Sorted set operations ---

    #[test]
    fn zset_add_and_get_deltas() {
        let mut buf = WriteBuffer::new();
        buf.zset_add("z1".into(), b"m1".to_vec(), 1.0);
        buf.zset_add("z1".into(), b"m2".to_vec(), 2.0);
        match buf.zset_get_deltas("z1") {
            BufferReadResult::Found(deltas) => {
                assert_eq!(deltas.added.get(b"m1".as_slice()), Some(&1.0));
                assert_eq!(deltas.added.get(b"m2".as_slice()), Some(&2.0));
                assert!(deltas.removed.is_empty());
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn zset_remove() {
        let mut buf = WriteBuffer::new();
        buf.zset_remove("z1".into(), b"m1".to_vec());
        match buf.zset_get_deltas("z1") {
            BufferReadResult::Found(deltas) => {
                assert!(deltas.added.is_empty());
                assert!(deltas.removed.contains(b"m1".as_slice()));
            }
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn zset_score_update() {
        let mut buf = WriteBuffer::new();
        buf.zset_add("z1".into(), b"m1".to_vec(), 1.0);
        buf.zset_add("z1".into(), b"m1".to_vec(), 5.0);
        match buf.zset_get_deltas("z1") {
            BufferReadResult::Found(deltas) => {
                assert_eq!(deltas.added.get(b"m1".as_slice()), Some(&5.0));
            }
            _ => panic!("expected Found"),
        }
    }

    // --- WriteBuffer: Cross-type operations ---

    #[test]
    fn del_string_then_set_as_hash() {
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"val".to_vec());
        buf.del("k1".into());
        buf.hash_set("k1".into(), "f1", b"hval".to_vec());
        // Should now be a hash
        match buf.hash_get("k1", "f1") {
            BufferReadResult::Found(v) => assert_eq!(v, b"hval"),
            _ => panic!("expected Found"),
        }
        // String accessor should not find it
        assert!(matches!(
            buf.string_get("k1"),
            BufferReadResult::NotInBuffer
        ));
    }

    // --- WriteBuffer: drain_into_pipeline ---

    #[test]
    fn drain_empty_buffer_produces_no_commands() {
        let mut buf = WriteBuffer::new();
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        // Pipeline should have no commands — we verify by checking it was not modified
        // (no public len method, but we can verify the buffer is now empty)
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_deleted_key() {
        let mut buf = WriteBuffer::new();
        buf.del("k1".into());
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_string_value() {
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"v1".to_vec());
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_hash_with_set_and_del_fields() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"v1".to_vec());
        buf.hash_set("h1".into(), "f2", b"v2".to_vec());
        buf.hash_del("h1".into(), "f3");
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_set_with_add_and_remove() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"a".to_vec());
        buf.set_remove("s1".into(), b"b".to_vec());
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_sorted_set_with_add_and_remove() {
        let mut buf = WriteBuffer::new();
        buf.zset_add("z1".into(), b"m1".to_vec(), 1.0);
        buf.zset_remove("z1".into(), b"m2".to_vec());
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
    }

    #[test]
    fn drain_del_then_set_add_clears_stale_members() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"old".to_vec());
        buf.del("s1".into());
        buf.set_add("s1".into(), b"new".to_vec());
        assert!(buf.cleared_keys.contains("s1"));
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
        assert!(buf.cleared_keys.is_empty());
    }

    #[test]
    fn drain_del_then_zset_add_clears_stale_members() {
        let mut buf = WriteBuffer::new();
        buf.zset_add("z1".into(), b"old".to_vec(), 1.0);
        buf.del("z1".into());
        buf.zset_add("z1".into(), b"new".to_vec(), 2.0);
        assert!(buf.cleared_keys.contains("z1"));
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
        assert!(buf.cleared_keys.is_empty());
    }

    #[test]
    fn drain_del_then_hash_set_clears_stale_fields() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"old".to_vec());
        buf.del("h1".into());
        buf.hash_set("h1".into(), "f2", b"new".to_vec());
        assert!(buf.cleared_keys.contains("h1"));
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
        assert!(buf.cleared_keys.is_empty());
    }

    #[test]
    fn drain_del_only_no_rewrite_not_in_cleared() {
        let mut buf = WriteBuffer::new();
        buf.del("k1".into());
        // cleared_keys tracks it, but KeyState is still Deleted
        assert!(buf.cleared_keys.contains("k1"));
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
        assert!(buf.cleared_keys.is_empty());
    }

    #[test]
    fn string_set_after_del_not_affected() {
        // SET is a full overwrite in Redis, so cleared_keys is tracked
        // but drain_into_pipeline skips DEL for StringValue.
        let mut buf = WriteBuffer::new();
        buf.string_set("k1".into(), b"v1".to_vec());
        buf.del("k1".into());
        buf.string_set("k1".into(), b"v2".to_vec());
        assert!(buf.cleared_keys.contains("k1"));
        let mut pipeline = redis::pipe();
        buf.drain_into_pipeline(&mut pipeline);
        assert!(buf.keys.is_empty());
        assert!(buf.cleared_keys.is_empty());
    }

    // --- GarnetSessionState unit tests ---

    use shared_tests::redis_helpers::setup_redis;

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn begin_creates_buffer() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        state.begin().expect("begin should succeed");
        assert!(state.lock().expect("lock").is_some());
        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn begin_twice_nests() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        state.begin().expect("first begin");
        state.begin().expect("second begin should succeed (nested)");
        // Buffer should still be present
        assert!(state.lock().expect("lock").is_some());
        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn rollback_clears_buffer() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        state.begin().expect("begin");
        state.rollback().expect("rollback");
        assert!(state.lock().expect("lock").is_none());
        redis.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn drop_warns_on_active_session() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        state.begin().expect("begin");
        drop(state); // Should warn but not panic
        redis.cleanup().await;
    }

    // --- Issue 1: cleared_keys / full_replace tests ---

    #[test]
    fn set_del_then_add_is_member_unknown_returns_false() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"m1".to_vec());
        buf.del("s1".into());
        buf.set_add("s1".into(), b"m2".to_vec());
        // m1 is not in the new set — should return Found(false), not NotInBuffer
        match buf.set_is_member("s1", b"m1") {
            BufferReadResult::Found(false) => {}
            other => panic!("expected Found(false), got {other:?}"),
        }
    }

    #[test]
    fn set_del_then_add_get_deltas_full_replace() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"old".to_vec());
        buf.del("s1".into());
        buf.set_add("s1".into(), b"new".to_vec());
        match buf.set_get_deltas("s1") {
            BufferReadResult::Found(deltas) => {
                assert!(
                    deltas.full_replace,
                    "full_replace should be true after del+add"
                );
                assert!(deltas.added.contains(b"new".as_slice()));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn set_no_del_get_deltas_not_full_replace() {
        let mut buf = WriteBuffer::new();
        buf.set_add("s1".into(), b"m1".to_vec());
        match buf.set_get_deltas("s1") {
            BufferReadResult::Found(deltas) => {
                assert!(
                    !deltas.full_replace,
                    "full_replace should be false without del"
                );
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn zset_del_then_add_get_deltas_full_replace() {
        let mut buf = WriteBuffer::new();
        buf.zset_add("z1".into(), b"old".to_vec(), 1.0);
        buf.del("z1".into());
        buf.zset_add("z1".into(), b"new".to_vec(), 2.0);
        match buf.zset_get_deltas("z1") {
            BufferReadResult::Found(deltas) => {
                assert!(
                    deltas.full_replace,
                    "full_replace should be true after del+add"
                );
                assert_eq!(deltas.added.get(b"new".as_slice()), Some(&2.0));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn hash_del_then_set_get_unknown_field_returns_deleted() {
        let mut buf = WriteBuffer::new();
        buf.hash_set("h1".into(), "f1", b"v1".to_vec());
        buf.del("h1".into());
        buf.hash_set("h1".into(), "f2", b"v2".to_vec());
        // f1 is not in the new hash — should return KeyDeleted
        assert!(matches!(
            buf.hash_get("h1", "f1"),
            BufferReadResult::KeyDeleted
        ));
        // f2 should be found
        match buf.hash_get("h1", "f2") {
            BufferReadResult::Found(v) => assert_eq!(v, b"v2"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // --- Nesting tests ---

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn nested_begin_commit() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection.clone());

        state.begin().expect("outer begin"); // depth 0→1
        state.begin().expect("inner begin"); // depth 1→2

        {
            let mut guard = state.lock().expect("lock");
            guard
                .as_mut()
                .unwrap()
                .string_set("test:nest:k1".into(), b"v1".to_vec());
        }

        state.commit().await.expect("inner commit"); // depth 2→1 (no-op)

        // Write checkpoint between inner commit and outer commit
        {
            let mut guard = state.lock().expect("lock");
            guard
                .as_mut()
                .unwrap()
                .string_set("test:nest:k2".into(), b"v2".to_vec());
        }

        state.commit().await.expect("outer commit"); // depth 1→0 (real commit)

        // Verify both keys persisted in Redis
        let mut con = connection.clone();
        let v1: Option<Vec<u8>> = redis::cmd("GET")
            .arg("test:nest:k1")
            .query_async(&mut con)
            .await
            .unwrap();
        let v2: Option<Vec<u8>> = redis::cmd("GET")
            .arg("test:nest:k2")
            .query_async(&mut con)
            .await
            .unwrap();
        assert_eq!(v1.as_deref(), Some(b"v1".as_slice()));
        assert_eq!(v2.as_deref(), Some(b"v2".as_slice()));

        redis_guard.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn outer_rollback_after_inner_commit() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection.clone());

        state.begin().expect("outer begin");
        state.begin().expect("inner begin");

        {
            let mut guard = state.lock().expect("lock");
            guard
                .as_mut()
                .unwrap()
                .string_set("test:rollback:k1".into(), b"v1".to_vec());
        }

        state.commit().await.expect("inner commit"); // depth 2→1 (no-op)
        state.rollback().expect("outer rollback"); // depth 1→0, buffer dropped

        // Nothing should be in Redis
        let mut con = connection.clone();
        let v: Option<Vec<u8>> = redis::cmd("GET")
            .arg("test:rollback:k1")
            .query_async(&mut con)
            .await
            .unwrap();
        assert!(v.is_none());

        redis_guard.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn inner_rollback_aborts_all() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);

        state.begin().expect("outer begin");
        state.begin().expect("inner begin");

        {
            let mut guard = state.lock().expect("lock");
            guard
                .as_mut()
                .unwrap()
                .string_set("test:innerrb:k1".into(), b"v1".to_vec());
        }

        state.rollback().expect("inner rollback"); // depth→0, buffer dropped

        // Buffer should be None
        assert!(state.lock().expect("lock").is_none());
        // Commit at depth 0 should error
        assert!(state.commit().await.is_err());
        // Rollback at depth 0 should be a no-op
        assert!(state.rollback().is_ok());

        redis_guard.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn triple_nesting() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection.clone());

        state.begin().expect("begin 1"); // depth 1
        state.begin().expect("begin 2"); // depth 2
        state.begin().expect("begin 3"); // depth 3

        {
            let mut guard = state.lock().expect("lock");
            guard
                .as_mut()
                .unwrap()
                .string_set("test:triple:k1".into(), b"v1".to_vec());
        }

        state.commit().await.expect("commit 3"); // depth 3→2
        state.commit().await.expect("commit 2"); // depth 2→1
        state.commit().await.expect("commit 1"); // depth 1→0 (real commit)

        let mut con = connection.clone();
        let v: Option<Vec<u8>> = redis::cmd("GET")
            .arg("test:triple:k1")
            .query_async(&mut con)
            .await
            .unwrap();
        assert_eq!(v.as_deref(), Some(b"v1".as_slice()));

        redis_guard.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn commit_at_depth_zero_errors() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        assert!(state.commit().await.is_err());
        redis_guard.cleanup().await;
    }

    #[allow(clippy::unwrap_used)]
    #[tokio::test]
    async fn rollback_at_depth_zero_is_noop() {
        let redis_guard = setup_redis().await;
        let client = redis::Client::open(redis_guard.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        assert!(state.rollback().is_ok());
        redis_guard.cleanup().await;
    }
}
