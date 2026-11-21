// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Transient timing ownership. The host owns tracker instances and completion;
//! this module does not install instrumentation or enable the deferred metric.

use super::{Result, TelemetryError};
use std::{
    collections::BTreeMap,
    fmt,
    mem::size_of,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::time::Instant;

/// Opaque identity, never an address, clock value, event field, or metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HandledBranchToken {
    pub owner: [u8; 16],
    pub lease: u64,
}

/// Library-local ownership of a transient token. Native implementations must
/// acquire/release through negotiated host callbacks, not pass this trait object.
pub trait HandledBranchLease: fmt::Debug + Send + Sync {
    fn token(&self) -> HandledBranchToken;
    fn acquire(&self, _token: HandledBranchToken) -> Result<HandledBranchContext> {
        Err(TelemetryError::Unsupported("timing lease acquisition"))
    }
    fn retain_token(&self, _token: HandledBranchToken) -> Result<HandledBranchToken> {
        Err(TelemetryError::Unsupported("timing lease transfer"))
    }
    fn release_token(&self, _token: HandledBranchToken) -> Result<()> {
        Err(TelemetryError::Unsupported("timing lease release"))
    }
    fn transport(&self) -> Option<Arc<dyn HandledBranchTransport>> {
        None
    }
}

pub trait HandledBranchTransport: fmt::Debug + Send + Sync {
    fn acquire(&self, token: HandledBranchToken) -> Result<HandledBranchContext>;
    fn keep_reply(&self, _contexts: &[super::ExecutionContext]) -> Result<()> {
        Ok(())
    }
    fn retain_token(&self, _token: HandledBranchToken) -> Result<HandledBranchToken> {
        Err(TelemetryError::Unsupported("timing lease transfer"))
    }
    fn release_token(&self, _token: HandledBranchToken) -> Result<()> {
        Err(TelemetryError::Unsupported("timing lease release"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HandledBranchUnavailable {
    Capacity,
    Unsupported,
    Ambiguous,
    Replay,
    Bootstrap,
    Scheduled,
    Retired,
}

/// Explicit unavailability prevents an ambient context from restoring a timing
/// origin that was intentionally suppressed or lost.
#[derive(Debug, Clone)]
pub enum HandledBranchContext {
    Tracked(Arc<dyn HandledBranchLease>),
    Unavailable(HandledBranchUnavailable),
}

/// Runtime-only attribution selected at the query output boundary. It is not
/// persisted with the event and must never be inferred from a batch trace parent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandledBranchOutput {
    pub query_id: Arc<str>,
    pub producer_epoch: [u8; 16],
    pub sequence: u64,
    pub required_rows: usize,
}

impl HandledBranchContext {
    pub fn token(&self) -> Option<HandledBranchToken> {
        match self {
            Self::Tracked(lease) => Some(lease.token()),
            Self::Unavailable(_) => None,
        }
    }
}

impl PartialEq for HandledBranchContext {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Tracked(one), Self::Tracked(two)) => one.token() == two.token(),
            (Self::Unavailable(one), Self::Unavailable(two)) => one == two,
            _ => false,
        }
    }
}
impl Eq for HandledBranchContext {}

/// Record-data bytes include row bitsets and retained key data. Map/allocator
/// overhead is separately bounded by `max_records`, not claimed as byte-exact RSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandledBranchLimits {
    pub max_records: usize,
    pub max_bytes: usize,
}

