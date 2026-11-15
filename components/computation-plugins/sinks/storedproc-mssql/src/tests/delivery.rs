// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The cursor protocol, reconnect and retry behaviour against an in-memory
//! session that models one SQL Server transaction per operation.

use crate::delivery::{
    classify, classify_server_code, decide, owner, Connector, CursorRow, CursorWrite, Decision,
    Delivery, DeliveryError, FailureClass, Outcome, Position, Session, PROGRESS_FORMAT_VERSION,
};
use anyhow::Result;
use async_trait::async_trait;
use drasi_mssql_common::sql::RenderedCommand;
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

type Rows = BTreeMap<(String, String), CursorRow>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// The connection attempt fails with an I/O error.
    ConnectRefused,
    /// The command fails with a broken connection.
    ExecuteIo,
    /// The command never answers (the host drops the call).
    ExecuteHangs,
    /// COMMIT reaches the server and commits, then the reply is lost.
    CommitLostAfterApply,
    /// The connection dies before COMMIT reaches the server.
    CommitLostBeforeApply,
    /// The procedure commits the delivery transaction itself.
    ProcedureCommits,
}

#[derive(Default)]
struct Database {
    rows: Rows,
    effects: Vec<String>,
    faults: VecDeque<Fault>,
    connects: usize,
    prepares: usize,
}

