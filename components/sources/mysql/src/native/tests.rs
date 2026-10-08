// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::models::SourceChange;
use drasi_lib::computation::v1::{
    ComponentId, ComputationComponent, EnvelopeSource, GraphChangeCodec, SourceTransactionCodec,
    StreamId,
};
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::mysql::Mysql;
mod snapshot;

pub(super) fn config(port: u16, output: MySqlOutput) -> MySqlConfig {
    MySqlConfig {
        connection: MySqlConnectionConfig {
            host: "127.0.0.1".into(),
            port,
            database: "test".into(),
            user: "root".into(),
            password: "root".into(),
            ssl_mode: MySqlTlsMode::Disabled,
            tls_ca_pem: None,
            server_id: NonZeroU32::new(874).unwrap(),
            heartbeat_interval_ms: NonZeroU64::new(100).unwrap(),
        },
        tables: vec!["items".into()],
        start: MySqlStartPosition::End,
        output,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: NonZeroUsize::new(20).unwrap(),
            max_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
            max_duration_ms: NonZeroU64::new(5000).unwrap(),
        },
        max_protocol_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
        io_timeout_ms: NonZeroU64::new(5000).unwrap(),
    }
}

#[test]
fn native_settings_reject_unsafe_replay_and_redact_passwords() -> Result<()> {
    let mut config = config(3306, MySqlOutput::Changes);
    config.validate()?;
    assert!(!format!("{:?}", config.connection).contains("password: \"root\""));
    config.replay = Some(MySqlRetention::Server);
    assert!(config.validate().is_err());
    config.output = MySqlOutput::Transactions;
    assert!(config.validate().is_err());
    config.start = MySqlStartPosition::Position {
        file: "mysql-bin.000001".into(),
        position: 4,
    };
    config.validate()?;
    let mut names = config.clone();
    names.tables = vec!["t".repeat(64)];
    names.validate()?;
    names.tables = vec!["t".repeat(65)];
    assert!(names.validate().is_err());
    names.tables = vec!["items".into()];
    names.connection.database = "d".repeat(65);
    assert!(names.validate().is_err());
    config.connection.tls_ca_pem = Some("x".repeat(65537));
    assert!(config.validate().is_err());
    for file in [
        "../mysql-bin.000001",
        "mysql-bin.000000",
        "mysql-bin.1",
        "mysql-bin.4294967296",
    ] {
        assert!(file_number(file).is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_owned_source_emits_complete_transactions_and_fast_changes() -> Result<()> {
    let container = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_cmd(vec![
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
        ])
        .start()
        .await?;
    let port = container.get_host_port_ipv4(3306).await?;
    let settings = config(port, MySqlOutput::Transactions);
    let mut admin = connection::Connection::connect(
        &settings.connection,
        settings.max_protocol_bytes.get(),
        settings.timeout(),
    )
    .await?;
    admin
        .execute("CREATE TABLE items (id VARBINARY(8) PRIMARY KEY, value VARCHAR(80) NOT NULL)")
        .await?;
    let mut source = MySqlSource::new(
        ComponentId::try_new("mysql")?,
        StreamId::try_new("mysql-changes")?,
        settings.clone(),
        None,
    )?;
    source.start().await?;
    admin.execute("BEGIN").await?;
    admin
        .execute("INSERT INTO items VALUES (X'00FF', 'first'), (X'0100', 'second')")
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), source.next())
            .await
            .is_err()
    );
    admin.execute("COMMIT").await?;
    let envelope = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("transaction")?;
    let changes =
        SourceTransactionCodec::decode(&envelope.envelope, settings.transactions)?.into_changes();
    assert_eq!(changes.len(), 2);
    assert!(changes
        .iter()
        .all(|change| matches!(change, SourceChange::Insert { .. })));
    let old = changes[0].get_reference().clone();
    admin
        .execute("UPDATE items SET id=X'AA00' WHERE id=X'00FF'")
        .await?;
    let envelope = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("key update")?;
    let changes =
        SourceTransactionCodec::decode(&envelope.envelope, settings.transactions)?.into_changes();
    assert_eq!(changes.len(), 2);
    assert!(matches!(&changes[0], SourceChange::Delete { metadata } if metadata.reference == old));
    assert!(matches!(&changes[1], SourceChange::Insert { .. }));
    assert_ne!(changes[1].get_reference(), &old);
    source.stop().await?;
    source.start().await?;
    admin.execute("DELETE FROM items").await?;
    let envelope = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("delete")?;
    assert_eq!(
        SourceTransactionCodec::decode(&envelope.envelope, settings.transactions)?
            .into_changes()
            .len(),
        2
    );
    source.stop().await?;
    let mut fast = MySqlSource::new(
        ComponentId::try_new("mysql")?,
        StreamId::try_new("mysql-fast")?,
        config(port, MySqlOutput::Changes),
        None,
    )?;
    assert_eq!(fast.recovery_contract(), Default::default());
    assert!(fast.recovery_progress().is_none());
    fast.start().await?;
    admin
        .execute("INSERT INTO items VALUES (X'FF00', 'fast')")
        .await?;
    let envelope = tokio::time::timeout(Duration::from_secs(5), fast.next())
        .await??
        .context("fast change")?;
    assert_eq!(
        envelope.envelope.changes().schema(),
        GraphChangeCodec::schema().descriptor()
    );
    fast.stop().await?;
    admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked by the process-exit qualification parent"]
async fn native_mysql_process_exit_helper() -> Result<()> {
    let Ok(point) = std::env::var("DRASI_MYSQL_CRASH_POINT") else {
        return Ok(());
    };
    anyhow::ensure!(
        matches!(point.as_str(), "before" | "after"),
        "invalid MySQL crash point"
    );
    let config: MySqlConfig = serde_json::from_str(&std::env::var("DRASI_MYSQL_CRASH_CONFIG")?)?;
    let path = std::env::var("DRASI_MYSQL_CRASH_PATH")?;
    let owner = progress()?;
    let mut query = query(std::path::Path::new(&path), owner.clone()).await?;
    query.start().await?;
    let mut source = source(config, owner)?;
    source.start().await?;
    let transaction = next(&mut source).await?;
    if point == "after" {
        apply(&mut query, transaction).await?;
    }
    std::process::exit(83);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_process_exit_before_and_after_commit_recovers_complete_query_state() -> Result<()> {
    for point in ["before", "after"] {
        let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
        let path = tempfile::tempdir()?;
        database.admin.execute("BEGIN").await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (1, 'first')")
            .await?;
        database
            .admin
            .execute("UPDATE items SET value='final' WHERE id=1")
            .await?;
        database.admin.execute("COMMIT").await?;
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--ignored",
                    "--exact",
                    "native::tests::native_mysql_process_exit_helper",
                    "--nocapture",
                ])
                .env("DRASI_MYSQL_CRASH_POINT", point)
                .env(
                    "DRASI_MYSQL_CRASH_CONFIG",
                    serde_json::to_string(&database.settings)?,
                )
                .env("DRASI_MYSQL_CRASH_PATH", path.path())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        anyhow::ensure!(
            output.status.code() == Some(83),
            "MySQL crash helper did not reach {point}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let owner = progress()?;
        let mut query = query(path.path(), owner.clone()).await?;
        query.start().await?;
        assert_eq!(
            query.results().snapshot()?.rows.len(),
            usize::from(point == "after")
        );
        database
            .admin
            .execute("INSERT INTO items VALUES (2, 'offline')")
            .await?;
        let mut source = source(database.settings, owner)?;
        source.start().await?;
        if point == "before" {
            apply(&mut query, next(&mut source).await?).await?;
        }
        apply(&mut query, next(&mut source).await?).await?;
        assert_eq!(query.results().snapshot()?.rows.len(), 2);
        source.stop().await?;
        query.stop().await?;
        database.admin.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_retention_and_schema_failures_do_not_modify_server_settings_or_checkpoint(
) -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let mut query = query(path.path(), owner.clone()).await?;
    query.start().await?;
    let mut source = source(database.settings, owner.clone())?;
    if !database
        .admin
        .query("SHOW GLOBAL VARIABLES LIKE 'expire_logs_days'")
        .await?
        .values
        .is_empty()
    {
        database
            .admin
            .execute("SET GLOBAL expire_logs_days=1")
            .await?;
        assert!(source
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("expiry disabled"));
        source.stop().await?;
        let expiry = database
            .admin
            .query("SELECT @@global.expire_logs_days")
            .await?;
        assert_eq!(catalog::field(&expiry.values[0], 0)?, "1");
        database
            .admin
            .execute("SET GLOBAL expire_logs_days=0")
            .await?;
    }
    database
        .admin
        .execute("SET GLOBAL binlog_expire_logs_seconds=3600")
        .await?;
    assert!(source
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("expiry disabled"));
    source.stop().await?;
    let expiry = database
        .admin
        .query("SELECT @@global.binlog_expire_logs_seconds")
        .await?;
    assert_eq!(catalog::field(&expiry.values[0], 0)?, "3600");
    assert!(owner.snapshot().checkpoints.is_empty());
    if !database
        .admin
        .query("SHOW GLOBAL VARIABLES LIKE 'binlog_expire_logs_auto_purge'")
        .await?
        .values
        .is_empty()
    {
        database
            .admin
            .execute("SET GLOBAL binlog_expire_logs_auto_purge=OFF")
            .await?;
        source.start().await?;
        source.stop().await?;
        database
            .admin
            .execute("SET GLOBAL binlog_expire_logs_auto_purge=ON")
            .await?;
    }
    database
        .admin
        .execute("SET GLOBAL binlog_expire_logs_seconds=0")
        .await?;
    source.start().await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1, 'original')")
        .await?;
    apply(&mut query, next(&mut source).await?).await?;
    let checkpoint = owner.snapshot().checkpoints.clone();
    database
        .admin
        .execute("ALTER TABLE items ADD COLUMN extra INT")
        .await?;
    assert!(next(&mut source)
        .await
        .unwrap_err()
        .to_string()
        .contains("schema recovery"));
    assert_eq!(owner.snapshot().checkpoints, checkpoint);
    source.stop().await?;
    assert!(source
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("binding"));
    source.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

struct Database {
    _container: testcontainers::ContainerAsync<Mysql>,
    admin: connection::Connection,
    settings: MySqlConfig,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_column_types_preserve_values_and_unsupported_types_fail_before_streaming(
) -> Result<()> {
    use drasi_core::models::ElementValue;
    use drasi_core::models::ElementValue::Integer;
    let mut database = Database::new(MySqlRetention::Server).await?;
    database.admin.execute("DROP TABLE items").await?;
    database
        .admin
        .execute(
            "CREATE TABLE items (
        id INT PRIMARY KEY, ti TINYINT, sm SMALLINT, mi MEDIUMINT, bi BIGINT,
        u BIGINT UNSIGNED, z INT UNSIGNED ZEROFILL, f FLOAT, d DOUBLE, de DECIMAL(12,2),
        yr YEAR, da DATE, tm TIME(6), dt DATETIME(6), ts TIMESTAMP(6),
        c CHAR(3), v VARCHAR(80), tx TEXT, b BINARY(3), vb VARBINARY(8), bl BLOB,
        bits BIT(9), en ENUM('not unsigned value','second'), st SET('red','green'),
        js JSON, absent INT NULL)",
        )
        .await?;
    database.settings.replay = None;
    database.settings.start = MySqlStartPosition::End;
    let mut source = MySqlSource::new(
        ComponentId::try_new("mysql")?,
        StreamId::try_new("types")?,
        database.settings.clone(),
        None,
    )?;
    source.start().await?;
    database
        .admin
        .execute(
            "INSERT INTO items VALUES (
        1, -128, -32768, -8388608, -9, 18446744073709551615, 7, 1.25, 2.5, 1234.50,
        2026, '2026-01-02', '12:34:56.123456', '2026-01-02 12:34:56.123456',
        '2026-01-02 12:34:56.123456', 'xy', CONVERT(0x636166C3A9 USING utf8mb4),
        'text', X'00FF', X'00FF', X'FF01', b'111111111', 'not unsigned value',
        'red,green', '{\"value\":[1,true,null]}', NULL)",
        )
        .await?;
    let envelope = next(&mut source).await?;
    let changes =
        SourceTransactionCodec::decode(&envelope.envelope, database.settings.transactions)?
            .into_changes();
    let [SourceChange::Insert { element }] = changes.as_slice() else {
        anyhow::bail!("expected one complete typed row");
    };
    for (name, value) in [
        ("ti", ElementValue::Integer(-128)),
        ("sm", ElementValue::Integer(-32768)),
        ("mi", ElementValue::Integer(-8388608)),
        ("bi", ElementValue::Integer(-9)),
        ("u", ElementValue::String("18446744073709551615".into())),
        ("z", ElementValue::Integer(7)),
        ("f", ElementValue::Float(1.25.into())),
        ("d", ElementValue::Float(2.5.into())),
        ("de", ElementValue::String("1234.50".into())),
        ("yr", ElementValue::Integer(2026)),
        ("da", ElementValue::String("2026-01-02 00:00:00".into())),
        ("tm", ElementValue::String("012:34:56.123456".into())),
        (
            "dt",
            ElementValue::String("2026-01-02 12:34:56.123456".into()),
        ),
        (
            "ts",
            ElementValue::String("2026-01-02 12:34:56.123456".into()),
        ),
        ("c", ElementValue::String("xy".into())),
        ("tx", ElementValue::String("text".into())),
        ("v", ElementValue::String("caf\u{e9}".into())),
        (
            "b",
            ElementValue::List(vec![Integer(0), Integer(255), Integer(0)]),
        ),
        ("vb", ElementValue::List(vec![Integer(0), Integer(255)])),
        ("bl", ElementValue::List(vec![Integer(255), Integer(1)])),
        ("bits", ElementValue::List(vec![Integer(1), Integer(255)])),
        ("en", ElementValue::String("not unsigned value".into())),
        ("st", ElementValue::String("red,green".into())),
        (
            "js",
            ElementValue::String("{\"value\":[1,true,null]}".into()),
        ),
        ("absent", ElementValue::Null),
    ] {
        assert_eq!(element.get_property(name), &value, "{name}");
    }
    database.admin.execute("FLUSH BINARY LOGS").await?;
    database
        .admin
        .execute("UPDATE items SET mi=-1, yr=0, tm='-123:45:56.000001', b=X'00' WHERE id=1")
        .await?;
    let envelope = next(&mut source).await?;
    let changes =
        SourceTransactionCodec::decode(&envelope.envelope, database.settings.transactions)?
            .into_changes();
    let [SourceChange::Update { element }] = changes.as_slice() else {
        anyhow::bail!("expected a complete typed update after binlog rotation");
    };
    assert_eq!(element.get_property("mi"), &Integer(-1));
    assert_eq!(element.get_property("yr"), &Integer(0));
    assert_eq!(
        element.get_property("tm"),
        &ElementValue::String("-123:45:56.000001".into())
    );
    assert_eq!(
        element.get_property("b"),
        &ElementValue::List(vec![Integer(0), Integer(0), Integer(0)])
    );
    source.stop().await?;
    let (_, catalog) = catalog::inspect(&mut database.admin, &database.settings).await?;
    let mut cursor = database.admin.cursor("SELECT * FROM items").await?;
    catalog::validate_snapshot_columns(&catalog["items"], &cursor.columns)?;
    let mut changed = cursor.columns.clone();
    changed[0] = changed[0].clone().with_name(b"renamed");
    assert!(catalog::validate_snapshot_columns(&catalog["items"], &changed).is_err());
    changed[0] = cursor.columns[0].clone().with_flags(
        cursor.columns[0].flags() ^ mysql_common::constants::ColumnFlags::UNSIGNED_FLAG,
    );
    assert!(catalog::validate_snapshot_columns(&catalog["items"], &changed).is_err());
    let row = database
        .admin
        .fetch(&mut cursor)
        .await?
        .context("snapshot type row")?;
    let initial = crate::decoder::MySqlDecoder::new("mysql", &[]).native_snapshot_element(
        "test",
        "items",
        &cursor.columns,
        row,
        &["id".into()],
        database.settings.max_protocol_bytes.get(),
    )?;
    assert_eq!(initial.get_reference(), element.get_reference());
    for column in &cursor.columns {
        let name = std::str::from_utf8(column.name_ref())?;
        assert_eq!(
            initial.get_property(name),
            element.get_property(name),
            "snapshot/stream mapping: {name}"
        );
    }
    assert!(database.admin.fetch(&mut cursor).await?.is_none());
    database.admin.close_cursor(cursor).await?;
    database.admin.execute("DROP TABLE items").await?;
    database
        .admin
        .execute("CREATE TABLE items (id INT PRIMARY KEY, p POINT)")
        .await?;
    assert!(source
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported native MySQL column type"));
    source.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_native_start_closes_owned_handshake_and_requires_cleanup_before_retry(
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let settings = config(listener.local_addr()?.port(), MySqlOutput::Changes);
    let mut source = MySqlSource::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        settings,
        None,
    )?;
    let (start, peer) = tokio::join!(
        tokio::time::timeout(Duration::from_millis(50), source.start()),
        async {
            let (mut socket, _) = listener.accept().await?;
            socket.write_all(&[100, 0]).await?;
            let mut byte = [0];
            anyhow::ensure!(
                socket.read(&mut byte).await? == 0,
                "cancelled handshake retained its socket"
            );
            Result::<()>::Ok(())
        }
    );
    assert!(start.is_err());
    peer?;
    assert!(source
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("cleanup"));
    source.stop().await?;
    Ok(())
}

struct Settings(serde_json::Value);

#[async_trait::async_trait]
impl drasi_lib::computation::v1::ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(key == "mysql", "unknown MySQL configuration reference");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<serde_json::Value> {
        self.validate_reference(key)?;
        Ok(self.0.clone())
    }
}

impl Database {
    async fn new(retention: MySqlRetention) -> Result<Self> {
        let container = Mysql::default()
            .with_env_var("MYSQL_DATABASE", "test")
            .with_env_var("MYSQL_ROOT_PASSWORD", "root")
            .with_cmd(vec![
                "--log-bin=mysql-bin",
                "--binlog-format=ROW",
                "--binlog-row-image=FULL",
                "--binlog-row-metadata=FULL",
                "--server-id=1",
                "--binlog-expire-logs-seconds=0",
            ])
            .start()
            .await?;
        let port = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(port) = container.ports().await?.map_to_host_port_ipv4(3306) {
                    return Result::<u16>::Ok(port);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "MySQL fixture {} did not publish its IPv4 port",
                container.id()
            )
        })??;
        let mut settings = config(port, MySqlOutput::Transactions);
        let mut admin = connection::Connection::connect(
            &settings.connection,
            settings.max_protocol_bytes.get(),
            settings.timeout(),
        )
        .await?;
        admin
            .execute("CREATE TABLE items (id INT PRIMARY KEY, value VARCHAR(80) NOT NULL)")
            .await?;
        let statement = if admin.version >= (8, 4, 0) {
            "SHOW BINARY LOG STATUS"
        } else {
            "SHOW MASTER STATUS"
        };
        let rows = admin.query(statement).await?;
        settings.start = MySqlStartPosition::Position {
            file: catalog::field(&rows.values[0], 0)?.to_string(),
            position: catalog::field(&rows.values[0], 1)?.parse()?,
        };
        settings.replay = Some(retention);
        Ok(Self {
            _container: container,
            admin,
            settings,
        })
    }
}

