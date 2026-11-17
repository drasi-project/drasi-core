// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Live MySQL qualification of the shared stored-procedure protocol over
//! `MySqlSession`: the cross-backend protocol table, a lost COMMIT reply and
//! an InnoDB deadlock. Requires a Docker-compatible container runtime.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    delivery_failure_class, DeliveryFailureClass, PermanentFailurePolicy,
};
use drasi_computation_storedproc_common::{
    conformance::{run_protocol_table, Backend, Probe, ReplyDropper, STREAM_A},
    driver::{Delivery, Outcome},
    owner, Position, ProgressTable,
};
use drasi_mysql_common::{
    native::{catalog::field, connection::Connection},
    storedproc::{
        config::MySqlStoredProcConfig,
        delivery::{quoted, Effect, MySqlConnector, Timeouts},
        session::{self, Endpoint},
    },
};
use mysql_common::Value;
use serde_json::json;
use std::time::Duration;
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};

const TABLE: &str = "conformance_progress";
const PASSWORD: &str = "native-test";

struct Database {
    _container: ContainerAsync<GenericImage>,
    port: u16,
    admin: Connection,
}

fn endpoint(port: u16) -> Result<Endpoint> {
    let config = MySqlStoredProcConfig::parse(json!({
        "host": "127.0.0.1", "port": port, "database": "test", "user": "root",
        "password": PASSWORD, "sslMode": "disable",
        "defaultTemplate": {"added": {"template": "CALL x({{param after.id}})"}}
    }))?;
    Endpoint::new(&config, "root".into(), PASSWORD.into())
}

async fn connect(port: u16) -> Result<Connection> {
    Ok(
        session::connect(&endpoint(port)?, Duration::from_secs(5), None, None)
            .await?
            .connection,
    )
}

impl Database {
    async fn start() -> Result<Self> {
        let container = GenericImage::new("mysql", "8.4")
            .with_exposed_port(testcontainers::core::ContainerPort::Tcp(3306))
            .with_env_var("MYSQL_ROOT_PASSWORD", PASSWORD)
            .with_env_var("MYSQL_DATABASE", "test")
            .start()
            .await?;
        let port = container.get_host_port_ipv4(3306).await?;
        let mut last = None;
        for _ in 0..240 {
            match connect(port).await {
                Ok(admin) => {
                    return Ok(Self {
                        _container: container,
                        port,
                        admin,
                    })
                }
                Err(error) => last = Some(error),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("MySQL did not start")))
    }

    async fn rows(&mut self, sql: &str) -> Result<Vec<String>> {
        self.admin
            .query(sql)
            .await?
            .values
            .iter()
            .map(|row| field(row, 0).map(str::to_owned))
            .collect()
    }

    async fn count(&mut self, sql: &str) -> Result<i64> {
        Ok(self.rows(sql).await?[0].parse()?)
    }
}

fn sink_owner() -> String {
    owner("conformance", "mysql")
}

fn connector(port: u16) -> Result<MySqlConnector> {
    MySqlConnector::new(
        endpoint(port)?,
        sink_owner(),
        "test".into(),
        ProgressTable::parse(TABLE)?,
        Timeouts {
            connect: Duration::from_secs(5),
            command: Duration::from_secs(20),
            shutdown: Duration::from_secs(10),
        },
    )
}

const PROCEDURES: [&str; 7] = [
    "CREATE TABLE effects (tag VARCHAR(128) PRIMARY KEY) ENGINE=InnoDB",
    "CREATE PROCEDURE probe_effect(t VARCHAR(128)) BEGIN INSERT INTO effects VALUES (t); END",
    "CREATE PROCEDURE probe_reject(t VARCHAR(128)) BEGIN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'procedure rejected the change'; END",
    "CREATE PROCEDURE probe_transient(t VARCHAR(128)) BEGIN SIGNAL SQLSTATE '40001' SET MYSQL_ERRNO = 1213, MESSAGE_TEXT = 'deadlock'; END",
    "CREATE PROCEDURE probe_commit(t VARCHAR(128)) BEGIN INSERT INTO effects VALUES (t); COMMIT; END",
    "CREATE PROCEDURE probe_rollback(t VARCHAR(128)) BEGIN INSERT INTO effects VALUES (t); ROLLBACK; END",
    "CREATE PROCEDURE probe_deadlock(t VARCHAR(128)) BEGIN UPDATE locks SET v = v + 1 WHERE id = 2; DO SLEEP(1); UPDATE locks SET v = v + 1 WHERE id = 1; INSERT INTO effects VALUES (t); END",
];