impl Database {
    fn take(&mut self, fault: Fault) -> bool {
        if self.faults.front() == Some(&fault) {
            self.faults.pop_front();
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Database>>);

impl Fake {
    fn db(&self) -> std::sync::MutexGuard<'_, Database> {
        self.0.lock().expect("fake database")
    }
    fn fault(&self, fault: Fault) {
        self.db().faults.push_back(fault);
    }
    fn effects(&self) -> Vec<String> {
        self.db().effects.clone()
    }
    fn row(&self, stream: &str) -> Option<CursorRow> {
        self.db()
            .rows
            .get(&(owner(super::GRAPH, super::SINK_ID), stream.to_owned()))
            .cloned()
    }
}

fn io() -> anyhow::Error {
    tiberius::error::Error::Io {
        kind: std::io::ErrorKind::ConnectionReset,
        message: "connection reset".into(),
    }
    .into()
}

struct FakeSession {
    fake: Fake,
    /// Uncommitted transaction state.
    transaction: Option<(Rows, Vec<String>)>,
}

impl FakeSession {
    fn tx(&mut self) -> Result<&mut (Rows, Vec<String>)> {
        self.transaction
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no open transaction"))
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn prepare(&mut self) -> Result<()> {
        self.fake.db().prepares += 1;
        Ok(())
    }
    async fn begin(&mut self) -> Result<()> {
        let db = self.fake.db();
        let state = (db.rows.clone(), db.effects.clone());
        drop(db);
        self.transaction = Some(state);
        Ok(())
    }
    async fn read_cursor(&mut self, owner: &str, stream: &str) -> Result<Option<CursorRow>> {
        Ok(self
            .tx()?
            .0
            .get(&(owner.to_owned(), stream.to_owned()))
            .cloned())
    }
    async fn lock_admission(&mut self, _owner: &str) -> Result<()> {
        self.tx()?;
        Ok(())
    }
    async fn count_streams(&mut self, owner: &str) -> Result<i64> {
        Ok(self.tx()?.0.keys().filter(|(o, _)| o == owner).count() as i64)
    }
    async fn insert_cursor(&mut self, owner: &str, position: &Position) -> Result<()> {
        let previous = self.tx()?.0.insert(
            (owner.to_owned(), position.stream.clone()),
            CursorRow {
                version: PROGRESS_FORMAT_VERSION,
                sequence: position.sequence.to_string(),
                digest: position.digest.to_vec(),
                operations: position.operations,
                completed: 0,
            },
        );
        anyhow::ensure!(previous.is_none(), "primary key violation");
        Ok(())
    }
    async fn rollover_cursor(&mut self, owner: &str, position: &Position) -> Result<()> {
        let row = self
            .tx()?
            .0
            .get_mut(&(owner.to_owned(), position.stream.clone()))
            .ok_or_else(|| anyhow::anyhow!("missing row"))?;
        row.sequence = position.sequence.to_string();
        row.digest = position.digest.to_vec();
        row.operations = position.operations;
        row.completed = 0;
        Ok(())
    }
    async fn execute(&mut self, command: &RenderedCommand) -> Result<()> {
        let (io_fault, hang) = {
            let mut db = self.fake.db();
            (db.take(Fault::ExecuteIo), db.take(Fault::ExecuteHangs))
        };
        if io_fault {
            return Err(io());
        }
        if hang {
            std::future::pending::<()>().await;
        }
        let effect = format!("{} {}", command.sql, json!(command.parameters));
        self.tx()?.1.push(effect);
        Ok(())
    }
    async fn advance_cursor(&mut self, owner: &str, stream: &str, completed: i64) -> Result<u64> {
        if self.fake.db().take(Fault::ProcedureCommits) {
            // The procedure's COMMIT published its effect without the cursor.
            let (_, effects) = self.transaction.take().expect("transaction");
            self.fake.db().effects = effects;
            return Err(DeliveryError::TransactionStateChanged { depth: 0 }.into());
        }
        let row = self.tx()?.0.get_mut(&(owner.to_owned(), stream.to_owned()));
        match row {
            Some(row) if row.completed == completed => {
                row.completed += 1;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
    async fn commit(&mut self) -> Result<()> {
        let (lost_after, lost_before) = {
            let mut db = self.fake.db();
            (
                db.take(Fault::CommitLostAfterApply),
                db.take(Fault::CommitLostBeforeApply),
            )
        };
        let (rows, effects) = self.transaction.take().expect("transaction");
        if lost_before {
            return Err(io());
        }
        let mut db = self.fake.db();
        db.rows = rows;
        db.effects = effects;
        drop(db);
        if lost_after {
            return Err(io());
        }
        Ok(())
    }
    async fn rollback(&mut self) -> Result<()> {
        self.transaction = None;
        Ok(())
    }
    async fn erase(&mut self, owner: &str) -> Result<u64> {
        let mut db = self.fake.db();
        let before = db.rows.len();
        db.rows.retain(|(o, _), _| o != owner);
        Ok((before - db.rows.len()) as u64)
    }
    async fn close(self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Connector for Fake {
    type Session = FakeSession;
    async fn connect(&self) -> Result<FakeSession> {
        let refused = {
            let mut db = self.db();
            db.connects += 1;
            db.take(Fault::ConnectRefused)
        };
        if refused {
            return Err(io());
        }
        Ok(FakeSession {
            fake: self.clone(),
            transaction: None,
        })
    }
}

fn delivery(fake: &Fake, max_streams: u32, budget: Duration) -> Delivery<Fake> {
    Delivery::new(
        fake.clone(),
        owner(super::GRAPH, super::SINK_ID),
        max_streams,
        budget,
    )
}

fn position(stream: &str, sequence: u64, operation: i64, operations: i64, digest: u8) -> Position {
    Position {
        stream: stream.into(),
        sequence,
        operation,
        operations,
        digest: [digest; 32],
    }
}

const STREAM_A: &str = "a";
const STREAM_B: &str = "b";

async fn send(delivery: &mut Delivery<Fake>, position: &Position, label: &str) -> Result<Outcome> {
    let label = label.to_owned();
    let mut render = move || {
        Ok(Some(RenderedCommand {
            sql: "EXEC dbo.p @P1".into(),
            parameters: vec![json!(label.clone())],
        }))
    };
    delivery.deliver(position, &mut render).await
}

fn expect<T: std::fmt::Debug>(result: Result<T>) -> DeliveryError {
    let error = result.expect_err("protocol failure");
    assert_eq!(classify(&error), FailureClass::Permanent, "{error:#}");
    match error.downcast::<DeliveryError>() {
        Ok(error) => error,
        Err(error) => panic!("unexpected error {error:#}"),
    }
}

#[test]
fn owner_is_the_domain_separated_sha256_of_graph_and_component() {
    assert_eq!(
        owner(super::GRAPH, super::SINK_ID),
        "0a0c9c47e1febe798f442826307b8d94bce97b3aec7f8b4c4a91d64e855dc5fe"
    );
    assert_eq!(owner(super::GRAPH, super::SINK_ID).len(), 64);
    assert_ne!(
        owner("graph-b", super::SINK_ID),
        owner(super::GRAPH, super::SINK_ID)
    );
    assert_ne!(
        owner(super::GRAPH, "00000000-0000-0000-0000-000000000001"),
        owner(super::GRAPH, super::SINK_ID)
    );
}

#[test]
fn pure_decisions_cover_every_cursor_state() {
    let row = |sequence: u64, operations: i64, completed: i64, digest: u8| CursorRow {
        version: PROGRESS_FORMAT_VERSION,
        sequence: sequence.to_string(),
        digest: vec![digest; 32],
        operations,
        completed,
    };
    assert_eq!(
        decide(None, &position("s", 7, 0, 2, 1)).expect("first"),
        Decision::Apply {
            completed: 0,
            write: CursorWrite::Insert
        }
    );
    assert!(matches!(
        decide(None, &position("s", 7, 1, 2, 1)),
        Err(DeliveryError::MissingFirstOperation)
    ));
    let current = row(7, 3, 1, 1);
    assert_eq!(
        decide(Some(&current), &position("s", 7, 0, 3, 1)).expect("duplicate"),
        Decision::Duplicate
    );
    assert_eq!(
        decide(Some(&current), &position("s", 7, 1, 3, 1)).expect("next"),
        Decision::Apply {
            completed: 1,
            write: CursorWrite::Keep
        }
    );
    assert!(matches!(
        decide(Some(&current), &position("s", 7, 2, 3, 1)),
        Err(DeliveryError::OperationGap {
            expected: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        decide(Some(&current), &position("s", 7, 1, 3, 9)),
        Err(DeliveryError::PayloadConflict)
    ));
    assert!(matches!(
        decide(Some(&current), &position("s", 7, 1, 4, 1)),
        Err(DeliveryError::PayloadConflict)
    ));
    assert!(matches!(
        decide(Some(&current), &position("s", 6, 0, 1, 1)),
        Err(DeliveryError::ReceiptExpired {
            sequence: 6,
            cursor: 7
        })
    ));
    assert!(matches!(
        decide(Some(&current), &position("s", 8, 0, 1, 2)),
        Err(DeliveryError::UnfinishedBatch)
    ));
    let finished = row(7, 3, 3, 1);
    assert_eq!(
        decide(Some(&finished), &position("s", 9, 0, 1, 2)).expect("rollover"),
        Decision::Apply {
            completed: 0,
            write: CursorWrite::Rollover
        }
    );
    assert!(matches!(
        decide(Some(&finished), &position("s", 9, 1, 2, 2)),
        Err(DeliveryError::UnfinishedBatch)
    ));
    for invalid in [
        CursorRow {
            version: 1,
            ..finished.clone()
        },
        CursorRow {
            sequence: "007".into(),
            ..finished.clone()
        },
        CursorRow {
            sequence: "0".into(),
            ..finished.clone()
        },
        CursorRow {
            digest: vec![1; 31],
            ..finished.clone()
        },
        CursorRow {
            completed: 4,
            ..finished.clone()
        },
        CursorRow {
            operations: 0,
            completed: 0,
            ..finished.clone()
        },
    ] {
        assert!(matches!(
            decide(Some(&invalid), &position("s", 9, 0, 1, 2)),
            Err(DeliveryError::InvalidProgress(_))
        ));
    }
}

#[tokio::test]
async fn operations_commit_once_with_the_cursor_and_duplicates_are_skipped() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::ZERO);
        delivery.prepare().await?;
        assert_eq!(fake.db().prepares, 1);
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 1, 0, 2, 1), "a1.0").await?,
            Outcome::Applied
        );
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 1, 0, 2, 1), "a1.0").await?,
            Outcome::Duplicate
        );
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 1, 1, 2, 1), "a1.1").await?,
            Outcome::Applied
        );
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 1, 1, 2, 1), "a1.1").await?,
            Outcome::Duplicate
        );
        // Rollover to the next batch.
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 3, 0, 1, 2), "a3.0").await?,
            Outcome::Applied
        );
        assert_eq!(fake.effects().len(), 3);
        let row = fake.row(STREAM_A).expect("cursor row");
        assert_eq!(
            (row.sequence.as_str(), row.operations, row.completed),
            ("3", 1, 1)
        );
        assert_eq!(fake.db().connects, 1, "one persistent connection");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn protocol_violations_are_permanent_and_leave_no_effect() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::from_secs(10));
        send(&mut delivery, &position(STREAM_A, 5, 0, 3, 1), "ok").await?;
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_A, 5, 2, 3, 1), "gap").await),
            DeliveryError::OperationGap {
                expected: 1,
                actual: 2
            }
        ));
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_A, 5, 1, 3, 7), "conflict").await),
            DeliveryError::PayloadConflict
        ));
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_A, 4, 0, 1, 1), "expired").await),
            DeliveryError::ReceiptExpired { .. }
        ));
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_A, 6, 0, 1, 1), "early").await),
            DeliveryError::UnfinishedBatch
        ));
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_B, 1, 1, 2, 1), "late").await),
            DeliveryError::MissingFirstOperation
        ));
        assert_eq!(fake.effects().len(), 1);
        assert!(fake.row(STREAM_B).is_none());
        assert_eq!(
            fake.db().connects,
            1,
            "permanent failures keep the connection"
        );
        Ok(())
    })
    .await
}

