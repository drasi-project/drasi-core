// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Isolated Item 7 ownership gate. No production metric or ABI is enabled here.

pub mod protocol;

use protocol::{Host, Token};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem::size_of,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub records: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Closed,
    Invalid,
    Sealed,
    Exhausted,
    Poisoned,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timing ownership: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub records: usize,
    pub bytes: usize,
    pub capacity: u64,
    pub invalid: u64,
    pub closed: u64,
    pub sealed: u64,
    pub duplicates: u64,
    pub observations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BranchKey {
    pub output: u64,
    pub sink: u64,
}

struct Branch {
    rows: Box<[u64]>,
    required: usize,
    remaining: usize,
    finished: bool,
}

struct Origin {
    started: Duration,
    leases: usize,
    sealed: bool,
    branches: BTreeMap<BranchKey, Branch>,
}

struct State {
    open: bool,
    next: u64,
    origins: BTreeMap<u64, Origin>,
    leases: BTreeMap<u64, u64>,
    diagnostics: Diagnostics,
}

enum Clock {
    Live(Instant),
    Controlled(Arc<AtomicU64>),
}

impl Clock {
    fn now(&self) -> Duration {
        match self {
            Self::Live(start) => start.elapsed(),
            Self::Controlled(clock) => Duration::from_nanos(clock.load(Ordering::Acquire)),
        }
    }
}

pub struct Registry {
    owner: [u8; 16],
    limits: Limits,
    clock: Clock,
    state: Mutex<State>,
}

// Account record data and row buffers. Tree/allocator overhead is additionally
// bounded by the record limit; this is deliberately not an RSS measurement.
pub const ORIGIN_BYTES: usize = size_of::<Origin>() + size_of::<u64>();
pub const LEASE_BYTES: usize = 2 * size_of::<u64>();
pub const BRANCH_BYTES: usize = size_of::<Branch>() + size_of::<BranchKey>();

impl Registry {
    pub fn new(limits: Limits) -> Result<Arc<Self>, Error> {
        Self::with_clock(limits, Clock::Live(Instant::now()))
    }

    pub fn controlled(limits: Limits, clock: Arc<AtomicU64>) -> Result<Arc<Self>, Error> {
        Self::with_clock(limits, Clock::Controlled(clock))
    }

    fn with_clock(limits: Limits, clock: Clock) -> Result<Arc<Self>, Error> {
        if limits.records < 2 || limits.bytes < ORIGIN_BYTES + LEASE_BYTES {
            return Err(Error::Invalid);
        }
        let owner = NEXT_OWNER
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
            .map_err(|_| Error::Exhausted)?;
        let mut namespace = [0; 16];
        namespace[8..].copy_from_slice(&owner.to_be_bytes());
        Ok(Arc::new(Self {
            owner: namespace,
            limits,
            clock,
            state: Mutex::new(State {
                open: true,
                next: 1,
                origins: BTreeMap::new(),
                leases: BTreeMap::new(),
                diagnostics: Diagnostics::default(),
            }),
        }))
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state.lock().map_err(|_| Error::Poisoned)
    }

    fn ensure_open(state: &mut State) -> Result<(), Error> {
        if !state.open {
            state.diagnostics.closed = state.diagnostics.closed.saturating_add(1);
            return Err(Error::Closed);
        }
        Ok(())
    }

    fn origin(&self, state: &mut State, token: Token) -> Result<u64, Error> {
        if token.owner == self.owner {
            if let Some(origin) = state.leases.get(&token.lease) {
                return Ok(*origin);
            }
        }
        state.diagnostics.invalid = state.diagnostics.invalid.saturating_add(1);
        Err(Error::Invalid)
    }

    fn reserve(&self, state: &mut State, records: usize, bytes: usize) -> Result<(), Error> {
        let current = &mut state.diagnostics;
        if records > self.limits.records - current.records
            || bytes > self.limits.bytes - current.bytes
        {
            current.capacity = current.capacity.saturating_add(1);
            return Err(Error::Capacity);
        }
        current.records += records;
        current.bytes += bytes;
        Ok(())
    }