async fn reset(database: &mut Database) -> Result<()> {
    let table = ProgressTable::parse(TABLE)?;
    for statement in [
        format!("DROP TABLE IF EXISTS {}", quoted(&table, "test")),
        format!(
            "DROP TABLE IF EXISTS {}",
            quoted(&table.dead_letter(), "test")
        ),
        "DROP TABLE IF EXISTS effects".into(),
        "DROP TABLE IF EXISTS locks".into(),
        "DROP TABLE IF EXISTS bulk".into(),
        "DROP PROCEDURE IF EXISTS probe_effect".into(),
        "DROP PROCEDURE IF EXISTS probe_reject".into(),
        "DROP PROCEDURE IF EXISTS probe_transient".into(),
        "DROP PROCEDURE IF EXISTS probe_commit".into(),
        "DROP PROCEDURE IF EXISTS probe_rollback".into(),
        "DROP PROCEDURE IF EXISTS probe_deadlock".into(),
    ]
    .into_iter()
    .chain(PROCEDURES.map(str::to_owned))
    {
        database.admin.execute(&statement).await?;
    }
    Ok(())
}

fn command(probe: Probe, tag: &str) -> Effect {
    let procedure = match probe {
        Probe::Effect => "probe_effect",
        Probe::Reject => "probe_reject",
        Probe::Transient => "probe_transient",
        Probe::Commit => "probe_commit",
        Probe::Rollback => "probe_rollback",
    };
    Effect {
        sql: format!("CALL {procedure}(?)"),
        parameters: vec![Value::from(tag)],
    }
}

struct MySqlBackend<'a> {
    database: &'a mut Database,
}