#[tokio::test]
async fn stream_capacity_is_per_owner_and_refuses_without_effects() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut other = Delivery::new(
            fake.clone(),
            owner(super::GRAPH, "another-sink"),
            1,
            Duration::ZERO,
        );
        send(&mut other, &position(STREAM_A, 1, 0, 1, 1), "other").await?;
        let mut delivery = delivery(&fake, 1, Duration::ZERO);
        send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "a").await?;
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_B, 1, 0, 1, 1), "b").await),
            DeliveryError::StreamCapacity { max: 1 }
        ));
        assert!(fake.row(STREAM_B).is_none());
        assert_eq!(fake.effects().len(), 2);
        delivery.set_options(2, Duration::ZERO);
        send(&mut delivery, &position(STREAM_B, 1, 0, 1, 1), "b").await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_unknown_commit_outcome_is_retried_after_reconnect_without_duplicates() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::from_secs(30));
        fake.fault(Fault::CommitLostAfterApply);
        // The retry finds the committed cursor and reports the duplicate as done.
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "once").await?,
            Outcome::Duplicate
        );
        assert_eq!(fake.effects().len(), 1, "effect committed exactly once");
        assert_eq!(fake.db().connects, 2, "reconnected after the lost reply");

        fake.fault(Fault::CommitLostBeforeApply);
        assert_eq!(
            send(&mut delivery, &position(STREAM_A, 2, 0, 1, 2), "twice").await?,
            Outcome::Applied
        );
        assert_eq!(fake.effects().len(), 2, "rolled-back attempt left nothing");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn connection_failures_retry_with_backoff_within_the_budget() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::from_secs(30));
        fake.fault(Fault::ConnectRefused);
        fake.fault(Fault::ConnectRefused);
        send(&mut delivery, &position(STREAM_A, 1, 0, 2, 1), "a").await?;
        assert_eq!(fake.db().connects, 3);
        fake.fault(Fault::ExecuteIo);
        send(&mut delivery, &position(STREAM_A, 1, 1, 2, 1), "b").await?;
        assert_eq!(fake.db().connects, 4, "a broken connection is replaced");
        assert_eq!(fake.effects().len(), 2);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn an_exhausted_budget_returns_a_retryable_error_and_nothing_latches() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::ZERO);
        fake.fault(Fault::ExecuteIo);
        let error = send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "a")
            .await
            .expect_err("no retry budget");
        assert_eq!(
            classify(&error),
            FailureClass::Retryable { reconnect: true }
        );
        assert!(!delivery.connected());
        // The host retries the same operation later, on the same object.
        send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "a").await?;
        assert_eq!(fake.effects().len(), 1);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn a_dropped_call_drops_its_connection_and_the_next_call_reconnects() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::ZERO);
        send(&mut delivery, &position(STREAM_A, 1, 0, 2, 1), "a").await?;
        fake.fault(Fault::ExecuteHangs);
        let dropped = tokio::time::timeout(
            Duration::from_millis(20),
            send(&mut delivery, &position(STREAM_A, 1, 1, 2, 1), "hang"),
        )
        .await;
        assert!(dropped.is_err(), "the call was dropped mid-statement");
        assert!(
            !delivery.connected(),
            "the in-flight connection went with it"
        );
        send(&mut delivery, &position(STREAM_A, 1, 1, 2, 1), "b").await?;
        assert_eq!(fake.db().connects, 2);
        assert_eq!(fake.effects().len(), 2);
        Ok(())
    })
    .await
}

