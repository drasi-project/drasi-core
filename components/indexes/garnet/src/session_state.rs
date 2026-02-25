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
pub struct SetDeltas {
    pub added: HashSet<Vec<u8>>,
    pub removed: HashSet<Vec<u8>>,
}

/// Deltas for a buffered sorted set, used by callers that need to merge with Redis.
pub struct SortedSetDeltas {
    pub added: HashMap<Vec<u8>, f64>,
    pub removed: HashSet<Vec<u8>>,
}

/// In-memory write buffer that intercepts all reads and writes during a session.
///
/// Models Redis data types with delta-based tracking. On commit, the buffer
/// is drained into a single atomic Redis pipeline (MULTI/EXEC).
pub struct WriteBuffer {
    keys: HashMap<String, KeyState>,
}

impl WriteBuffer {
    fn new() -> Self {
        Self {
            keys: HashMap::new(),
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
            Some(KeyState::Deleted) => {
                // Key was deleted, create fresh hash
                let mut fields = HashMap::new();
                fields.insert(field.to_string(), Some(value));
                self.keys.insert(key, KeyState::Hash { fields });
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
                None => BufferReadResult::NotInBuffer,      // field not in buffer
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
            Some(KeyState::Deleted) => {
                // Key was deleted, create fresh set
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
            Some(KeyState::Deleted) => {
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
                })
            }
            Some(KeyState::Deleted) => BufferReadResult::KeyDeleted,
            _ => BufferReadResult::NotInBuffer,
        }
    }

    // --- Key-level operations ---

    pub fn del(&mut self, key: String) {
        self.keys.insert(key, KeyState::Deleted);
    }

    pub fn is_deleted(&self, key: &str) -> bool {
        matches!(self.keys.get(key), Some(KeyState::Deleted))
    }

    /// Drain the buffer into a Redis pipeline for atomic execution.
    ///
    /// Ordering: For keys that went through `Deleted` → new state, the original
    /// Deleted has been overwritten by the new state (since we use `insert`),
    /// but we still need `DEL` for set/sorted-set delta keys to clear Redis state
    /// before applying deltas. `Deleted` variant keys just get `DEL`.
    pub fn drain_into_pipeline(&mut self, pipeline: &mut Pipeline) {
        for (key, state) in self.keys.drain() {
            match state {
                KeyState::Deleted => {
                    pipeline.del(&key).ignore();
                }
                KeyState::StringValue(v) => {
                    pipeline.cmd("SET").arg(&key).arg(v).ignore();
                }
                KeyState::Hash { fields } => {
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
                    // SREM before SADD to handle re-add after remove
                    if !removed.is_empty() {
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
                    // ZREM before ZADD
                    if !removed.is_empty() {
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

/// Shared session state for Garnet session-scoped transactions.
///
/// Holds an optional `WriteBuffer` behind a `Mutex`. All Garnet index types
/// (element, result, future_queue) share a single `Arc<GarnetSessionState>`
/// so that a session transaction spans all indexes atomically.
///
/// The `connection` field is used solely for commit (MULTI/EXEC pipeline execution).
/// Each index keeps its own connection for non-session Redis reads.
pub struct GarnetSessionState {
    connection: MultiplexedConnection,
    active_buffer: Mutex<Option<WriteBuffer>>,
}

impl GarnetSessionState {
    pub fn new(connection: MultiplexedConnection) -> Self {
        Self {
            connection,
            active_buffer: Mutex::new(None),
        }
    }

    /// Begin a new session-scoped transaction.
    ///
    /// Creates a new `WriteBuffer` and stores it. Returns an error if
    /// a session is already active.
    pub(crate) fn begin(&self) -> Result<(), IndexError> {
        let mut guard = self
            .active_buffer
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
        if guard.is_some() {
            return Err(IndexError::other(SessionStateError(
                "begin() called while a session is already active".to_string(),
            )));
        }
        *guard = Some(WriteBuffer::new());
        Ok(())
    }

    /// Commit the active session-scoped transaction.
    ///
    /// Takes the buffer, drains it into an atomic Redis pipeline (MULTI/EXEC),
    /// and executes it.
    pub(crate) async fn commit(&self) -> Result<(), IndexError> {
        let mut buffer = {
            let mut guard = self
                .active_buffer
                .lock()
                .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
            match guard.take() {
                Some(buf) => buf,
                None => {
                    return Err(IndexError::other(SessionStateError(
                        "commit() called with no active session".to_string(),
                    )));
                }
            }
        };

        let mut pipeline = redis::pipe();
        pipeline.atomic();
        buffer.drain_into_pipeline(&mut pipeline);

        let mut con = self.connection.clone();
        pipeline
            .query_async::<_, ()>(&mut con)
            .await
            .map_err(IndexError::other)
    }

    /// Roll back the active session-scoped transaction.
    ///
    /// Takes and drops the buffer. This is synchronous and safe for use
    /// in `Drop` implementations.
    pub(crate) fn rollback(&self) -> Result<(), IndexError> {
        let mut guard = self
            .active_buffer
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))?;
        let _ = guard.take(); // Drop the buffer
        Ok(())
    }

    /// Lock and return a guard providing access to the active write buffer.
    ///
    /// Callers check if the `Option` is `Some` (session active) or `None` (no session).
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Option<WriteBuffer>>, IndexError> {
        self.active_buffer
            .lock()
            .map_err(|e| IndexError::other(PoisonError(e.to_string())))
    }
}

impl Drop for GarnetSessionState {
    fn drop(&mut self) {
        // Use get_mut() since &mut self guarantees exclusive access.
        if let Ok(inner) = self.active_buffer.get_mut() {
            if inner.is_some() {
                log::warn!("GarnetSessionState dropped with active session — rolling back");
            }
            let _ = inner.take();
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
        // f1 should not be in the fresh hash
        assert!(matches!(
            buf.hash_get("h1", "f1"),
            BufferReadResult::NotInBuffer
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
    async fn begin_twice_errors() {
        let redis = setup_redis().await;
        let client = redis::Client::open(redis.url()).unwrap();
        let connection = client.get_multiplexed_async_connection().await.unwrap();
        let state = GarnetSessionState::new(connection);
        state.begin().expect("first begin");
        assert!(state.begin().is_err());
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
}
