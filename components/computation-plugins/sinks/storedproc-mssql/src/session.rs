// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! [`Session`] over one Tiberius connection, and the connector that opens it.
//!
//! Every statement here is parameterized except for the progress table name,
//! which is validated (`ProgressTable`) and bracket-quoted. Tiberius cannot
//! send a TDS ATTENTION, so a statement cannot be cancelled in place: the
//! per-round-trip `commandTimeoutMs` and the host dropping a call both drop
//! the connection, and SQL Server rolls back the open transaction.

use crate::{
    config::{MsSqlStoredProcConfig, ProgressTable},
    delivery::{
        ConnectTimeout, Connector, CursorRow, DeliveryError, Position, Session,
        PROGRESS_FORMAT_VERSION,
    },
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use drasi_mssql_common::sql::{self, RenderedCommand, SqlClient};
use futures_util::TryStreamExt;
use std::time::Duration;
use tiberius::{AuthMethod, Query};

/// Error number raised when first-stream admission cannot take its lock.
/// Classified as retryable.
pub const ADMISSION_LOCK_ERROR: u32 = 59_101;

/// How a session authenticates. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub enum Authentication {
    /// SQL Server authentication.
    Sql { user: String, password: String },
    /// A Microsoft Entra ID access token (Tiberius `AADToken`).
    AccessToken { token: String },
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Sql { .. } => "Sql([REDACTED])",
            Self::AccessToken { .. } => "AccessToken([REDACTED])",
        })
    }
}

impl Authentication {
    fn method(&self) -> AuthMethod {
        match self {
            Self::Sql { user, password } => AuthMethod::sql_server(user, password),
            Self::AccessToken { token } => AuthMethod::aad_token(token),
        }
    }
}

/// Builds the Tiberius configuration for the sink's settings.
pub fn tiberius_config(
    config: &MsSqlStoredProcConfig,
    authentication: &Authentication,
) -> tiberius::Config {
    let mut tiberius = tiberius::Config::new();
    tiberius.host(&config.host);
    tiberius.port(config.port);
    tiberius.database(&config.database);
    tiberius.application_name("drasi-native-storedproc-mssql");
    tiberius.encryption(config.encryption.level());
    // `validate` makes these mutually exclusive; Tiberius panics otherwise.
    if config.trust_server_certificate {
        tiberius.trust_cert();
    } else if let Some(pem) = &config.trusted_ca_pem {
        tiberius.trust_cert_ca_bundle(pem.as_bytes().to_vec());
    }
    if let Some(name) = &config.host_name_in_certificate {
        tiberius.hostname_in_certificate(name);
    }
    tiberius.command_timeout(Some(config.command_timeout()));
    tiberius.handshake_timeout(Some(config.connect_timeout()));
    tiberius.authentication(authentication.method());
    tiberius
}

/// Opens [`TiberiusSession`]s.
pub struct TiberiusConnector {
    config: tiberius::Config,
    connect_timeout: Duration,
    table: ProgressTable,
}

impl TiberiusConnector {
    pub fn new(config: &MsSqlStoredProcConfig, authentication: &Authentication) -> Result<Self> {
        Ok(Self {
            config: tiberius_config(config, authentication),
            connect_timeout: config.connect_timeout(),
            table: config.progress_table()?,
        })
    }
}

#[async_trait]
impl Connector for TiberiusConnector {
    type Session = TiberiusSession;

    async fn connect(&self) -> Result<TiberiusSession> {
        let mut client =
            tokio::time::timeout(self.connect_timeout, sql::connect(self.config.clone()))
                .await
                .map_err(|_| ConnectTimeout(self.connect_timeout))?
                .context("connecting to SQL Server")?;
        drain(
            client
                .simple_query("SET XACT_ABORT ON; SET NOCOUNT ON;")
                .await?,
        )
        .await?;
        Ok(TiberiusSession {
            client,
            table: self.table.quoted(),
        })
    }
}

/// One connection and the quoted progress table name.
pub struct TiberiusSession {
    client: SqlClient,
    table: String,
}

async fn drain(mut stream: tiberius::QueryStream<'_>) -> Result<()> {
    while stream.try_next().await?.is_some() {}
    Ok(())
}