#[tokio::test]
async fn render_failures_are_permanent_and_roll_back_the_cursor_insert() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::from_secs(10));
        let mut render = || Err(anyhow::anyhow!("missing field"));
        let error = delivery
            .deliver(&position(STREAM_A, 1, 0, 1, 1), &mut render)
            .await
            .expect_err("render failure");
        assert_eq!(classify(&error), FailureClass::Permanent);
        assert!(matches!(
            error.downcast_ref::<DeliveryError>(),
            Some(DeliveryError::Render(_))
        ));
        assert!(
            fake.row(STREAM_A).is_none(),
            "the cursor insert rolled back"
        );
        // An operation with no command still advances the cursor.
        let mut nothing = || Ok(None);
        delivery
            .deliver(&position(STREAM_A, 1, 0, 1, 1), &mut nothing)
            .await?;
        assert_eq!(fake.row(STREAM_A).expect("row").completed, 1);
        assert!(fake.effects().is_empty());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn a_procedure_that_commits_is_reported_permanently() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut delivery = delivery(&fake, 16, Duration::from_secs(10));
        fake.fault(Fault::ProcedureCommits);
        assert!(matches!(
            expect(send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "a").await),
            DeliveryError::TransactionStateChanged { depth: 0 }
        ));
        assert_eq!(fake.effects().len(), 1, "the effect escaped the cursor");
        assert!(fake.row(STREAM_A).is_none());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn erase_deletes_only_this_owner_and_is_idempotent() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        let mut other = Delivery::new(
            fake.clone(),
            owner(super::GRAPH, "another-sink"),
            4,
            Duration::ZERO,
        );
        send(&mut other, &position(STREAM_A, 1, 0, 1, 1), "other").await?;
        let mut delivery = delivery(&fake, 4, Duration::ZERO);
        send(&mut delivery, &position(STREAM_A, 1, 0, 1, 1), "a").await?;
        send(&mut delivery, &position(STREAM_B, 1, 0, 1, 1), "b").await?;
        assert_eq!(delivery.erase().await?, 2);
        assert_eq!(delivery.erase().await?, 0);
        assert_eq!(fake.db().rows.len(), 1, "the other owner's row remains");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn prepare_retries_connection_failures_and_fails_fast_without_budget() -> Result<()> {
    super::within(async {
        let fake = Fake::default();
        fake.fault(Fault::ConnectRefused);
        let mut delivery = delivery(&fake, 4, Duration::from_secs(10));
        delivery.prepare().await?;
        // One guard at a time: the fake's std Mutex is not re-entrant.
        let (connects, prepares) = {
            let db = fake.db();
            (db.connects, db.prepares)
        };
        assert_eq!((connects, prepares), (2, 1));
        let fake = Fake::default();
        fake.fault(Fault::ConnectRefused);
        let mut delivery = self::delivery(&fake, 4, Duration::ZERO);
        let error = delivery.prepare().await.expect_err("no budget");
        assert!(classify(&error).is_retryable());
        Ok(())
    })
    .await
}

