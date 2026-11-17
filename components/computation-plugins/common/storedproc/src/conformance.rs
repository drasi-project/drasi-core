// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The cross-backend protocol table and an in-memory [`Session`].
//!
//! [`run_protocol_table`] drives the shared [`Delivery`] driver through every
//! protocol rule against a [`Backend`]: the in-memory session here, and the
//! PostgreSQL, MySQL and SQL Server sessions in their live suites. Every
//! backend must give the same answers, so their implementations cannot drift.

use crate::{
    cursor::{CursorRow, Position, NO_PENDING, PROGRESS_FORMAT_VERSION},
    driver::{CommitError, Connector, Delivery, ErrorClass, Outcome, Session, TransactionEnded},
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    delivery_failure_class, DeliveryFailureClass, PermanentFailurePolicy,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// A procedure command the table asks a backend for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Records the tag in the effect log.
    Effect,
    /// The procedure rejects the operation with a deterministic error.
    Reject,
    /// A transient failure: a deadlock or serialization failure.
    Transient,
    /// Records the tag, then commits the delivery transaction itself.
    Commit,
    /// Records the tag, then rolls the delivery transaction back itself.
    Rollback,
}

/// A database under test, with a procedure for each [`Probe`].
#[async_trait]
pub trait Backend: Send {
    type Connector: Connector;
    /// Empties the progress, dead-letter and effect tables and returns a
    /// connector whose sessions own their rows as one fixed owner.
    async fn reset(&mut self) -> Result<Self::Connector>;
    /// The command for `probe` recording `tag`.
    fn command(
        &self,
        probe: Probe,
        tag: &str,
    ) -> <<Self::Connector as Connector>::Session as Session>::Command;
    /// The committed effect log, in order.
    async fn effects(&mut self) -> Result<Vec<String>>;
    /// The delivery IDs recorded in the dead-letter table.
    async fn dead_letters(&mut self) -> Result<Vec<String>>;
    /// The owner's cursor rows.
    async fn cursor_rows(&mut self) -> Result<u64>;
    /// The completed prefix of `stream`'s row.
    async fn completed(&mut self, stream: &str) -> Result<Option<i64>>;
}

/// The expected answer to one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Applied,
    Duplicate,
    Skipped,
    Transient,
    /// Permanent; the text must appear in the error.
    Permanent(&'static str),
    /// Applied when the earlier transaction-control attempt left nothing
    /// committed (PostgreSQL refuses it outright); otherwise permanent with
    /// the text, because the escaped effect must not run again.
    AppliedUnlessEscaped(&'static str),
}

/// How many effects a case leaves committed.
#[derive(Clone, Copy, Debug)]
pub enum Effects {
    Exactly(usize),
    /// A command that ended the transaction may have committed its effect
    /// (MySQL, SQL Server) or not (PostgreSQL refuses it); never twice.
    AtMost(usize),
}

/// One delivery in a case.
#[derive(Clone, Debug)]
pub struct Step {
    pub stream: &'static str,
    pub sequence: u64,
    pub operation: i64,
    pub operations: i64,
    pub digest: u8,
    pub epoch: &'static str,
    pub generation: i64,
    pub probe: Option<Probe>,
    pub expect: Expect,
}

const fn step(
    sequence: u64,
    operation: i64,
    operations: i64,
    probe: Probe,
    expect: Expect,
) -> Step {
    Step {
        stream: STREAM_A,
        sequence,
        operation,
        operations,
        digest: 1,
        epoch: "",
        generation: 0,
        probe: Some(probe),
        expect,
    }
}

impl Step {
    const fn stream(mut self, stream: &'static str) -> Self {
        self.stream = stream;
        self
    }
    const fn digest(mut self, digest: u8) -> Self {
        self.digest = digest;
        self
    }
    const fn epoch(mut self, epoch: &'static str) -> Self {
        self.epoch = epoch;
        self
    }
    const fn generation(mut self, generation: i64) -> Self {
        self.generation = generation;
        self
    }
    const fn without_command(mut self) -> Self {
        self.probe = None;
        self
    }

    fn position(&self) -> Position {
        Position {
            stream: self.stream.to_owned(),
            sequence: self.sequence,
            operation: self.operation,
            operations: self.operations,
            digest: [self.digest; 32],
            epoch: self.epoch.to_owned(),
            generation: self.generation,
            delivery_id: format!(
                "drasi-delivery-v1-{}-{}-{}-{}-{}",
                &self.stream[self.stream.len() - 4..],
                self.generation,
                self.epoch.get(..4).unwrap_or("-"),
                self.sequence,
                self.operation
            ),
        }
    }

    fn tag(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            &self.stream[self.stream.len() - 4..],
            self.epoch.get(..4).unwrap_or("-"),
            self.generation,
            self.sequence,
            self.operation
        )
    }
}

