// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{future::Future, sync::Mutex};

pub(super) enum Command {
    Transaction {
        commands: mpsc::Receiver<TransactionCommand>,
        ready: oneshot::Sender<Result<()>>,
        completed: oneshot::Sender<Result<()>>,
    },
    Query {
        sql: String,
        params: Vec<SqliteParam>,
        response: oneshot::Sender<Result<Vec<SqliteRow>>>,
    },
}

pub(super) enum TransactionCommand {
    Execute {
        sql: String,
        params: Vec<SqliteParam>,
        batch: bool,
        response: oneshot::Sender<Result<usize>>,
    },
    Query {
        sql: String,
        params: Vec<SqliteParam>,
        response: oneshot::Sender<Result<Vec<SqliteRow>>>,
    },
    Finish {
        commit: bool,
    },
    Reject {
        error: anyhow::Error,
        response: oneshot::Sender<Result<()>>,
    },
}

#[derive(Clone)]
pub struct SqliteHandle {
    commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    capacity: usize,
    max_sql_bytes: usize,
}

impl SqliteHandle {
    pub(super) fn new(
        commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
        config: &SqliteConfig,
    ) -> Self {
        Self {
            commands,
            capacity: config.command_capacity.get(),
            max_sql_bytes: config.max_sql_bytes.get(),
        }
    }

    async fn sender(&self) -> Result<mpsc::Sender<Command>> {
        self.commands
            .read()
            .await
            .clone()
            .context("native SQLite source is not running")
    }

    pub async fn execute(&self, sql: impl Into<String>) -> Result<usize> {
        self.execute_parameterized(sql, Vec::new()).await
    }
    pub async fn execute_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<usize> {
        let sql = sql.into();
        validate_sql(&sql, &params, self.max_sql_bytes)?;
        self.transaction(|tx| async move { tx.execute_parameterized(sql, params).await })
            .await
    }
    /// A script is one transaction; no committed prefix survives a failed statement.
    pub async fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        let sql = sql.into();
        validate_sql(&sql, &[], self.max_sql_bytes)?;
        self.transaction(|tx| async move { tx.execute_batch(sql).await })
            .await
    }
    pub async fn query(&self, sql: impl Into<String>) -> Result<Vec<SqliteRow>> {
        self.query_parameterized(sql, Vec::new()).await
    }
    /// Query operations must be read-only. Their result rows and bytes are bounded.
    pub async fn query_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<Vec<SqliteRow>> {
        let sql = sql.into();
        validate_sql(&sql, &params, self.max_sql_bytes)?;
        let (response, result) = oneshot::channel();
        self.sender()
            .await?
            .send(Command::Query {
                sql,
                params,
                response,
            })
            .await?;
        result
            .await
            .context("native SQLite query response closed")?
    }
    /// SQL errors abort the entire scope even if caught by the callback.
    /// Cancellation before commit requests rollback; after commit submission its
    /// outcome may be unknown to the caller. Do not blindly retry a cancelled write.
    pub async fn transaction<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(SqliteTransactionHandle) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let sender = self.sender().await?;
        let (commands, receiver) = mpsc::channel(self.capacity);
        let cleanup = commands.clone().reserve_owned().await?;
        let active = Arc::new(Mutex::new(true));
        let mut scope = Scope {
            cleanup: Some(cleanup),
            active: active.clone(),
        };
        let (ready, started) = oneshot::channel();
        let (completed, mut completion) = oneshot::channel();
        sender
            .send(Command::Transaction {
                commands: receiver,
                ready,
                completed,
            })
            .await?;
        started
            .await
            .context("native SQLite transaction startup closed")??;
        let callback = f(SqliteTransactionHandle {
            commands,
            active,
            max_sql_bytes: self.max_sql_bytes,
        });
        tokio::pin!(callback);
        let result = tokio::select! {
            biased;
            result = &mut callback => result,
            completed = &mut completion => {
                completed.context("native SQLite transaction completion closed")??;
                anyhow::bail!("native SQLite transaction ended before its callback completed");
            }
        };
        scope.finish(result.is_ok())?;
        let cleanup = completion
            .await
            .context("native SQLite transaction completion closed")?;
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
                "native SQLite transaction failed",
                vec![error, cleanup],
            )
            .into()),
        }
    }
}