fn progress() -> Result<std::sync::Arc<drasi_lib::computation::v1::QuerySourceProgress>> {
    Ok(std::sync::Arc::new(
        drasi_lib::computation::v1::QuerySourceProgress::new(
            "native-mysql",
            ComponentId::try_new("query")?,
        )?,
    ))
}

async fn query(
    path: &std::path::Path,
    progress: std::sync::Arc<drasi_lib::computation::v1::QuerySourceProgress>,
) -> Result<drasi_lib::computation::v1::ContinuousQueryTransformer> {
    query_with_recovery(
        path,
        progress,
        drasi_lib::computation::v1::QueryRecoveryPolicy::Strict,
    )
    .await
}

async fn query_with_recovery(
    path: &std::path::Path,
    progress: std::sync::Arc<drasi_lib::computation::v1::QuerySourceProgress>,
    recovery: drasi_lib::computation::v1::QueryRecoveryPolicy,
) -> Result<drasi_lib::computation::v1::ContinuousQueryTransformer> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use drasi_lib::computation::v1::*;
    ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: "native-mysql".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:items) RETURN n.id AS id, n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: NonZeroUsize::new(20).unwrap(),
        },
        std::sync::Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        QueryOptions {
            recovery,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(config(3306, MySqlOutput::Transactions).transactions),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(progress)
}