impl Default for HandledBranchLimits {
    fn default() -> Self {
        Self {
            max_records: 16_384,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

impl HandledBranchLimits {
    pub fn validate(self) -> Result<Self> {
        if self.max_records < 2 || self.max_bytes < ORIGIN_BYTES + LEASE_BYTES {
            return Err(TelemetryError::Invalid(
                "handled-branch limits must accommodate an origin and its initial lease".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandledBranchDiagnostics {
    pub records: usize,
    pub record_bytes: usize,
    pub capacity_rejections: u64,
    pub invalid_requests: u64,
    pub retired_requests: u64,
    pub sealed_requests: u64,
    pub duplicate_completions: u64,
    pub completed_branches: u64,
    pub missing_origins: u64,
    pub unsupported_origins: u64,
    pub ambiguous_origins: u64,
    pub replay_origins: u64,
    pub excluded_origins: u64,
}

/// Use the validated query producer's epoch and logical output sequence, not a
/// transport retry sequence. Sink generation authorizes the caller separately;
/// replacing a sink must not create another success for the same live branch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HandledBranchKey {
    pub producer_epoch: [u8; 16],
    pub output_sequence: u64,
    pub sink: Arc<str>,
}

#[derive(Debug)]
struct Branch {
    rows: Box<[u64]>,
    required: usize,
    remaining: usize,
    completed: bool,
}

#[derive(Debug)]
struct Origin {
    started: Instant,
    leases: usize,
    sealed: bool,
    branches: BTreeMap<HandledBranchKey, Branch>,
}

#[derive(Debug)]
struct State {
    active: bool,
    next: u64,
    origins: BTreeMap<u64, Origin>,
    leases: BTreeMap<u64, u64>,
    diagnostics: HandledBranchDiagnostics,
}

const ORIGIN_BYTES: usize = size_of::<Origin>() + size_of::<u64>();
const LEASE_BYTES: usize = 2 * size_of::<u64>();
const BRANCH_BYTES: usize = size_of::<HandledBranchKey>() + size_of::<Branch>();
const MAX_SINK_BYTES: usize = 1024;

/// Shared state-machine implementation, instantiated and retained by the owning
/// host graph. Plugins transport leases; they must not instantiate a replacement
/// tracker or measure an origin with their own clock.
#[derive(Debug)]
pub struct HandledBranchTracker {
    owner: [u8; 16],
    limits: HandledBranchLimits,
    state: Mutex<State>,
}

impl HandledBranchTracker {
    pub fn new(limits: HandledBranchLimits) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            owner: *uuid::Uuid::new_v4().as_bytes(),
            limits: limits.validate()?,
            state: Mutex::new(State {
                active: true,
                next: 1,
                origins: BTreeMap::new(),
                leases: BTreeMap::new(),
                diagnostics: Default::default(),
            }),
        }))
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| TelemetryError::Poisoned)
    }

    fn active(state: &mut State) -> Result<()> {
        if !state.active {
            state.diagnostics.retired_requests =
                state.diagnostics.retired_requests.saturating_add(1);
            return Err(TelemetryError::Retired);
        }
        Ok(())
    }

    fn invalid(state: &mut State, message: &'static str) -> TelemetryError {
        state.diagnostics.invalid_requests = state.diagnostics.invalid_requests.saturating_add(1);
        TelemetryError::Invalid(message.into())
    }

    fn origin(&self, state: &mut State, token: HandledBranchToken) -> Result<u64> {
        if token.owner == self.owner {
            if let Some(origin) = state.leases.get(&token.lease) {
                return Ok(*origin);
            }
        }
        Err(Self::invalid(state, "unknown or foreign timing lease"))
    }

    fn reserve(&self, state: &mut State, records: usize, bytes: usize) -> Result<()> {
        let current = &mut state.diagnostics;
        if records > self.limits.max_records - current.records
            || bytes > self.limits.max_bytes - current.record_bytes
        {
            current.capacity_rejections = current.capacity_rejections.saturating_add(1);
            return Err(TelemetryError::Capacity("handled-branch tracking"));
        }
        current.records += records;
        current.record_bytes += bytes;
        Ok(())
    }

    fn next(state: &mut State) -> Result<u64> {
        let id = state.next;
        state.next = id
            .checked_add(1)
            .ok_or(TelemetryError::Capacity("handled-branch lease identities"))?;
        Ok(id)
    }

    pub fn start(self: &Arc<Self>) -> Result<HandledBranchContext> {
        let mut state = self.lock()?;
        Self::active(&mut state)?;
        let id = Self::next(&mut state)?;
        self.reserve(&mut state, 2, ORIGIN_BYTES + LEASE_BYTES)?;
        state.origins.insert(
            id,
            Origin {
                started: Instant::now(),
                leases: 1,
                sealed: false,
                branches: BTreeMap::new(),
            },
        );
        state.leases.insert(id, id);
        Ok(self.owned(HandledBranchToken {
            owner: self.owner,
            lease: id,
        }))
    }

    fn owned(self: &Arc<Self>, token: HandledBranchToken) -> HandledBranchContext {
        HandledBranchContext::Tracked(Arc::new(LocalLease {
            tracker: self.clone(),
            token,
        }))
    }

    pub fn acquire(self: &Arc<Self>, token: HandledBranchToken) -> Result<HandledBranchContext> {
        Ok(self.owned(self.retain_token(token)?))
    }

    /// Transfer API for a negotiated native owner. Every successful acquisition
    /// must eventually be paired with `release_token`, including failed decoding.
    /// The callback context's lifetime is a separate ownership obligation.
    pub fn retain_token(&self, token: HandledBranchToken) -> Result<HandledBranchToken> {
        let mut state = self.lock()?;
        Self::active(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        let id = Self::next(&mut state)?;
        if let Err(error) = self.reserve(&mut state, 1, LEASE_BYTES) {
            state.origins.get_mut(&origin).expect("live origin").sealed = true;
            return Err(error);
        }
        state.leases.insert(id, origin);
        state.origins.get_mut(&origin).expect("live origin").leases += 1;
        Ok(HandledBranchToken {
            owner: self.owner,
            lease: id,
        })
    }

    /// Release remains available after retirement. Unknown/double releases fail
    /// explicitly; no token number is reused by this tracker.
    pub fn release_token(&self, token: HandledBranchToken) -> Result<()> {
        let mut state = self.lock()?;
        let origin = self.origin(&mut state, token)?;
        state.leases.remove(&token.lease);
        state.diagnostics.records -= 1;
        state.diagnostics.record_bytes -= LEASE_BYTES;
        let entry = state.origins.get_mut(&origin).expect("live origin");
        entry.leases -= 1;
        if entry.leases == 0 {
            let origin = state.origins.remove(&origin).expect("last origin");
            state.diagnostics.records -= 1 + origin.branches.len();
            state.diagnostics.record_bytes -= ORIGIN_BYTES
                + origin
                    .branches
                    .iter()
                    .map(|(key, branch)| {
                        BRANCH_BYTES + key.sink.len() + branch.rows.len() * size_of::<u64>()
                    })
                    .sum::<usize>();
        }
        Ok(())
    }

    /// Reserve before effect work. The required non-Noop row set must remain
    /// unchanged across splitting, filtering, retries, and duplicate reports.
    pub fn prepare(
        &self,
        token: HandledBranchToken,
        key: &HandledBranchKey,
        required_rows: usize,
    ) -> Result<()> {
        let mut state = self.lock()?;
        Self::active(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        if required_rows == 0
            || key.sink.is_empty()
            || key.sink.len() > MAX_SINK_BYTES
            || key.producer_epoch == [0; 16]
        {
            return Err(Self::invalid(
                &mut state,
                "invalid handled-branch identity or rows",
            ));
        }
        let entry = state.origins.get(&origin).expect("live origin");
        if let Some(branch) = entry.branches.get(key) {
            return if branch.required == required_rows {
                Ok(())
            } else {
                Err(Self::invalid(
                    &mut state,
                    "handled-branch required rows changed",
                ))
            };
        }
        if entry.sealed {
            state.diagnostics.sealed_requests = state.diagnostics.sealed_requests.saturating_add(1);
            return Err(TelemetryError::Capacity("sealed handled-branch origin"));
        }
        let words = required_rows / 64 + usize::from(required_rows % 64 != 0);
        let bytes = words
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| bytes.checked_add(BRANCH_BYTES + key.sink.len()));
        let admission = match bytes {
            Some(bytes) => self.reserve(&mut state, 1, bytes),
            None => {
                state.diagnostics.capacity_rejections =
                    state.diagnostics.capacity_rejections.saturating_add(1);
                Err(TelemetryError::Capacity("handled-branch row storage"))
            }
        };
        if let Err(error) = admission {
            state.origins.get_mut(&origin).expect("live origin").sealed = true;
            return Err(error);
        }
        state
            .origins
            .get_mut(&origin)
            .expect("live origin")
            .branches
            .insert(
                key.clone(),
                Branch {
                    rows: vec![0; words].into_boxed_slice(),
                    required: required_rows,
                    remaining: required_rows,
                    completed: false,
                },
            );
        Ok(())
    }

    pub fn handled_rows(
        &self,
        token: HandledBranchToken,
        key: &HandledBranchKey,
        rows: &[usize],
    ) -> Result<Option<Duration>> {
        self.complete(token, key, Some(rows))
    }

    /// Positive success of an entire envelope, never enqueue/acknowledgement.
    pub fn handled_all(
        &self,
        token: HandledBranchToken,
        key: &HandledBranchKey,
    ) -> Result<Option<Duration>> {
        self.complete(token, key, None)
    }

    fn complete(
        &self,
        token: HandledBranchToken,
        key: &HandledBranchKey,
        rows: Option<&[usize]>,
    ) -> Result<Option<Duration>> {
        let mut state = self.lock()?;
        Self::active(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        let entry = state.origins.get_mut(&origin).expect("live origin");
        let Some(branch) = entry.branches.get_mut(key) else {
            return Err(Self::invalid(&mut state, "handled-branch was not admitted"));
        };
        if rows.is_some_and(|rows| rows.iter().any(|row| *row >= branch.required)) {
            return Err(Self::invalid(
                &mut state,
                "handled-branch row is out of bounds",
            ));
        }
        if branch.completed {
            state.diagnostics.duplicate_completions =
                state.diagnostics.duplicate_completions.saturating_add(1);
            return Ok(None);
        }
        match rows {
            Some(rows) => {
                for &row in rows {
                    let bit = 1u64 << (row % 64);
                    let word = &mut branch.rows[row / 64];
                    if *word & bit == 0 {
                        *word |= bit;
                        branch.remaining -= 1;
                    }
                }
            }
            None => branch.remaining = 0,
        }
        if branch.remaining != 0 {
            return Ok(None);
        }
        let elapsed = entry.started.elapsed();
        branch.completed = true;
        state.diagnostics.completed_branches =
            state.diagnostics.completed_branches.saturating_add(1);
        Ok(Some(elapsed))
    }

    pub fn retire(&self) -> Result<()> {
        self.lock()?.active = false;
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<HandledBranchDiagnostics> {
        Ok(self.lock()?.diagnostics)
    }
    pub fn unavailable(&self, reason: Option<HandledBranchUnavailable>) -> Result<()> {
        let mut state = self.lock()?;
        let counter = match reason {
            None => &mut state.diagnostics.missing_origins,
            Some(HandledBranchUnavailable::Unsupported) => {
                &mut state.diagnostics.unsupported_origins
            }
            Some(HandledBranchUnavailable::Ambiguous) => &mut state.diagnostics.ambiguous_origins,
            Some(HandledBranchUnavailable::Replay) => &mut state.diagnostics.replay_origins,
            Some(HandledBranchUnavailable::Bootstrap | HandledBranchUnavailable::Scheduled) => {
                &mut state.diagnostics.excluded_origins
            }
            Some(HandledBranchUnavailable::Capacity | HandledBranchUnavailable::Retired) => {
                return Ok(())
            }
        };
        *counter = counter.saturating_add(1);
        Ok(())
    }
}

struct LocalLease {
    tracker: Arc<HandledBranchTracker>,
    token: HandledBranchToken,
}

impl fmt::Debug for LocalLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("HandledBranchLease")
            .field(&self.token)
            .finish()
    }
}

impl HandledBranchLease for LocalLease {
    fn token(&self) -> HandledBranchToken {
        self.token
    }
    fn acquire(&self, token: HandledBranchToken) -> Result<HandledBranchContext> {
        self.tracker.acquire(token)
    }
    fn retain_token(&self, token: HandledBranchToken) -> Result<HandledBranchToken> {
        self.tracker.retain_token(token)
    }
    fn release_token(&self, token: HandledBranchToken) -> Result<()> {
        self.tracker.release_token(token)
    }
    fn transport(&self) -> Option<Arc<dyn HandledBranchTransport>> {
        Some(Arc::new(Resolver(self.tracker.clone())))
    }
}

#[derive(Debug)]
struct Resolver(Arc<HandledBranchTracker>);
impl HandledBranchTransport for Resolver {
    fn acquire(&self, token: HandledBranchToken) -> Result<HandledBranchContext> {
        self.0.acquire(token)
    }
    fn retain_token(&self, token: HandledBranchToken) -> Result<HandledBranchToken> {
        self.0.retain_token(token)
    }
    fn release_token(&self, token: HandledBranchToken) -> Result<()> {
        self.0.release_token(token)
    }
}

impl Drop for LocalLease {
    fn drop(&mut self) {
        if let Err(error) = self.tracker.release_token(self.token) {
            log::error!("Failed to release handled-branch timing lease: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
