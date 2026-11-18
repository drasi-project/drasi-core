// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Live PostgreSQL qualification of the shared stored-procedure protocol
//! over [`PgSession`]: the cross-backend protocol table, commit outcome
//! classification and a backend lost during COMMIT. Requires a
//! Docker-compatible container runtime.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    delivery_failure_class, DeliveryFailureClass, PermanentFailurePolicy,
};
use drasi_computation_storedproc_common::{
    conformance::{run_protocol_table, Backend, Probe, ReplyDropper, STREAM_A},
    driver::{Connector, Delivery, Outcome},
    owner, Position, ProgressTable,
};
use drasi_postgres_common::delivery::progress::{qualified, PgSession};
use serde_json::{json, Value};
use std::time::Duration;
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};
use tokio_postgres::{Client, NoTls};

const TABLE: &str = "conformance_progress";

struct Database {
    _container: ContainerAsync<GenericImage>,
    port: u16,
    admin: Client,
}

async fn client(port: u16) -> Result<Client> {
    let (client, connection) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres"),
        NoTls,
    )
    .await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

impl Database {
    async fn start() -> Result<Self> {
        let container = GenericImage::new("postgres", "16-alpine")
            .with_exposed_port(testcontainers::core::ContainerPort::Tcp(5432))
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .start()
            .await?;
        let port = container.get_host_port_ipv4(5432).await?;
        let mut last = None;
        for _ in 0..120 {
            match client(port).await {
                Ok(admin) if admin.simple_query("SELECT 1").await.is_ok() => {
                    return Ok(Self {
                        _container: container,
                        port,
                        admin,
                    })
                }
                Ok(_) => {}
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("PostgreSQL did not start")))
    }
}

/// Connects [`PgSession`]s directly or through a [`ReplyDropper`].
struct PgConnector {
    port: u16,
    owner: String,
    table: ProgressTable,
}

#[async_trait]
impl Connector for PgConnector {
    type Session = PgSession;
    async fn connect(&mut self) -> Result<PgSession> {
        let (client, connection) = tokio_postgres::connect(
            &format!(
                "host=127.0.0.1 port={} user=postgres password=postgres dbname=postgres \
                 options='-c statement_timeout=20000'",
                self.port
            ),
            NoTls,
        )
        .await?;
        PgSession::new(
            client,
            tokio::spawn(connection),
            self.owner.clone(),
            self.table.clone(),
            Duration::from_secs(25),
        )
    }
}

fn sink_owner() -> String {
    owner("conformance", "postgres")
}

struct PgBackend<'a> {
    database: &'a Database,
}

const PROCEDURES: &str = "
DROP TABLE IF EXISTS effects;
CREATE TABLE effects (tag TEXT PRIMARY KEY);
CREATE OR REPLACE PROCEDURE probe_effect(t TEXT) LANGUAGE plpgsql AS $$
BEGIN INSERT INTO effects VALUES (t); END $$;
CREATE OR REPLACE PROCEDURE probe_reject(t TEXT) LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'procedure rejected %', t USING ERRCODE = 'P0001'; END $$;
CREATE OR REPLACE PROCEDURE probe_transient(t TEXT) LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'could not serialize %', t USING ERRCODE = '40001'; END $$;
CREATE OR REPLACE PROCEDURE probe_commit(t TEXT) LANGUAGE plpgsql AS $$
BEGIN INSERT INTO effects VALUES (t); COMMIT; END $$;
CREATE OR REPLACE PROCEDURE probe_rollback(t TEXT) LANGUAGE plpgsql AS $$
BEGIN INSERT INTO effects VALUES (t); ROLLBACK; END $$;
";

async fn reset(database: &Database) -> Result<ProgressTable> {
    let table = ProgressTable::parse(TABLE)?;
    database
        .admin
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {}; DROP TABLE IF EXISTS {}; {PROCEDURES}",
            qualified(&table),
            qualified(&table.dead_letter())
        ))
        .await?;
    Ok(table)
}

fn command(probe: Probe, tag: &str) -> (String, Vec<Value>) {
    let procedure = match probe {
        Probe::Effect => "probe_effect",
        Probe::Reject => "probe_reject",
        Probe::Transient => "probe_transient",
        Probe::Commit => "probe_commit",
        Probe::Rollback => "probe_rollback",
    };
    (format!("CALL {procedure}($1)"), vec![json!(tag)])
}

async fn count(database: &Database, sql: &str) -> Result<i64> {
    Ok(database.admin.query_one(sql, &[]).await?.get(0))
}

