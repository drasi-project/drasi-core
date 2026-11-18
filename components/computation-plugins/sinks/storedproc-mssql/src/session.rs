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
    config::MsSqlStoredProcConfig,
    delivery::{self, ADMISSION_LOCK_ERROR, TRANSACTION_COUNT_MISMATCH},
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use drasi_computation_storedproc_common::{
    cursor::{valid_owner, CursorRow, Position, ProgressTable, PROGRESS_FORMAT_VERSION},
    driver::{CommitError, Connector, ErrorClass, Session, TransactionEnded},
};
use drasi_mssql_common::sql::{self, RenderedCommand, SqlClient};
use futures_util::TryStreamExt;
use std::time::Duration;
use tiberius::Query;

/// The default schema of an unqualified progress table.
pub const DEFAULT_SCHEMA: &str = "dbo";

/// The bracket-quoted, schema-qualified name of `table`.
pub fn quoted(table: &ProgressTable) -> String {
    table.quoted(DEFAULT_SCHEMA, |part| {
        format!("[{}]", part.replace(']', "]]"))
    })
}

/// How a session authenticates; shared with the native SQL Server source.
pub use drasi_mssql_common::client::Authentication;

/// Builds the Tiberius configuration for the sink's settings.
pub fn tiberius_config(
    config: &MsSqlStoredProcConfig,
    authentication: &Authentication,
) -> tiberius::Config {
    config.connection_settings().tiberius_config(authentication)
}

/// Opens [`TiberiusSession`]s bound to one owner and progress table.
pub struct TiberiusConnector {
    config: tiberius::Config,
    connect_timeout: Duration,
    table: ProgressTable,
    owner: String,
}

impl TiberiusConnector {
    pub fn new(
        config: &MsSqlStoredProcConfig,
        authentication: &Authentication,
        owner: String,
    ) -> Result<Self> {
        anyhow::ensure!(valid_owner(&owner), "invalid destination cursor owner");
        Ok(Self {
            config: tiberius_config(config, authentication),
            connect_timeout: config.connect_timeout(),
            table: config.progress_table()?,
            owner,
        })
    }
}

#[async_trait]
impl Connector for TiberiusConnector {
    type Session = TiberiusSession;

    async fn connect(&mut self) -> Result<TiberiusSession> {
        let mut client =
            drasi_mssql_common::client::connect_within(self.config.clone(), self.connect_timeout)
                .await?;
        drain(
            client
                .simple_query("SET XACT_ABORT ON; SET NOCOUNT ON;")
                .await?,
        )
        .await?;
        Ok(TiberiusSession {
            client,
            table: quoted(&self.table),
            dead_letter: quoted(&self.table.dead_letter()),
            owner: self.owner.clone(),
        })
    }
}

/// One connection, its owner and the quoted progress and dead-letter tables.
pub struct TiberiusSession {
    client: SqlClient,
    table: String,
    dead_letter: String,
    owner: String,
}

async fn drain(mut stream: tiberius::QueryStream<'_>) -> Result<()> {
    while stream.try_next().await?.is_some() {}
    Ok(())
}

/// The cursor (format 4; `epoch` is empty for exactly-once rows) and
/// dead-letter table DDL, each created only when missing. Columns are
/// NVARCHAR so the bound string parameters seek the primary key without
/// conversion. Operators who pre-create the tables run exactly this batch.
pub fn create_table_sql(table: &ProgressTable) -> String {
    create_tables(&quoted(table), &quoted(&table.dead_letter()))
}