fn source(
    settings: MySqlConfig,
    progress: std::sync::Arc<drasi_lib::computation::v1::QuerySourceProgress>,
) -> Result<MySqlSource> {
    MySqlSource::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        settings,
        Some(progress),
    )
}

async fn next(source: &mut MySqlSource) -> Result<drasi_lib::computation::v1::OutputEnvelope> {
    tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("source exhausted")
}

async fn apply(
    query: &mut drasi_lib::computation::v1::ContinuousQueryTransformer,
    output: drasi_lib::computation::v1::OutputEnvelope,
) -> Result<Vec<drasi_lib::computation::v1::OutputEnvelope>> {
    use drasi_lib::computation::v1::*;
    let results = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.envelope,
        })
        .await?;
    query.delivery_completed(&results).await?;
    Ok(results)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_transactions_replay_only_after_actual_query_commit_and_fail_on_purged_history(
) -> Result<()> {
    use drasi_lib::computation::v1::*;
    for retention in [MySqlRetention::Server, MySqlRetention::UntilProcessed] {
        let mut database = Database::new(retention).await?;
        let path = tempfile::tempdir()?;
        let owner = progress()?;
        let mut query = query(path.path(), owner.clone()).await?;
        query.start().await?;
        let mut first = source(database.settings.clone(), owner.clone())?;
        let contract = if retention == MySqlRetention::UntilProcessed {
            ComponentRecovery::admitted(
                drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART,
            )
            .replay_until(owner.component_id().clone())
        } else {
            ComponentRecovery::default()
        };
        assert_eq!(first.recovery_contract(), contract);
        first.start().await?;
        database.admin.execute("BEGIN").await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (1, 'first')")
            .await?;
        database
            .admin
            .execute("UPDATE items SET value='final' WHERE id=1")
            .await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (2, 'temporary')")
            .await?;
        database
            .admin
            .execute("DELETE FROM items WHERE id=2")
            .await?;
        database.admin.execute("COMMIT").await?;
        let uncommitted = next(&mut first).await?;
        let original =
            SourceTransactionCodec::decode(&uncommitted.envelope, database.settings.transactions)?;
        let original_position = original.position().to_vec();
        assert!(owner.snapshot().checkpoints.is_empty());
        first.stop().await?;
        first.start().await?;
        let replayed = next(&mut first).await?;
        assert_eq!(
            SourceTransactionCodec::decode(&replayed.envelope, database.settings.transactions)?
                .position(),
            original_position
        );
        let results = apply(&mut query, replayed).await?;
        let [ChangeOperation::Added { after, .. }] = results[0].envelope.changes().operations()
        else {
            anyhow::bail!("partial transaction visibility");
        };
        let row = QueryChangeCodec::decode_row(after)?;
        assert_eq!(
            QueryChangeCodec::row_values_to_json(&row.values),
            serde_json::json!({"id":1,"value":"final"})
        );
        assert!(!owner.snapshot().checkpoints.is_empty());
        first.stop().await?;
        query.stop().await?;
        drop(first);
        drop(query);
        database
            .admin
            .execute("INSERT INTO items VALUES (3, 'offline')")
            .await?;
        let owner = progress()?;
        let mut restored = self::query(path.path(), owner.clone()).await?;
        restored.start().await?;
        let mut second = source(database.settings.clone(), owner.clone())?;
        second.start().await?;
        let offline = next(&mut second).await?;
        let changes =
            SourceTransactionCodec::decode(&offline.envelope, database.settings.transactions)?
                .into_changes();
        assert_eq!(changes.len(), 1, "committed anchor must not be republished");
        apply(&mut restored, offline).await?;
        assert_eq!(restored.results().snapshot()?.rows.len(), 2);
        second.stop().await?;
        database.admin.execute("FLUSH BINARY LOGS").await?;
        let logs = database.admin.query("SHOW BINARY LOGS").await?;
        let latest = catalog::field(logs.values.last().context("latest binlog")?, 0)?;
        database
            .admin
            .execute(&format!("PURGE BINARY LOGS TO '{latest}'"))
            .await?;
        let error = second.start().await.expect_err("purged replay anchor");
        assert!(
            error
                .downcast_ref::<drasi_lib::sources::SourceError>()
                .is_some(),
            "{error:#}"
        );
        second.stop().await?;
        restored.stop().await?;
        database.admin.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_oversized_transactions_never_publish_a_prefix_and_retry_after_limit_increase(
) -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let mut query = query(path.path(), owner.clone()).await?;
    query.start().await?;
    let mut settings = database.settings.clone();
    settings.transactions.max_changes = NonZeroUsize::new(1).unwrap();
    let mut limited = source(settings, owner.clone())?;
    limited.start().await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1, 'one'), (2, 'two')")
        .await?;
    let error = next(&mut limited).await.expect_err("oversized transaction");
    assert!(
        error
            .downcast_ref::<drasi_lib::computation::v1::SourceTransactionError>()
            .is_some(),
        "{error:#}"
    );
    assert!(owner.snapshot().checkpoints.is_empty());
    assert!(query.results().snapshot()?.rows.is_empty());
    limited.stop().await?;
    let mut retry = source(database.settings, owner.clone())?;
    retry.start().await?;
    apply(&mut query, next(&mut retry).await?).await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 2);
    retry.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_factory_and_direct_sources_deliver_transactions_through_graph_pipes() -> Result<()>
{
    use drasi_lib::computation::v1::*;
    use std::{collections::BTreeMap, sync::Arc};
    for (factory_created, initialize) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
        if factory_created && initialize {
            database
                .admin
                .execute("CREATE USER 'snapshot_reader'@'%' IDENTIFIED BY 'snapshot-test'")
                .await?;
            database
                .admin
                .execute("GRANT SELECT ON test.* TO 'snapshot_reader'@'%'")
                .await?;
            database.admin.execute("GRANT REPLICATION SLAVE, REPLICATION CLIENT, RELOAD ON *.* TO 'snapshot_reader'@'%'").await?;
            database.settings.connection.user = "snapshot_reader".into();
            database.settings.connection.password = "snapshot-test".into();
        }
        let path = tempfile::tempdir()?;
        let owner = progress()?;
        let bootstrap = if initialize {
            database.settings.start = MySqlStartPosition::Snapshot {
                lock_timeout_ms: NonZeroU64::new(2000).unwrap(),
            };
            Some(Arc::new(MySqlSnapshot::new(
                ComponentId::try_new("source")?,
                StreamId::try_new("changes")?,
                database.settings.clone(),
                owner.clone(),
            )?))
        } else {
            None
        };
        let mut query = query(path.path(), owner.clone()).await?;
        if let Some(bootstrap) = &bootstrap {
            query = query.with_bootstrap(bootstrap.clone());
        }
        let (sink, mut received) = drasi_reaction_application::NativeApplicationReaction::channel(
            "sink",
            QueryChangeCodec::schema().descriptor().clone(),
            NonZeroUsize::new(1).unwrap(),
        )?;
        let graph = ComputationGraph::builder("native-mysql")
            .query(Box::new(query))
            .sink(Box::new(sink));
        let graph = if factory_created {
            let factory = Arc::new(MySqlSourceFactory::default());
            let resource = ResourceId::try_new("progress")?;
            let settings = ResourceId::try_new("settings")?;
            let mut spec = ComponentSpecification {
                descriptor: MySqlSource::describe(
                    ComponentId::try_new("source")?,
                    MySqlOutput::Transactions,
                )?,
                role: ComponentRole::Source,
                completion: None,
                implementation: factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::from([
                    (
                        Arc::from("stream"),
                        ConfigurationValue::Literal("changes".into()),
                    ),
                    (
                        Arc::from("settings"),
                        ConfigurationValue::Reference {
                            resource: settings.clone(),
                            key: Arc::from("mysql"),
                            secret: true,
                        },
                    ),
                ]),
                dependencies: BTreeMap::from([(
                    Arc::from("source_progress"),
                    vec![resource.clone()],
                )]),
            };
            if bootstrap.is_some() {
                spec.dependencies.insert(
                    Arc::from("snapshot"),
                    vec![ResourceId::try_new("snapshot")?],
                );
            }
            let mut invalid = spec.clone();
            invalid.dependencies.clear();
            invalid.configuration.insert(
                Arc::from("settings"),
                ConfigurationValue::Literal(serde_json::to_value(&database.settings)?),
            );
            assert!(factory.validate(&invalid).is_err());
            invalid = spec.clone();
            invalid.descriptor =
                MySqlSource::describe(ComponentId::try_new("source")?, MySqlOutput::Changes)?;
            invalid.configuration.insert(
                Arc::from("settings"),
                ConfigurationValue::Literal(serde_json::to_value(&database.settings)?),
            );
            assert!(factory.validate(&invalid).is_err());
            let graph = graph
                .declare_resource(ResourceSpecification {
                    id: settings.clone(),
                    role: ResourceRole::SecretStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("settings"),
                })?
                .provide_resource(
                    settings,
                    ResourceHandle::new(
                        ResourceRole::SecretStore,
                        Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                            serde_json::to_value(&database.settings)?,
                        )))),
                    ),
                )?
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::Checkpoint,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("progress"),
                })?
                .provide_resource(
                    resource,
                    ResourceHandle::new(
                        ResourceRole::Checkpoint,
                        Arc::new(QuerySourceProgressResource(owner.clone())),
                    ),
                )?;
            let graph = if let Some(bootstrap) = bootstrap {
                let id = ResourceId::try_new("snapshot")?;
                graph
                    .declare_resource(ResourceSpecification {
                        id: id.clone(),
                        role: ResourceRole::Bootstrap,
                        ownership: ResourceOwnership::Borrowed,
                        binding: Arc::from("mysql-snapshot"),
                    })?
                    .provide_resource(id, ResourceHandle::new(ResourceRole::Bootstrap, bootstrap))?
            } else {
                graph
            };
            graph.component(spec, factory)
        } else {
            let source = if let Some(bootstrap) = bootstrap {
                MySqlSource::with_snapshot(
                    ComponentId::try_new("source")?,
                    StreamId::try_new("changes")?,
                    database.settings.clone(),
                    owner,
                    bootstrap,
                )?
            } else {
                source(database.settings.clone(), owner)?
            };
            graph.source(Box::new(source))
        };
        let endpoint = |component: &str, port: &str| {
            Endpoint::new(
                ComponentId::try_new(component).unwrap(),
                PortId::try_new(port).unwrap(),
            )
        };
        let mut graph = graph
            .bind_stream(endpoint("source", "out"), StreamId::try_new("changes")?)
            .bind_stream(endpoint("query", "out"), StreamId::try_new("results")?)
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let (run_result, result) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(run, async {
                let result = async {
                    let report = control
                        .start_components(GraphRevision(1), GraphSelection::All)
                        .await?;
                    anyhow::ensure!(
                        report.summary == OperationSummary::Completed,
                        "MySQL graph startup: {report:?}"
                    );
                    database.admin.execute("BEGIN").await?;
                    database
                        .admin
                        .execute("INSERT INTO items VALUES (1, 'first')")
                        .await?;
                    database
                        .admin
                        .execute("UPDATE items SET value='last' WHERE id=1")
                        .await?;
                    database.admin.execute("COMMIT").await?;
                    let output = received.recv().await.context("sink closed")?;
                    let [ChangeOperation::Added { after, .. }] =
                        output.envelope.changes().operations()
                    else {
                        anyhow::bail!("partial MySQL transaction");
                    };
                    let row = QueryChangeCodec::decode_row(after)?;
                    assert_eq!(
                        QueryChangeCodec::row_values_to_json(&row.values),
                        serde_json::json!({"id":1,"value":"last"})
                    );
                    let report = control
                        .stop_components(GraphRevision(1), GraphSelection::All)
                        .await?;
                    anyhow::ensure!(
                        report.summary == OperationSummary::Completed,
                        "MySQL graph stop: {report:?}"
                    );
                    anyhow::Result::<()>::Ok(())
                }
                .await;
                control.cancel();
                result
            })
        })
        .await?;
        result?;
        assert!(matches!(run_result, Err(GraphError::Cancelled)));
        graph.shutdown().await?;
        database.admin.close().await?;
    }
    Ok(())
}