/// The progress table DDL (format 2). Columns are NVARCHAR so the bound
/// string parameters seek the primary key without conversion.
pub fn create_table_sql(table: &str) -> String {
    format!(
        "CREATE TABLE {table} (
            owner NVARCHAR(64) NOT NULL,
            stream_key NVARCHAR(128) NOT NULL,
            version SMALLINT NOT NULL CHECK (version = {PROGRESS_FORMAT_VERSION}),
            sequence NVARCHAR(20) NOT NULL,
            digest VARBINARY(32) NOT NULL CHECK (DATALENGTH(digest) = 32),
            operations BIGINT NOT NULL CHECK (operations > 0),
            completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations),
            PRIMARY KEY (owner, stream_key)
        )"
    )
}

impl TiberiusSession {
    async fn scalar_i64(&mut self, query: Query<'_>) -> Result<i64> {
        let row = query
            .query(&mut self.client)
            .await?
            .into_row()
            .await?
            .context("SQL Server returned no row")?;
        row.try_get::<i64, _>(0)?
            .context("SQL Server returned NULL")
    }
}

#[async_trait]
impl Session for TiberiusSession {
    async fn prepare(&mut self) -> Result<()> {
        let table = &self.table;
        // `table` is a validated, bracket-quoted identifier without quotes, so
        // it is safe inside the N'...' literal as well.
        let create = format!(
            "SET XACT_ABORT ON;
             BEGIN TRANSACTION;
             DECLARE @lock INT;
             EXEC @lock = sp_getapplock @Resource = N'drasi.storedproc/progress-table/v2',
                 @LockMode = N'Exclusive', @LockOwner = N'Transaction', @LockTimeout = -1;
             IF @lock < 0 THROW {ADMISSION_LOCK_ERROR}, N'progress table creation lock not granted', 1;
             IF OBJECT_ID(N'{table}', N'U') IS NULL
                 {};
             COMMIT TRANSACTION;",
            create_table_sql(table)
        );
        let result = async {
            drain(self.client.simple_query(create).await?).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = drain_rollback(&mut self.client).await;
            return Err(error.context(format!(
                "creating the progress table {table}; grant CREATE TABLE and ALTER on the \
                 schema, or pre-create the table (see CONFIGURATION.md)"
            )));
        }
        let mut access = Query::new(
            "SELECT CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'SELECT') AS INT),
                    CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'INSERT') AS INT),
                    CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'UPDATE') AS INT),
                    CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'DELETE') AS INT)",
        );
        access.bind(table.as_str());
        let row = access
            .query(&mut self.client)
            .await?
            .into_row()
            .await?
            .context("SQL Server returned no permission row")?;
        for (index, permission) in ["SELECT", "INSERT", "UPDATE", "DELETE"].iter().enumerate() {
            if row.try_get::<i32, _>(index)? != Some(1) {
                bail!(
                    "the login lacks {permission} on the progress table {table}; grant SELECT, \
                     INSERT, UPDATE and DELETE on it (see CONFIGURATION.md)"
                );
            }
        }
        let shape = format!(
            "SELECT TOP (0) owner, stream_key, version, sequence, digest, operations, completed \
             FROM {table}"
        );
        drain(self.client.simple_query(shape).await?)
            .await
            .with_context(|| {
                format!("the progress table {table} does not have the format-2 columns")
            })
    }

    async fn begin(&mut self) -> Result<()> {
        drain(
            self.client
                .simple_query("SET XACT_ABORT ON; BEGIN TRANSACTION;")
                .await?,
        )
        .await
    }

    async fn read_cursor(&mut self, owner: &str, stream: &str) -> Result<Option<CursorRow>> {
        let mut query = Query::new(format!(
            "SELECT version, sequence, digest, operations, completed FROM {} \
             WITH (UPDLOCK, HOLDLOCK) WHERE owner = @P1 AND stream_key = @P2",
            self.table
        ));
        query.bind(owner);
        query.bind(stream);
        let Some(row) = query.query(&mut self.client).await?.into_row().await? else {
            return Ok(None);
        };
        Ok(Some(CursorRow {
            version: row.try_get::<i16, _>(0)?.context("NULL version")?,
            sequence: row
                .try_get::<&str, _>(1)?
                .context("NULL sequence")?
                .to_owned(),
            digest: row.try_get::<&[u8], _>(2)?.context("NULL digest")?.to_vec(),
            operations: row.try_get::<i64, _>(3)?.context("NULL operations")?,
            completed: row.try_get::<i64, _>(4)?.context("NULL completed")?,
        }))
    }

    async fn lock_admission(&mut self, owner: &str) -> Result<()> {
        let mut query = Query::new(format!(
            "DECLARE @lock INT;
             EXEC @lock = sp_getapplock @Resource = @P1, @LockMode = N'Exclusive',
                 @LockOwner = N'Transaction', @LockTimeout = -1;
             IF @lock < 0 THROW {ADMISSION_LOCK_ERROR}, N'delivery stream admission lock not granted', 1;"
        ));
        query.bind(format!("drasi.storedproc/v2/{owner}"));
        drain(query.query(&mut self.client).await?).await
    }

    async fn count_streams(&mut self, owner: &str) -> Result<i64> {
        let mut query = Query::new(format!(
            "SELECT COUNT_BIG(*) FROM {} WHERE owner = @P1",
            self.table
        ));
        query.bind(owner);
        self.scalar_i64(query).await
    }

    async fn insert_cursor(&mut self, owner: &str, position: &Position) -> Result<()> {
        let mut query = Query::new(format!(
            "INSERT INTO {} (owner, stream_key, version, sequence, digest, operations, completed) \
             VALUES (@P1, @P2, {PROGRESS_FORMAT_VERSION}, @P3, @P4, @P5, 0)",
            self.table
        ));
        query.bind(owner);
        query.bind(position.stream.as_str());
        query.bind(position.sequence.to_string());
        query.bind(position.digest.to_vec());
        query.bind(position.operations);
        query.execute(&mut self.client).await?;
        Ok(())
    }

    async fn rollover_cursor(&mut self, owner: &str, position: &Position) -> Result<()> {
        let mut query = Query::new(format!(
            "UPDATE {} SET version = {PROGRESS_FORMAT_VERSION}, sequence = @P3, digest = @P4, \
             operations = @P5, completed = 0 WHERE owner = @P1 AND stream_key = @P2",
            self.table
        ));
        query.bind(owner);
        query.bind(position.stream.as_str());
        query.bind(position.sequence.to_string());
        query.bind(position.digest.to_vec());
        query.bind(position.operations);
        query.execute(&mut self.client).await?;
        Ok(())
    }

    async fn execute(&mut self, command: &RenderedCommand) -> Result<()> {
        let mut query = Query::new(command.sql.as_str());
        sql::bind_parameters(&mut query, &command.parameters);
        drain(query.query(&mut self.client).await?).await
    }

    async fn advance_cursor(&mut self, owner: &str, stream: &str, completed: i64) -> Result<u64> {
        let mut query = Query::new(format!(
            "IF @@TRANCOUNT <> 1
             BEGIN
                 SELECT CAST(-1 AS BIGINT), CAST(@@TRANCOUNT AS BIGINT);
                 RETURN;
             END;
             UPDATE {} SET completed = completed + 1
             WHERE owner = @P1 AND stream_key = @P2 AND completed = @P3;
             SELECT CAST(@@ROWCOUNT AS BIGINT), CAST(@@TRANCOUNT AS BIGINT);",
            self.table
        ));
        query.bind(owner);
        query.bind(stream);
        query.bind(completed);
        let row = query
            .query(&mut self.client)
            .await?
            .into_row()
            .await?
            .context("SQL Server returned no cursor row count")?;
        let changed = row.try_get::<i64, _>(0)?.context("NULL row count")?;
        if changed < 0 {
            let depth = row.try_get::<i64, _>(1)?.unwrap_or_default();
            return Err(DeliveryError::TransactionStateChanged { depth }.into());
        }
        Ok(u64::try_from(changed)?)
    }

    async fn commit(&mut self) -> Result<()> {
        drain(self.client.simple_query("COMMIT TRANSACTION;").await?).await
    }

    async fn rollback(&mut self) -> Result<()> {
        drain_rollback(&mut self.client).await
    }

    async fn erase(&mut self, owner: &str) -> Result<u64> {
        let mut query = Query::new(format!(
            "DECLARE @erased BIGINT = 0;
             IF OBJECT_ID(@P2, N'U') IS NOT NULL
             BEGIN
                 DELETE FROM {} WHERE owner = @P1;
                 SET @erased = @@ROWCOUNT;
             END;
             SELECT @erased;",
            self.table
        ));
        query.bind(owner);
        query.bind(self.table.clone());
        Ok(u64::try_from(self.scalar_i64(query).await?)?)
    }

    async fn close(self) -> Result<()> {
        self.client.close().await?;
        Ok(())
    }
}

async fn drain_rollback(client: &mut SqlClient) -> Result<()> {
    drain(
        client
            .simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
            .await?,
    )
    .await
}