/// One case of the table, run on empty tables.
pub struct Case {
    pub name: &'static str,
    pub policy: PermanentFailurePolicy,
    pub max_streams: u32,
    pub steps: Vec<Step>,
    pub effects: Effects,
    pub dead_letters: usize,
    pub rows: u64,
}

pub const STREAM_A: &str =
    "drasi-delivery-stream-v1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const STREAM_B: &str =
    "drasi-delivery-stream-v1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EPOCH_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const EPOCH_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// The protocol table.
pub fn protocol_table() -> Vec<Case> {
    use Expect::*;
    use PermanentFailurePolicy::{Skip, Stall};
    use Probe::*;
    let case = |name, policy, steps, effects, dead_letters, rows| Case {
        name,
        policy,
        max_streams: 8,
        steps,
        effects,
        dead_letters,
        rows,
    };
    let mut table = vec![
        case(
            "operations apply once and replays are duplicates",
            Stall,
            vec![
                step(1, 0, 3, Effect, Applied),
                step(1, 1, 3, Effect, Applied),
                step(1, 0, 3, Effect, Duplicate),
                step(1, 2, 3, Effect, Applied),
                step(1, 2, 3, Effect, Duplicate),
                step(2, 0, 1, Effect, Applied),
                step(3, 0, 1, Effect, Applied).without_command(),
            ],
            Effects::Exactly(4),
            0,
            1,
        ),
        case(
            "gaps, missing prefixes, expired and conflicting receipts are refused",
            Skip,
            vec![
                step(1, 1, 2, Effect, Permanent("missing its first operation")),
                step(1, 0, 2, Effect, Applied),
                step(1, 0, 2, Effect, Permanent("conflicts")).digest(2),
                step(2, 0, 1, Effect, Permanent("unfinished operations")),
                step(1, 1, 2, Effect, Applied),
                step(3, 1, 2, Effect, Permanent("unfinished operations")),
                step(3, 0, 1, Effect, Applied),
                step(2, 0, 1, Effect, Permanent("receipt expired")),
            ],
            Effects::Exactly(3),
            0,
            1,
        ),
        case(
            "a procedure rejection stalls without advancing the cursor",
            Stall,
            vec![
                step(1, 0, 2, Effect, Applied),
                step(1, 1, 2, Reject, Permanent("procedure")),
                step(1, 1, 2, Reject, Permanent("procedure")),
                step(1, 1, 2, Effect, Applied),
            ],
            Effects::Exactly(2),
            0,
            1,
        ),
        case(
            "a procedure rejection under skip is recorded once and never repeated",
            Skip,
            vec![
                step(1, 0, 3, Reject, Skipped),
                step(1, 0, 3, Reject, Duplicate),
                step(1, 1, 3, Effect, Applied),
                step(1, 2, 3, Reject, Skipped),
                step(1, 2, 3, Effect, Duplicate),
                step(2, 0, 1, Effect, Applied),
            ],
            Effects::Exactly(2),
            2,
            1,
        ),
        case(
            "a transient failure is retried by the caller and applies once",
            Stall,
            vec![
                step(1, 0, 1, Probe::Transient, Expect::Transient),
                step(1, 0, 1, Effect, Applied),
                step(1, 0, 1, Effect, Duplicate),
            ],
            Effects::Exactly(1),
            0,
            1,
        ),
        case(
            "a transient failure is never skipped",
            Skip,
            vec![
                step(1, 0, 1, Probe::Transient, Expect::Transient),
                step(1, 0, 1, Effect, Applied),
            ],
            Effects::Exactly(1),
            0,
            1,
        ),
        case(
            "a procedure that commits is refused and never run twice",
            Stall,
            vec![
                step(1, 0, 2, Effect, Applied),
                step(1, 1, 2, Commit, Permanent("delivery transaction")),
                step(1, 1, 2, Commit, Permanent("delivery transaction")),
                step(
                    1,
                    1,
                    2,
                    Effect,
                    AppliedUnlessEscaped("delivery transaction"),
                ),
            ],
            Effects::AtMost(2),
            0,
            1,
        ),
        case(
            "a procedure that commits on a new stream is refused and never run twice",
            Stall,
            vec![
                step(1, 0, 1, Commit, Permanent("delivery transaction")),
                step(1, 0, 1, Commit, Permanent("delivery transaction")),
            ],
            Effects::AtMost(1),
            0,
            1,
        ),
        case(
            "a procedure that rolls back is refused without advancing the cursor",
            Stall,
            vec![
                step(1, 0, 2, Rollback, Permanent("delivery transaction")),
                step(1, 0, 2, Effect, Applied),
                step(1, 1, 2, Rollback, Permanent("delivery transaction")),
                step(1, 1, 2, Effect, Applied),
            ],
            Effects::Exactly(2),
            0,
            1,
        ),
        case(
            "transaction control under skip is settled once",
            Skip,
            vec![
                step(1, 0, 2, Commit, Skipped),
                step(1, 0, 2, Effect, Duplicate),
                step(1, 1, 2, Rollback, Skipped),
                step(1, 1, 2, Effect, Duplicate),
            ],
            Effects::AtMost(1),
            2,
            1,
        ),
        case(
            "stream capacity is enforced and never skipped",
            Skip,
            vec![
                step(1, 0, 1, Effect, Applied),
                step(1, 0, 1, Effect, Permanent("capacity")).stream(STREAM_B),
                step(2, 0, 1, Effect, Applied),
            ],
            Effects::Exactly(2),
            0,
            1,
        ),
        case(
            "a new producer epoch starts a fresh cursor on the same row",
            Stall,
            vec![
                step(1, 0, 1, Effect, Applied).epoch(EPOCH_1),
                step(1, 0, 1, Effect, Duplicate).epoch(EPOCH_1),
                step(1, 0, 1, Effect, Applied).epoch(EPOCH_2),
                step(5, 0, 1, Effect, Permanent("different delivery guarantee")),
            ],
            Effects::Exactly(2),
            0,
            1,
        ),
        case(
            "a newer query generation reuses the row and an older one is refused",
            Stall,
            vec![
                step(7, 0, 1, Effect, Applied).generation(1),
                step(1, 0, 2, Effect, Applied).generation(2),
                step(8, 0, 1, Effect, Permanent("superseded")).generation(1),
                step(1, 1, 2, Effect, Applied).generation(2),
            ],
            Effects::Exactly(3),
            0,
            1,
        ),
    ];
    // Repeated query resets never add cursor rows, even with one stream.
    let mut resets = Case {
        name: "repeated query resets do not exhaust maxStreams",
        policy: Stall,
        max_streams: 1,
        steps: Vec::new(),
        effects: Effects::Exactly(20),
        dead_letters: 0,
        rows: 1,
    };
    for generation in 1..=20 {
        resets
            .steps
            .push(step(1, 0, 1, Effect, Applied).generation(generation));
    }
    table.push(resets);
    if let Some(capacity) = table
        .iter_mut()
        .find(|case| case.name.starts_with("stream capacity"))
    {
        capacity.max_streams = 1;
    }
    table
}