#[test]
fn errors_are_classified_by_number_and_type_never_by_text() {
    for code in [1205, 1222, 3960, 3961, 41301, 41302, 41305, 41325, 59_101] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Retryable { reconnect: false },
            "{code}"
        );
    }
    for code in [
        233, 64, 4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918, 49919, 49920,
    ] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Retryable { reconnect: true },
            "{code}"
        );
    }
    // Procedure THROW/RAISERROR, constraint and key violations, permission,
    // login, missing object and syntax errors are permanent.
    for code in [
        50000, 51000, 547, 2627, 2601, 229, 262, 18456, 2812, 102, 8144,
    ] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Permanent,
            "{code}"
        );
    }
    assert_eq!(classify(&io()), FailureClass::Retryable { reconnect: true });
    assert_eq!(
        classify(&anyhow::Error::from(tiberius::error::Error::Protocol(
            "desync".into()
        ))),
        FailureClass::Retryable { reconnect: true }
    );
    assert_eq!(
        classify(&anyhow::Error::from(tiberius::error::Error::Tls(
            "bad certificate".into()
        ))),
        FailureClass::Permanent
    );
    assert_eq!(
        classify(&anyhow::Error::new(crate::delivery::ConnectTimeout(
            Duration::from_secs(1)
        ))),
        FailureClass::Retryable { reconnect: true }
    );
    assert_eq!(
        classify(&io().context("wrapped with context")),
        FailureClass::Retryable { reconnect: true }
    );
    assert_eq!(
        classify(&anyhow::anyhow!(
            "deadlock victim 1205 timeout connection reset"
        )),
        FailureClass::Permanent,
        "text is never classified"
    );
    assert_eq!(
        classify(&DeliveryError::CommitUnknown(io()).into()),
        FailureClass::Retryable { reconnect: true }
    );
    assert_eq!(
        classify(&anyhow::Error::from(
            drasi_mssql_common::sql::ParameterLimitExceeded { count: 2099 }
        )),
        FailureClass::Permanent
    );
}