#[derive(Clone)]
pub struct SqliteTransactionHandle {
    commands: mpsc::Sender<TransactionCommand>,
    active: Arc<Mutex<bool>>,
    max_sql_bytes: usize,
}

impl SqliteTransactionHandle {
    async fn validate(&self, sql: &str, params: &[SqliteParam]) -> Result<()> {
        if let Err(error) = validate_sql(sql, params, self.max_sql_bytes) {
            return self
                .request(|response| TransactionCommand::Reject { error, response })
                .await;
        }
        Ok(())
    }
    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> TransactionCommand,
    ) -> Result<T> {
        let permit = self
            .commands
            .reserve()
            .await
            .context("native SQLite transaction scope has ended")?;
        let (response, result) = oneshot::channel();
        {
            let active = self
                .active
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite scope lease poisoned"))?;
            anyhow::ensure!(*active, "native SQLite transaction scope has ended");
            permit.send(command(response));
        }
        result
            .await
            .context("native SQLite transaction response closed")?
    }
    pub async fn execute(&self, sql: impl Into<String>) -> Result<usize> {
        self.execute_parameterized(sql, Vec::new()).await
    }
    pub async fn execute_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<usize> {
        let sql = sql.into();
        self.validate(&sql, &params).await?;
        self.request(|response| TransactionCommand::Execute {
            sql,
            params,
            batch: false,
            response,
        })
        .await
    }
    pub async fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        let sql = sql.into();
        self.validate(&sql, &[]).await?;
        self.request(|response| TransactionCommand::Execute {
            sql,
            params: Vec::new(),
            batch: true,
            response,
        })
        .await?;
        Ok(())
    }
    pub async fn query(&self, sql: impl Into<String>) -> Result<Vec<SqliteRow>> {
        self.query_parameterized(sql, Vec::new()).await
    }
    pub async fn query_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<Vec<SqliteRow>> {
        let sql = sql.into();
        self.validate(&sql, &params).await?;
        self.request(|response| TransactionCommand::Query {
            sql,
            params,
            response,
        })
        .await
    }
}

fn validate_sql(sql: &str, params: &[SqliteParam], limit: usize) -> Result<()> {
    anyhow::ensure!(
        params
            .iter()
            .all(|param| !matches!(param, SqliteParam::Real(value) if !value.is_finite())),
        "SQLite parameters cannot contain non-finite real values"
    );
    let bytes = params
        .iter()
        .try_fold(sql.len(), |size, param| {
            size.checked_add(match param {
                SqliteParam::Text(value) => value.len(),
                _ => 8,
            })
        })
        .context("SQLite SQL/parameter size overflow")?;
    anyhow::ensure!(
        bytes <= limit,
        "SQLite SQL and parameters exceed the configured byte limit"
    );
    anyhow::ensure!(!sql.contains('\0'), "SQLite SQL contains NUL");
    Ok(())
}

struct Scope {
    cleanup: Option<mpsc::OwnedPermit<TransactionCommand>>,
    active: Arc<Mutex<bool>>,
}
impl Scope {
    fn finish(&mut self, commit: bool) -> Result<()> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite scope lease poisoned"))?;
        *active = false;
        self.cleanup
            .take()
            .context("SQLite scope already ended")?
            .send(TransactionCommand::Finish { commit });
        Ok(())
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(permit) = self.cleanup.take() {
            match self.active.lock() {
                Ok(mut active) => *active = false,
                Err(poisoned) => {
                    *poisoned.into_inner() = false;
                    log::error!("SQLite scope lease poisoned; requesting rollback");
                }
            }
            permit.send(TransactionCommand::Finish { commit: false });
        }
    }
}