/// Runs every case of [`protocol_table`] against `backend`.
pub async fn run_protocol_table<B: Backend>(backend: &mut B) -> Result<()> {
    for case in protocol_table() {
        run_case(backend, &case)
            .await
            .with_context(|| format!("protocol case '{}'", case.name))?;
    }
    Ok(())
}

async fn run_case<B: Backend>(backend: &mut B, case: &Case) -> Result<()> {
    let connector = backend.reset().await?;
    let mut delivery = Delivery::new(connector, case.max_streams, case.policy);
    delivery.prepare().await?;
    for (index, step) in case.steps.iter().enumerate() {
        let position = step.position();
        let command = step.probe.map(|probe| backend.command(probe, &step.tag()));
        let before = backend.completed(step.stream).await?;
        let mut command = Some(command);
        let mut render = || Ok(command.take().flatten());
        let result = delivery.deliver(&position, &mut render).await;
        let actual = match &result {
            Ok(Outcome::Applied) if matches!(step.expect, Expect::AppliedUnlessEscaped(_)) => {
                step.expect
            }
            Ok(Outcome::Applied) => Expect::Applied,
            Ok(Outcome::Duplicate) => Expect::Duplicate,
            Ok(Outcome::Skipped) => anyhow::bail!("step {index}: skips are reported as errors"),
            Err(error) => match delivery_failure_class(error) {
                Some(DeliveryFailureClass::Skipped) => Expect::Skipped,
                Some(DeliveryFailureClass::Transient { .. }) => Expect::Transient,
                Some(DeliveryFailureClass::Permanent) => match step.expect {
                    Expect::Permanent(text) | Expect::AppliedUnlessEscaped(text)
                        if format!("{error:#}").contains(text) =>
                    {
                        step.expect
                    }
                    _ => Expect::Permanent("unexpected"),
                },
                None => anyhow::bail!("step {index}: unclassified failure {error:#}"),
            },
        };
        ensure!(
            actual == step.expect,
            "step {index} ({step:?}): expected {:?}, got {actual:?} ({:?})",
            step.expect,
            result.as_ref().err().map(|error| format!("{error:#}"))
        );
        if matches!(actual, Expect::Permanent(_) | Expect::Transient)
            || (matches!(actual, Expect::AppliedUnlessEscaped(_)) && result.is_err())
        {
            // An escaped effect may leave a new row behind, never progress.
            ensure!(
                backend.completed(step.stream).await?.unwrap_or(0) == before.unwrap_or(0),
                "step {index}: a failed operation advanced the cursor"
            );
        }
    }
    let effects = backend.effects().await?;
    let mut unique = effects.clone();
    unique.sort();
    unique.dedup();
    ensure!(
        unique.len() == effects.len(),
        "duplicate effects: {effects:?}"
    );
    match case.effects {
        Effects::Exactly(count) => ensure!(effects.len() == count, "effects: {effects:?}"),
        Effects::AtMost(count) => ensure!(effects.len() <= count, "effects: {effects:?}"),
    }
    let dead_letters = backend.dead_letters().await?;
    ensure!(
        dead_letters.len() == case.dead_letters,
        "dead letters: {dead_letters:?}"
    );
    let rows = backend.cursor_rows().await?;
    match case.effects {
        Effects::Exactly(_) => ensure!(rows == case.rows, "cursor rows: {rows}"),
        // A refused transaction-control command may leave no row at all.
        Effects::AtMost(_) => ensure!(rows <= case.rows, "cursor rows: {rows}"),
    }
    delivery.close(std::time::Duration::from_secs(5)).await?;
    Ok(())
}