fn create_tables(progress: &str, dead_letter: &str) -> String {
    format!(
        "IF OBJECT_ID(N'{progress}', N'U') IS NULL
         CREATE TABLE {progress} (
            owner NVARCHAR(64) NOT NULL,
            stream_key NVARCHAR(128) NOT NULL,
            version SMALLINT NOT NULL CHECK (version = {PROGRESS_FORMAT_VERSION}),
            sequence NVARCHAR(20) NOT NULL,
            digest VARBINARY(32) NOT NULL CHECK (DATALENGTH(digest) = 32),
            operations BIGINT NOT NULL CHECK (operations > 0),
            completed BIGINT NOT NULL,
            epoch NVARCHAR(64) NOT NULL,
            generation BIGINT NOT NULL CHECK (generation >= 0),
            pending BIGINT NOT NULL,
            CHECK (completed >= 0 AND completed <= operations),
            CHECK (pending >= -1 AND pending < operations),
            PRIMARY KEY (owner, stream_key)
        );
        IF OBJECT_ID(N'{dead_letter}', N'U') IS NULL
         CREATE TABLE {dead_letter} (
            owner NVARCHAR(64) NOT NULL,
            delivery_id NVARCHAR(128) NOT NULL,
            stream_key NVARCHAR(128) NOT NULL,
            sequence NVARCHAR(20) NOT NULL,
            operation BIGINT NOT NULL,
            epoch NVARCHAR(64) NOT NULL,
            generation BIGINT NOT NULL,
            error NVARCHAR(4000) NOT NULL,
            recorded_at DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME(),
            PRIMARY KEY (owner, delivery_id)
        );"
    )
}

impl TiberiusSession {
    fn create_sql(&self) -> String {
        create_tables(&self.table, &self.dead_letter)
    }

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
    type Command = RenderedCommand;

    fn classify(error: &anyhow::Error) -> ErrorClass {
        delivery::classify(error)
    }