#[async_trait]
impl Backend for PgBackend<'_> {
    type Connector = PgConnector;
    async fn reset(&mut self) -> Result<PgConnector> {
        Ok(PgConnector {
            port: self.database.port,
            owner: sink_owner(),
            table: reset(self.database).await?,
        })
    }
    fn command(&self, probe: Probe, tag: &str) -> (String, Vec<Value>) {
        command(probe, tag)
    }
    async fn effects(&mut self) -> Result<Vec<String>> {
        let rows = self
            .database
            .admin
            .query("SELECT tag FROM effects ORDER BY tag", &[])
            .await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }
    async fn dead_letters(&mut self) -> Result<Vec<String>> {
        let table = ProgressTable::parse(TABLE)?.dead_letter();
        let rows = self
            .database
            .admin
            .query(
                &format!(
                    "SELECT delivery_id FROM {} WHERE owner = $1 ORDER BY delivery_id",
                    qualified(&table)
                ),
                &[&sink_owner()],
            )
            .await?;
        Ok(rows.iter().map(|row| row.get(0)).collect())
    }
    async fn cursor_rows(&mut self) -> Result<u64> {
        let table = ProgressTable::parse(TABLE)?;
        let rows: i64 = self
            .database
            .admin
            .query_one(
                &format!(
                    "SELECT count(*) FROM {} WHERE owner = $1",
                    qualified(&table)
                ),
                &[&sink_owner()],
            )
            .await?
            .get(0);
        Ok(u64::try_from(rows)?)
    }
    async fn completed(&mut self, stream: &str) -> Result<Option<i64>> {
        let table = ProgressTable::parse(TABLE)?;
        Ok(self
            .database
            .admin
            .query_opt(
                &format!(
                    "SELECT completed FROM {} WHERE owner = $1 AND stream_key = $2",
                    qualified(&table)
                ),
                &[&sink_owner(), &stream],
            )
            .await?
            .map(|row| row.get(0)))
    }
}

fn position(sequence: u64, operation: i64, operations: i64) -> Position {
    Position {
        stream: STREAM_A.into(),
        sequence,
        operation,
        operations,
        digest: [7; 32],
        epoch: String::new(),
        generation: 0,
        delivery_id: format!("drasi-delivery-v1-live-{sequence}-{operation}"),
    }
}