/// A TCP proxy that loses the reply to one command: once armed, the next
/// client chunk containing `needle` (for example a plaintext `COMMIT`) is
/// forwarded to the server, and then the connection is cut without
/// forwarding the server's answer. The server completes the command; the
/// client sees a lost connection, exactly like a crash after COMMIT was
/// sent.
pub struct ReplyDropper {
    port: u16,
    armed: Arc<std::sync::atomic::AtomicBool>,
    cuts: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ReplyDropper {
    pub async fn start(target: std::net::SocketAddr, needle: &'static [u8]) -> Result<Self> {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let armed = Arc::new(AtomicBool::new(false));
        let cuts = Arc::new(AtomicUsize::new(0));
        let (task_armed, task_cuts) = (armed.clone(), cuts.clone());
        let task = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::TcpStream::connect(target).await else {
                    continue;
                };
                tokio::spawn(Self::relay(
                    client,
                    server,
                    needle,
                    task_armed.clone(),
                    task_cuts.clone(),
                ));
            }
        });
        Ok(Self {
            port,
            armed,
            cuts,
            task,
        })
    }

    async fn relay(
        client: tokio::net::TcpStream,
        server: tokio::net::TcpStream,
        needle: &'static [u8],
        armed: Arc<std::sync::atomic::AtomicBool>,
        cuts: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client_read, mut client_write) = client.into_split();
        let (mut server_read, mut server_write) = server.into_split();
        let cut = Arc::new(AtomicBool::new(false));
        let upstream_cut = cut.clone();
        let upstream = async move {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let read = match client_read.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let chunk = &buffer[..read];
                let hit = armed.load(Ordering::Acquire)
                    && chunk.windows(needle.len()).any(|window| window == needle);
                if hit {
                    armed.store(false, Ordering::Release);
                    upstream_cut.store(true, Ordering::Release);
                }
                if server_write.write_all(chunk).await.is_err() {
                    return;
                }
                if hit {
                    // Let the server finish the command, then cut.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    cuts.fetch_add(1, Ordering::AcqRel);
                    return;
                }
            }
        };
        let downstream = async move {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let read = match server_read.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                if cut.load(Ordering::Acquire) {
                    // Never forward the answer to the cut command.
                    std::future::pending::<()>().await;
                }
                if client_write.write_all(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        };
        tokio::select! {
            _ = upstream => {}
            _ = downstream => {}
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Loses the reply to the next command containing the needle.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::Release);
    }

    /// How many replies were lost.
    pub fn cuts(&self) -> usize {
        self.cuts.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for ReplyDropper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The in-memory database behind [`MemorySession`].
#[derive(Default, Clone)]
pub struct MemoryState {
    pub rows: BTreeMap<String, CursorRow>,
    pub effects: Vec<String>,
    pub dead_letters: Vec<String>,
    /// The next COMMIT applies but its reply is lost.
    pub lose_commit_reply: bool,
    /// The next COMMIT is refused definitively.
    pub reject_commit: bool,
}

/// A procedure command for [`MemorySession`].
#[derive(Clone, Debug)]
pub struct MemoryCommand {
    pub probe: Probe,
    pub tag: String,
}

#[derive(Debug, thiserror::Error)]
#[error("memory database error: {0}")]
pub struct MemoryError(&'static str);

/// A transient memory error.
#[derive(Debug, thiserror::Error)]
#[error("memory database deadlock")]
pub struct MemoryDeadlock;

/// A [`Session`] over shared in-memory state with transaction semantics.
pub struct MemorySession {
    state: Arc<Mutex<MemoryState>>,
    staged: Option<MemoryState>,
}

impl MemorySession {
    fn staged(&mut self) -> Result<&mut MemoryState> {
        self.staged
            .as_mut()
            .ok_or_else(|| MemoryError("no transaction").into())
    }
}

#[async_trait]
impl Session for MemorySession {
    type Command = MemoryCommand;

    fn classify(error: &anyhow::Error) -> ErrorClass {
        if error.chain().any(|cause| cause.is::<MemoryDeadlock>()) {
            ErrorClass::Transient { reconnect: false }
        } else {
            ErrorClass::Permanent
        }
    }

    async fn prepare(&mut self) -> Result<()> {
        Ok(())
    }
    async fn begin(&mut self) -> Result<()> {
        ensure!(self.staged.is_none(), "transaction already open");
        self.staged = Some(self.state.lock().expect("state").clone());
        Ok(())
    }
    async fn read_cursor(&mut self, stream: &str) -> Result<Option<CursorRow>> {
        Ok(self.staged()?.rows.get(stream).cloned())
    }
    async fn lock_admission(&mut self) -> Result<()> {
        self.staged().map(drop)
    }
    async fn count_streams(&mut self) -> Result<i64> {
        Ok(self.staged()?.rows.len() as i64)
    }
    async fn insert_cursor(&mut self, position: &Position) -> Result<()> {
        let row = row(position);
        let staged = self.staged()?;
        ensure!(
            staged.rows.insert(position.stream.clone(), row).is_none(),
            MemoryError("duplicate key")
        );
        Ok(())
    }
    async fn rollover_cursor(&mut self, position: &Position) -> Result<()> {
        let row = row(position);
        self.staged()?.rows.insert(position.stream.clone(), row);
        Ok(())
    }
    async fn mark_pending(&mut self, stream: &str, operation: i64) -> Result<()> {
        let row = self
            .staged()?
            .rows
            .get_mut(stream)
            .ok_or(MemoryError("missing row"))?;
        row.pending = operation;
        Ok(())
    }
    async fn execute(&mut self, command: &MemoryCommand) -> Result<()> {
        match command.probe {
            Probe::Effect => self.staged()?.effects.push(command.tag.clone()),
            Probe::Reject => return Err(MemoryError("procedure rejected the change").into()),
            Probe::Transient => return Err(MemoryDeadlock.into()),
            Probe::Commit => {
                let mut staged = self.staged.take().ok_or(MemoryError("no transaction"))?;
                staged.effects.push(command.tag.clone());
                *self.state.lock().expect("state") = staged;
                return Err(TransactionEnded.into());
            }
            Probe::Rollback => {
                self.staged = None;
                return Err(TransactionEnded.into());
            }
        }
        Ok(())
    }
    async fn record_skip(&mut self, position: &Position, _error: &str) -> Result<()> {
        let id = position.delivery_id.clone();
        let staged = self.staged()?;
        if !staged.dead_letters.contains(&id) {
            staged.dead_letters.push(id);
        }
        Ok(())
    }
    async fn advance_cursor(&mut self, stream: &str, completed: i64) -> Result<u64> {
        let Some(staged) = self.staged.as_mut() else {
            return Err(TransactionEnded.into());
        };
        match staged.rows.get_mut(stream) {
            Some(row) if row.completed == completed => {
                row.completed += 1;
                row.pending = NO_PENDING;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
    async fn commit(&mut self) -> std::result::Result<(), CommitError> {
        let staged = self
            .staged
            .take()
            .ok_or_else(|| CommitError::Rejected(MemoryError("no transaction").into()))?;
        let mut state = self.state.lock().expect("state");
        if std::mem::take(&mut state.reject_commit) {
            return Err(CommitError::Rejected(
                MemoryError("deferred constraint violated").into(),
            ));
        }
        let lost = std::mem::take(&mut state.lose_commit_reply);
        *state = MemoryState {
            lose_commit_reply: false,
            reject_commit: false,
            ..staged
        };
        if lost {
            return Err(CommitError::Unknown(MemoryError("connection lost").into()));
        }
        Ok(())
    }
    async fn rollback(&mut self) -> Result<()> {
        self.staged = None;
        Ok(())
    }
    async fn erase(&mut self) -> Result<u64> {
        let mut state = self.state.lock().expect("state");
        let erased = state.rows.len() as u64;
        state.rows.clear();
        state.dead_letters.clear();
        Ok(erased)
    }
    async fn close(self) -> Result<()> {
        Ok(())
    }
}

fn row(position: &Position) -> CursorRow {
    CursorRow {
        version: PROGRESS_FORMAT_VERSION,
        sequence: position.sequence.to_string(),
        digest: position.digest.to_vec(),
        operations: position.operations,
        completed: 0,
        epoch: position.epoch.clone(),
        generation: position.generation,
        pending: 0,
    }
}

/// Opens [`MemorySession`]s over one shared state.
#[derive(Clone, Default)]
pub struct MemoryConnector {
    pub state: Arc<Mutex<MemoryState>>,
    pub connects: Arc<Mutex<usize>>,
}

#[async_trait]
impl Connector for MemoryConnector {
    type Session = MemorySession;
    async fn connect(&mut self) -> Result<MemorySession> {
        *self.connects.lock().expect("connects") += 1;
        Ok(MemorySession {
            state: self.state.clone(),
            staged: None,
        })
    }
}

/// The in-memory [`Backend`].
#[derive(Default)]
pub struct MemoryBackend {
    connector: MemoryConnector,
}

impl MemoryBackend {
    pub fn state(&self) -> Arc<Mutex<MemoryState>> {
        self.connector.state.clone()
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    type Connector = MemoryConnector;
    async fn reset(&mut self) -> Result<MemoryConnector> {
        self.connector = MemoryConnector::default();
        Ok(self.connector.clone())
    }
    fn command(&self, probe: Probe, tag: &str) -> MemoryCommand {
        MemoryCommand {
            probe,
            tag: tag.to_owned(),
        }
    }
    async fn effects(&mut self) -> Result<Vec<String>> {
        Ok(self.connector.state.lock().expect("state").effects.clone())
    }
    async fn dead_letters(&mut self) -> Result<Vec<String>> {
        Ok(self
            .connector
            .state
            .lock()
            .expect("state")
            .dead_letters
            .clone())
    }
    async fn cursor_rows(&mut self) -> Result<u64> {
        Ok(self.connector.state.lock().expect("state").rows.len() as u64)
    }
    async fn completed(&mut self, stream: &str) -> Result<Option<i64>> {
        Ok(self
            .connector
            .state
            .lock()
            .expect("state")
            .rows
            .get(stream)
            .map(|row| row.completed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn the_memory_session_passes_the_protocol_table() -> Result<()> {
        run_protocol_table(&mut MemoryBackend::default()).await
    }

    fn effect_step(sequence: u64, operation: i64) -> (Position, MemoryCommand) {
        let step = step(sequence, operation, 2, Probe::Effect, Expect::Applied);
        (
            step.position(),
            MemoryCommand {
                probe: Probe::Effect,
                tag: step.tag(),
            },
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_lost_commit_reply_is_resolved_from_the_cursor_without_a_duplicate() -> Result<()> {
        let mut backend = MemoryBackend::default();
        let connector = backend.reset().await?;
        let state = backend.state();
        let mut delivery = Delivery::new(connector.clone(), 8, PermanentFailurePolicy::Stall);
        delivery.prepare().await?;
        let (position, command) = effect_step(1, 0);
        state.lock().unwrap().lose_commit_reply = true;
        let error = delivery
            .deliver(&position, &mut || Ok(Some(command.clone())))
            .await
            .unwrap_err();
        assert!(matches!(
            delivery_failure_class(&error),
            Some(DeliveryFailureClass::Transient { .. })
        ));
        assert!(
            format!("{error:#}").contains("outcome is unknown"),
            "{error:#}"
        );
        assert!(!delivery.connected(), "the session is discarded");
        assert_eq!(
            delivery
                .deliver(&position, &mut || Ok(Some(command.clone())))
                .await?,
            Outcome::Duplicate
        );
        assert_eq!(backend.effects().await?.len(), 1);
        assert_eq!(*connector.connects.lock().unwrap(), 2);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_definite_commit_failure_is_permanent_and_skippable() -> Result<()> {
        for policy in [PermanentFailurePolicy::Stall, PermanentFailurePolicy::Skip] {
            let mut backend = MemoryBackend::default();
            let connector = backend.reset().await?;
            let state = backend.state();
            let mut delivery = Delivery::new(connector, 8, policy);
            let (position, command) = effect_step(1, 0);
            state.lock().unwrap().reject_commit = true;
            let error = delivery
                .deliver(&position, &mut || Ok(Some(command.clone())))
                .await
                .unwrap_err();
            let expected = match policy {
                PermanentFailurePolicy::Stall => DeliveryFailureClass::Permanent,
                PermanentFailurePolicy::Skip => DeliveryFailureClass::Skipped,
            };
            assert_eq!(delivery_failure_class(&error), Some(expected));
            assert!(format!("{error:#}").contains("rejected the delivery commit"));
            assert!(backend.effects().await?.is_empty());
            assert_eq!(
                backend.dead_letters().await?.len(),
                usize::from(policy == PermanentFailurePolicy::Skip)
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_render_failure_is_permanent_and_skippable() -> Result<()> {
        let mut backend = MemoryBackend::default();
        let connector = backend.reset().await?;
        let mut delivery = Delivery::new(connector, 8, PermanentFailurePolicy::Skip);
        let (position, _) = effect_step(1, 0);
        let error = delivery
            .deliver(&position, &mut || anyhow::bail!("missing field after.id"))
            .await
            .unwrap_err();
        assert_eq!(
            delivery_failure_class(&error),
            Some(DeliveryFailureClass::Skipped)
        );
        assert!(format!("{error:#}").contains("failed to render"));
        assert_eq!(backend.completed(STREAM_A).await?, Some(1));
        Ok(())
    }
}