    async fn prepare(&mut self) -> Result<()> {
        let table = self.table.clone();
        // The quoted names hold validated identifiers without quotes, so they
        // are safe inside the N'...' literals as well.
        let create = format!(
            "SET XACT_ABORT ON;
             BEGIN TRANSACTION;
             DECLARE @lock INT;
             EXEC @lock = sp_getapplock @Resource = N'drasi.storedproc/progress-table/v4',
                 @LockMode = N'Exclusive', @LockOwner = N'Transaction', @LockTimeout = -1;
             IF @lock < 0 THROW {ADMISSION_LOCK_ERROR}, N'progress table creation lock not granted', 1;
             {}
             COMMIT TRANSACTION;",
            self.create_sql()
        );
        let result = async {
            drain(self.client.simple_query(create).await?).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = drain_rollback(&mut self.client).await;
            return Err(error.context(format!(
                "creating the progress tables {table} and {}; grant CREATE TABLE and ALTER on the \
                 schema, or pre-create them (see CONFIGURATION.md)",
                self.dead_letter
            )));
        }
        for (name, columns) in [
            (
                self.table.clone(),
                "owner, stream_key, version, sequence, digest, operations, completed, epoch, \
                 generation, pending",
            ),
            (
                self.dead_letter.clone(),
                "owner, delivery_id, stream_key, sequence, operation, epoch, generation, error, \
                 recorded_at",
            ),
        ] {
            let mut access = Query::new(
                "SELECT CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'SELECT') AS INT),
                        CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'INSERT') AS INT),
                        CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'UPDATE') AS INT),
                        CAST(HAS_PERMS_BY_NAME(@P1, N'OBJECT', N'DELETE') AS INT)",
            );
            access.bind(name.as_str());
            let row = access
                .query(&mut self.client)
                .await?
                .into_row()
                .await?
                .context("SQL Server returned no permission row")?;
            for (index, permission) in ["SELECT", "INSERT", "UPDATE", "DELETE"].iter().enumerate() {
                if row.try_get::<i32, _>(index)? != Some(1) {
                    bail!(
                        "the login lacks {permission} on {name}; grant SELECT, INSERT, UPDATE \
                         and DELETE on the progress and dead-letter tables (see CONFIGURATION.md)"
                    );
                }
            }
            let shape = format!("SELECT TOP (0) {columns} FROM {name}");
            drain(self.client.simple_query(shape).await?)
                .await
                .with_context(|| {
                    format!(
                        "{name} does not have the format-{PROGRESS_FORMAT_VERSION} columns \
                         (earlier formats are not migrated; drop it or configure another \
                         progressTable)"
                    )
                })?;
        }
        Ok(())
    }

    async fn begin(&mut self) -> Result<()> {
        drain(
            self.client
                .simple_query("SET XACT_ABORT ON; BEGIN TRANSACTION;")
                .await?,
        )
        .await
    }

    async fn read_cursor(&mut self, stream: &str) -> Result<Option<CursorRow>> {
        let mut query = Query::new(format!(
            "SELECT version, sequence, digest, operations, completed, epoch, generation, pending \
             FROM {} WITH (UPDLOCK, HOLDLOCK) WHERE owner = @P1 AND stream_key = @P2",
            self.table
        ));
        query.bind(self.owner.as_str());
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
            epoch: row.try_get::<&str, _>(5)?.context("NULL epoch")?.to_owned(),
            generation: row.try_get::<i64, _>(6)?.context("NULL generation")?,
            pending: row.try_get::<i64, _>(7)?.context("NULL pending")?,
        }))
    }

    async fn lock_admission(&mut self) -> Result<()> {
        let mut query = Query::new(format!(
            "DECLARE @lock INT;
             EXEC @lock = sp_getapplock @Resource = @P1, @LockMode = N'Exclusive',
                 @LockOwner = N'Transaction', @LockTimeout = -1;
             IF @lock < 0 THROW {ADMISSION_LOCK_ERROR}, N'delivery stream admission lock not granted', 1;"
        ));
        query.bind(format!("drasi.storedproc/v4/{}", self.owner));
        drain(query.query(&mut self.client).await?).await
    }

    async fn count_streams(&mut self) -> Result<i64> {
        let mut query = Query::new(format!(
            "SELECT COUNT_BIG(*) FROM {} WHERE owner = @P1",
            self.table
        ));
        query.bind(self.owner.clone());
        self.scalar_i64(query).await
    }

    async fn insert_cursor(&mut self, position: &Position) -> Result<()> {
        let mut query = Query::new(format!(
            "INSERT INTO {} (owner, stream_key, version, sequence, digest, operations, completed, \
             epoch, generation, pending) VALUES (@P1, @P2, {PROGRESS_FORMAT_VERSION}, @P3, @P4, \
             @P5, 0, @P6, @P7, 0)",
            self.table
        ));
        bind_position(&mut query, &self.owner, position);
        query.execute(&mut self.client).await?;
        Ok(())
    }

    async fn rollover_cursor(&mut self, position: &Position) -> Result<()> {
        let mut query = Query::new(format!(
            "UPDATE {} SET version = {PROGRESS_FORMAT_VERSION}, sequence = @P3, digest = @P4, \
             operations = @P5, completed = 0, epoch = @P6, generation = @P7, pending = 0 \
             WHERE owner = @P1 AND stream_key = @P2",
            self.table
        ));
        bind_position(&mut query, &self.owner, position);
        query.execute(&mut self.client).await?;
        Ok(())
    }

    async fn mark_pending(&mut self, stream: &str, operation: i64) -> Result<()> {
        let mut query = Query::new(format!(
            "UPDATE {} SET pending = @P3 WHERE owner = @P1 AND stream_key = @P2",
            self.table
        ));
        query.bind(self.owner.as_str());
        query.bind(stream);
        query.bind(operation);
        query.execute(&mut self.client).await?;
        Ok(())
    }

    async fn execute(&mut self, command: &RenderedCommand) -> Result<()> {
        let mut query = Query::new(command.sql.as_str());
        sql::bind_parameters(&mut query, &command.parameters);
        let result = async { drain(query.query(&mut self.client).await?).await }.await;
        match result {
            // XACT_ABORT with a procedure ROLLBACK or COMMIT: the transaction
            // count no longer matches.
            Err(error) if delivery::server_code(&error) == Some(TRANSACTION_COUNT_MISMATCH) => {
                Err(error.context(TransactionEnded))
            }
            result => result,
        }
    }

    async fn record_skip(&mut self, position: &Position, error: &str) -> Result<()> {
        let mut query = Query::new(format!(
            "UPDATE {dead_letter} SET error = @P8, recorded_at = SYSUTCDATETIME()
             WHERE owner = @P1 AND delivery_id = @P2;
             IF @@ROWCOUNT = 0
                 INSERT INTO {dead_letter} (owner, delivery_id, stream_key, sequence, operation,
                     epoch, generation, error)
                 VALUES (@P1, @P2, @P3, @P4, @P5, @P6, @P7, @P8);",
            dead_letter = self.dead_letter
        ));
        query.bind(self.owner.as_str());
        query.bind(position.delivery_id.as_str());
        query.bind(position.stream.as_str());
        query.bind(position.sequence.to_string());
        query.bind(position.operation);
        query.bind(position.epoch.as_str());
        query.bind(position.generation);
        query.bind(error);
        drain(query.query(&mut self.client).await?).await
    }

    async fn advance_cursor(&mut self, stream: &str, completed: i64) -> Result<u64> {
        let mut query = Query::new(format!(
            "IF @@TRANCOUNT <> 1
             BEGIN
                 SELECT CAST(-1 AS BIGINT), CAST(@@TRANCOUNT AS BIGINT);
                 RETURN;
             END;
             UPDATE {} SET completed = completed + 1, pending = -1
             WHERE owner = @P1 AND stream_key = @P2 AND completed = @P3;
             SELECT CAST(@@ROWCOUNT AS BIGINT), CAST(@@TRANCOUNT AS BIGINT);",
            self.table
        ));
        query.bind(self.owner.as_str());
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
            return Err(anyhow::Error::new(TransactionEnded).context(format!(
                "transaction depth {depth} after the command, expected 1"
            )));
        }
        Ok(u64::try_from(changed)?)
    }

    async fn commit(&mut self) -> std::result::Result<(), CommitError> {
        let result =
            async { drain(self.client.simple_query("COMMIT TRANSACTION;").await?).await }.await;
        match result {
            Ok(()) => Ok(()),
            // A server error on a live connection is a definite answer.
            Err(error)
                if delivery::server_code(&error).is_some()
                    && !matches!(
                        delivery::classify(&error),
                        ErrorClass::Transient { reconnect: true }
                    ) =>
            {
                Err(CommitError::Rejected(error))
            }
            Err(error) => Err(CommitError::Unknown(error)),
        }
    }

    async fn rollback(&mut self) -> Result<()> {
        drain_rollback(&mut self.client).await
    }

    async fn erase(&mut self) -> Result<u64> {
        let mut query = Query::new(format!(
            "DECLARE @erased BIGINT = 0;
             IF OBJECT_ID(@P3, N'U') IS NOT NULL
                 DELETE FROM {dead_letter} WHERE owner = @P1;
             IF OBJECT_ID(@P2, N'U') IS NOT NULL
             BEGIN
                 DELETE FROM {table} WHERE owner = @P1;
                 SET @erased = @@ROWCOUNT;
             END;
             SELECT @erased;",
            table = self.table,
            dead_letter = self.dead_letter
        ));
        query.bind(self.owner.clone());
        query.bind(self.table.clone());
        query.bind(self.dead_letter.clone());
        Ok(u64::try_from(self.scalar_i64(query).await?)?)
    }

    async fn close(self) -> Result<()> {
        self.client.close().await?;
        Ok(())
    }
}

fn bind_position(query: &mut Query<'_>, owner: &str, position: &Position) {
    query.bind(owner.to_owned());
    query.bind(position.stream.clone());
    query.bind(position.sequence.to_string());
    query.bind(position.digest.to_vec());
    query.bind(position.operations);
    query.bind(position.epoch.clone());
    query.bind(position.generation);
}

async fn drain_rollback(client: &mut SqlClient) -> Result<()> {
    drain(
        client
            .simple_query("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
            .await?,
    )
    .await
}