async fn deliver(
    delivery: &mut Delivery<PgConnector>,
    position: &Position,
    command: (String, Vec<Value>),
) -> Result<Outcome> {
    let mut command = Some(command);
    delivery.deliver(position, &mut || Ok(command.take())).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_passes_the_shared_protocol_table() -> Result<()> {
    let database = Database::start().await?;
    run_protocol_table(&mut PgBackend {
        database: &database,
    })
    .await
}

/// A deferred constraint fails at COMMIT: a definite rollback, so the
/// failure is permanent (and settled as skipped under `skip`), never an
/// "outcome unknown" retry.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn a_commit_time_constraint_failure_is_permanent_and_skippable() -> Result<()> {
    let database = Database::start().await?;
    for policy in [PermanentFailurePolicy::Stall, PermanentFailurePolicy::Skip] {
        let table = reset(&database).await?;
        database
            .admin
            .batch_execute(
                "DROP TABLE IF EXISTS deferred_child; DROP TABLE IF EXISTS deferred_parent;
                 CREATE TABLE deferred_parent (id INT PRIMARY KEY);
                 CREATE TABLE deferred_child (id INT PRIMARY KEY, parent INT REFERENCES
                     deferred_parent (id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE OR REPLACE PROCEDURE orphan(t TEXT) LANGUAGE plpgsql AS $$
                 BEGIN INSERT INTO effects VALUES (t); INSERT INTO deferred_child VALUES (1, 99); END $$;",
            )
            .await?;
        let mut delivery = Delivery::new(
            PgConnector {
                port: database.port,
                owner: sink_owner(),
                table,
            },
            8,
            policy,
        );
        delivery.prepare().await?;
        let error = deliver(
            &mut delivery,
            &position(1, 0, 1),
            ("CALL orphan($1)".into(), vec![json!("orphan")]),
        )
        .await
        .unwrap_err();
        let expected = match policy {
            PermanentFailurePolicy::Stall => DeliveryFailureClass::Permanent,
            PermanentFailurePolicy::Skip => DeliveryFailureClass::Skipped,
        };
        assert_eq!(delivery_failure_class(&error), Some(expected), "{error:#}");
        let text = format!("{error:#}");
        assert!(text.contains("rejected the delivery commit"), "{text}");
        assert!(!text.contains("outcome is unknown"), "{text}");
        assert!(delivery.connected(), "a definite answer keeps the session");
        assert_eq!(count(&database, "SELECT count(*) FROM effects").await?, 0);
        let dead_letters = count(
            &database,
            &format!(
                "SELECT count(*) FROM {}",
                qualified(&ProgressTable::parse(TABLE)?.dead_letter())
            ),
        )
        .await?;
        assert_eq!(
            dead_letters,
            i64::from(policy == PermanentFailurePolicy::Skip)
        );
        delivery.close(Duration::from_secs(5)).await?;
    }
    Ok(())
}

/// COMMIT reaches the server and commits, but its reply is lost: the
/// failure is transient with an unknown outcome, the session is discarded,
/// and the next attempt finds the operation committed (no duplicate).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn a_lost_commit_reply_is_resolved_without_duplicate_or_loss() -> Result<()> {
    let database = Database::start().await?;
    let table = reset(&database).await?;
    let proxy =
        ReplyDropper::start(format!("127.0.0.1:{}", database.port).parse()?, b"COMMIT\0").await?;
    let mut delivery = Delivery::new(
        PgConnector {
            port: proxy.port(),
            owner: sink_owner(),
            table,
        },
        8,
        PermanentFailurePolicy::Stall,
    );
    delivery.prepare().await?;
    proxy.arm();
    let error = deliver(
        &mut delivery,
        &position(1, 0, 1),
        command(Probe::Effect, "once"),
    )
    .await
    .unwrap_err();
    assert_eq!(proxy.cuts(), 1);
    assert!(
        matches!(
            delivery_failure_class(&error),
            Some(DeliveryFailureClass::Transient { .. })
        ),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("outcome is unknown"),
        "{error:#}"
    );
    assert!(!delivery.connected());
    assert_eq!(
        deliver(
            &mut delivery,
            &position(1, 0, 1),
            command(Probe::Effect, "once")
        )
        .await?,
        Outcome::Duplicate
    );
    assert_eq!(count(&database, "SELECT count(*) FROM effects").await?, 1);
    delivery.close(Duration::from_secs(5)).await?;
    Ok(())
}

/// The backend is terminated while COMMIT runs (a deferred trigger holds
/// it): the transaction rolls back, the failure is transient, and the retry
/// applies the operation exactly once.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn a_backend_killed_during_commit_is_resolved_without_duplicate_or_loss() -> Result<()> {
    let database = Database::start().await?;
    let table = reset(&database).await?;
    database
        .admin
        .batch_execute(
            "CREATE OR REPLACE FUNCTION slow_commit() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN IF NEW.tag = 'slow' THEN PERFORM pg_sleep(30); END IF; RETURN NULL; END $$;
             CREATE CONSTRAINT TRIGGER slow_commit AFTER INSERT ON effects
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION slow_commit();",
        )
        .await?;
    let mut delivery = Delivery::new(
        PgConnector {
            port: database.port,
            owner: sink_owner(),
            table,
        },
        8,
        PermanentFailurePolicy::Stall,
    );
    delivery.prepare().await?;
    let killer = async {
        for _ in 0..200 {
            let killed: i64 = database
                .admin
                .query_one(
                    "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity
                     WHERE query = 'COMMIT' AND wait_event = 'PgSleep'",
                    &[],
                )
                .await?
                .get(0);
            if killed > 0 {
                return anyhow::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("the COMMIT never started")
    };
    let slow = position(1, 0, 1);
    let (delivered, killed) = tokio::join!(
        deliver(&mut delivery, &slow, command(Probe::Effect, "slow")),
        killer
    );
    killed?;
    let error = delivered.unwrap_err();
    assert!(
        matches!(
            delivery_failure_class(&error),
            Some(DeliveryFailureClass::Transient { .. })
        ),
        "{error:#}"
    );
    assert_eq!(count(&database, "SELECT count(*) FROM effects").await?, 0);
    database
        .admin
        .batch_execute("DROP TRIGGER slow_commit ON effects")
        .await?;
    assert_eq!(
        deliver(
            &mut delivery,
            &position(1, 0, 1),
            command(Probe::Effect, "slow")
        )
        .await?,
        Outcome::Applied
    );
    assert_eq!(
        deliver(
            &mut delivery,
            &position(1, 0, 1),
            command(Probe::Effect, "slow")
        )
        .await?,
        Outcome::Duplicate
    );
    assert_eq!(count(&database, "SELECT count(*) FROM effects").await?, 1);
    delivery.close(Duration::from_secs(5)).await?;
    Ok(())
}