    fn next(state: &mut State) -> Result<u64, Error> {
        let id = state.next;
        state.next = id.checked_add(1).ok_or(Error::Exhausted)?;
        Ok(id)
    }

    pub fn start(self: &Arc<Self>) -> Result<Lease, Error> {
        let mut state = self.lock()?;
        Self::ensure_open(&mut state)?;
        let id = Self::next(&mut state)?;
        self.reserve(&mut state, 2, ORIGIN_BYTES + LEASE_BYTES)?;
        state.origins.insert(
            id,
            Origin {
                started: self.clock.now(),
                leases: 1,
                sealed: false,
                branches: BTreeMap::new(),
            },
        );
        state.leases.insert(id, id);
        Ok(Lease(Arc::new(LocalLease {
            registry: self.clone(),
            token: Token {
                owner: self.owner,
                lease: id,
            },
        })))
    }

    pub fn acquire(self: &Arc<Self>, token: Token) -> Result<Lease, Error> {
        let token = self.retain_token(token)?;
        Ok(Lease(Arc::new(LocalLease {
            registry: self.clone(),
            token,
        })))
    }

    fn retain_token(&self, token: Token) -> Result<Token, Error> {
        let mut state = self.lock()?;
        Self::ensure_open(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        let id = Self::next(&mut state)?;
        if let Err(error) = self.reserve(&mut state, 1, LEASE_BYTES) {
            state.origins.get_mut(&origin).expect("live origin").sealed = true;
            return Err(error);
        }
        state.leases.insert(id, origin);
        state.origins.get_mut(&origin).expect("live origin").leases += 1;
        Ok(Token {
            owner: self.owner,
            lease: id,
        })
    }

    fn release_token(&self, token: Token) -> Result<(), Error> {
        let mut state = self.lock()?;
        let origin = self.origin(&mut state, token)?;
        state.leases.remove(&token.lease);
        state.diagnostics.records -= 1;
        state.diagnostics.bytes -= LEASE_BYTES;
        let entry = state.origins.get_mut(&origin).expect("live origin");
        entry.leases -= 1;
        if entry.leases == 0 {
            let origin = state.origins.remove(&origin).expect("last origin");
            state.diagnostics.records -= 1 + origin.branches.len();
            state.diagnostics.bytes -= ORIGIN_BYTES
                + origin
                    .branches
                    .values()
                    .map(|branch| BRANCH_BYTES + branch.rows.len() * size_of::<u64>())
                    .sum::<usize>();
        }
        Ok(())
    }

    pub fn prepare(&self, token: Token, key: BranchKey, rows: usize) -> Result<(), Error> {
        let mut state = self.lock()?;
        Self::ensure_open(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        if rows == 0 {
            state.diagnostics.invalid = state.diagnostics.invalid.saturating_add(1);
            return Err(Error::Invalid);
        }
        let entry = state.origins.get(&origin).expect("live origin");
        if let Some(branch) = entry.branches.get(&key) {
            return if branch.required == rows {
                Ok(())
            } else {
                state.diagnostics.invalid = state.diagnostics.invalid.saturating_add(1);
                Err(Error::Invalid)
            };
        }
        if entry.sealed {
            state.diagnostics.sealed = state.diagnostics.sealed.saturating_add(1);
            return Err(Error::Sealed);
        }
        let words = rows / 64 + usize::from(rows % 64 != 0);
        let bytes = words
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| bytes.checked_add(BRANCH_BYTES));
        let admission = match bytes {
            Some(bytes) => self.reserve(&mut state, 1, bytes),
            None => {
                state.diagnostics.capacity = state.diagnostics.capacity.saturating_add(1);
                Err(Error::Capacity)
            }
        };
        if let Err(error) = admission {
            // No unbounded negative-key cache: seal new branch admission for this
            // origin, but preserve all branches already admitted.
            state.origins.get_mut(&origin).expect("live origin").sealed = true;
            return Err(error);
        }
        state
            .origins
            .get_mut(&origin)
            .expect("live origin")
            .branches
            .insert(
                key,
                Branch {
                    rows: vec![0; words].into_boxed_slice(),
                    required: rows,
                    remaining: rows,
                    finished: false,
                },
            );
        Ok(())
    }

    pub fn handled(
        &self,
        token: Token,
        key: BranchKey,
        rows: &[usize],
    ) -> Result<Option<Duration>, Error> {
        let mut state = self.lock()?;
        Self::ensure_open(&mut state)?;
        let origin = self.origin(&mut state, token)?;
        let now = self.clock.now();
        let entry = state.origins.get_mut(&origin).expect("live origin");
        let elapsed = now.checked_sub(entry.started).ok_or(Error::Invalid)?;
        let Some(branch) = entry.branches.get_mut(&key) else {
            state.diagnostics.invalid = state.diagnostics.invalid.saturating_add(1);
            return Err(Error::Invalid);
        };
        if rows.iter().any(|row| *row >= branch.required) {
            state.diagnostics.invalid = state.diagnostics.invalid.saturating_add(1);
            return Err(Error::Invalid);
        }
        if branch.finished {
            state.diagnostics.duplicates = state.diagnostics.duplicates.saturating_add(1);
            return Ok(None);
        }
        for &row in rows {
            let bit = 1u64 << (row % 64);
            let word = &mut branch.rows[row / 64];
            if *word & bit == 0 {
                *word |= bit;
                branch.remaining -= 1;
            }
        }
        if branch.remaining != 0 {
            return Ok(None);
        }
        branch.finished = true;
        state.diagnostics.observations = state.diagnostics.observations.saturating_add(1);
        Ok(Some(elapsed))
    }

    pub fn retire(&self) -> Result<(), Error> {
        self.lock()?.open = false;
        Ok(())
    }

    pub fn diagnostics(&self) -> Result<Diagnostics, Error> {
        Ok(self.lock()?.diagnostics)
    }

    /// Borrowed for the duration of a foreign call. The foreign owner must
    /// retain the context before storing the table.
    pub fn host(self: &Arc<Self>) -> Host {
        Host {
            context: Arc::as_ptr(self).cast(),
            retain_context,
            release_context,
            retain,
            release,
        }
    }
}

struct LocalLease {
    registry: Arc<Registry>,
    token: Token,
}

impl Drop for LocalLease {
    fn drop(&mut self) {
        if let Err(error) = self.registry.release_token(self.token) {
            eprintln!("Failed to release timing lease: {error}");
        }
    }
}

#[derive(Clone)]
pub struct Lease(Arc<LocalLease>);

impl Lease {
    pub fn token(&self) -> Token {
        self.0.token
    }
}

unsafe extern "C" fn retain_context(context: *const c_void) {
    unsafe { Arc::increment_strong_count(context.cast::<Registry>()) };
}

unsafe extern "C" fn release_context(context: *const c_void) {
    unsafe { Arc::decrement_strong_count(context.cast::<Registry>()) };
}

fn status(error: Error) -> u32 {
    match error {
        Error::Capacity => protocol::FULL,
        Error::Closed => protocol::CLOSED,
        _ => protocol::INVALID,
    }
}

unsafe extern "C" fn retain(context: *const c_void, token: Token, out: *mut Token) -> u32 {
    if context.is_null() || out.is_null() {
        return protocol::INVALID;
    }
    let registry = unsafe { &*context.cast::<Registry>() };
    match registry.retain_token(token) {
        Ok(token) => {
            unsafe { out.write(token) };
            protocol::OK
        }
        Err(error) => status(error),
    }
}

unsafe extern "C" fn release(context: *const c_void, token: Token) -> u32 {
    if context.is_null() {
        return protocol::INVALID;
    }
    let registry = unsafe { &*context.cast::<Registry>() };
    registry
        .release_token(token)
        .map_or_else(status, |()| protocol::OK)
}