#[async_trait]
impl Backend for MySqlBackend<'_> {
    type Connector = MySqlConnector;
    async fn reset(&mut self) -> Result<MySqlConnector> {
        reset(self.database).await?;
        connector(self.database.port)
    }
    fn command(&self, probe: Probe, tag: &str) -> Effect {
        command(probe, tag)
    }
    async fn effects(&mut self) -> Result<Vec<String>> {
        self.database
            .rows("SELECT tag FROM effects ORDER BY tag")
            .await
    }
    async fn dead_letters(&mut self) -> Result<Vec<String>> {
        let table = quoted(&ProgressTable::parse(TABLE)?.dead_letter(), "test");
        self.database
            .rows(&format!(
                "SELECT delivery_id FROM {table} WHERE owner = '{}' ORDER BY delivery_id",
                sink_owner()
            ))
            .await
    }
    async fn cursor_rows(&mut self) -> Result<u64> {
        let table = quoted(&ProgressTable::parse(TABLE)?, "test");
        Ok(u64::try_from(
            self.database
                .count(&format!(
                    "SELECT COUNT(*) FROM {table} WHERE owner = '{}'",
                    sink_owner()
                ))
                .await?,
        )?)
    }
    async fn completed(&mut self, stream: &str) -> Result<Option<i64>> {
        let table = quoted(&ProgressTable::parse(TABLE)?, "test");
        let rows = self
            .database
            .rows(&format!(
                "SELECT completed FROM {table} WHERE owner = '{}' AND stream_key = '{stream}'",
                sink_owner()
            ))
            .await?;
        rows.first().map(|value| Ok(value.parse()?)).transpose()
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
    delivery: &mut Delivery<MySqlConnector>,
    position: &Position,
    command: Effect,
) -> Result<Outcome> {
    let mut command = Some(command);
    delivery.deliver(position, &mut || Ok(command.take())).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible MySQL container runtime"]
async fn mysql_passes_the_shared_protocol_table() -> Result<()> {
    let mut database = Database::start().await?;
    run_protocol_table(&mut MySqlBackend {
        database: &mut database,
    })
    .await
}

/// COMMIT reaches the server and commits, but its reply is lost: the
/// session is KILLed and proven gone, the failure is transient, and the next
/// attempt finds the operation committed (no duplicate, no loss).
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible MySQL container runtime"]
async fn a_lost_commit_reply_is_resolved_without_duplicate_or_loss() -> Result<()> {
    let mut database = Database::start().await?;
    reset(&mut database).await?;
    let proxy = ReplyDropper::start(
        format!("127.0.0.1:{}", database.port).parse()?,
        b"\x03COMMIT",
    )
    .await?;
    let mut delivery = Delivery::new(connector(proxy.port())?, 8, PermanentFailurePolicy::Stall);
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
    assert_eq!(
        deliver(
            &mut delivery,
            &position(1, 0, 1),
            command(Probe::Effect, "once")
        )
        .await?,
        Outcome::Duplicate
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM effects").await?, 1);
    delivery.close(Duration::from_secs(5)).await?;
    Ok(())
}

/// An InnoDB deadlock (1213) makes the procedure the victim: the failure is
/// transient (InnoDB rolled the transaction back), so the graph retries the
/// same input, which then applies exactly once.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible MySQL container runtime"]
async fn a_deadlock_is_transient_and_the_retry_applies_once() -> Result<()> {
    let mut database = Database::start().await?;
    reset(&mut database).await?;
    for statement in [
        "CREATE TABLE locks (id INT PRIMARY KEY, v INT NOT NULL) ENGINE=InnoDB",
        "INSERT INTO locks VALUES (1, 0), (2, 0)",
        "CREATE TABLE bulk (id INT PRIMARY KEY, v INT NOT NULL) ENGINE=InnoDB",
    ] {
        database.admin.execute(statement).await?;
    }
    let values = (0..200).map(|id| format!("({id}, 0)")).collect::<Vec<_>>();
    database
        .admin
        .execute(&format!("INSERT INTO bulk VALUES {}", values.join(",")))
        .await?;
    let mut delivery = Delivery::new(connector(database.port)?, 8, PermanentFailurePolicy::Stall);
    delivery.prepare().await?;
    // The competing transaction holds row 1 and has modified many rows, so
    // InnoDB chooses the procedure's smaller transaction as the victim.
    let mut competitor = connect(database.port).await?;
    competitor.execute("START TRANSACTION").await?;
    competitor.execute("UPDATE bulk SET v = v + 1").await?;
    competitor
        .execute("UPDATE locks SET v = v + 1 WHERE id = 1")
        .await?;
    let deadlock = Effect {
        sql: "CALL probe_deadlock(?)".into(),
        parameters: vec![Value::from("deadlocked")],
    };
    let contend = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let result = competitor
            .execute("UPDATE locks SET v = v + 1 WHERE id = 2")
            .await;
        competitor.execute("COMMIT").await?;
        result
    };
    let position = position(1, 0, 1);
    let (delivered, contended) =
        tokio::join!(deliver(&mut delivery, &position, deadlock.clone()), contend);
    contended?;
    let error = delivered.unwrap_err();
    assert_eq!(
        delivery_failure_class(&error),
        Some(DeliveryFailureClass::Transient {
            retry_after_ms: None
        }),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("1213"), "{error:#}");
    assert!(delivery.connected(), "a deadlock keeps the session");
    assert_eq!(database.count("SELECT COUNT(*) FROM effects").await?, 0);
    // The graph's retry of the same input.
    assert_eq!(
        deliver(&mut delivery, &position, deadlock).await?,
        Outcome::Applied
    );
    assert_eq!(database.count("SELECT COUNT(*) FROM effects").await?, 1);
    delivery.close(Duration::from_secs(5)).await?;
    Ok(())
}
