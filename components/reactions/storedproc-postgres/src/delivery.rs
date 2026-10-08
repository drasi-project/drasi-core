// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opt-in transactional destination for the ComputationGraph delivery runner.
//! This does not change the legacy reaction's queue or failure policy.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::{DeliveryHandler, DeliveryItem};
use tokio_postgres::{Client, IsolationLevel, Row, Transaction};

const MAX_STREAMS: i64 = 4096;
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS public.drasi_delivery_stream_v1 (
        stream_key TEXT PRIMARY KEY,
        version SMALLINT NOT NULL CHECK (version = 1),
        sequence TEXT NOT NULL CHECK (sequence ~ '^[1-9][0-9]{0,19}$'),
        digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
        operations BIGINT NOT NULL CHECK (operations > 0),
        completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations)
    )";
const READ: &str = "
    SELECT version, sequence, digest, operations, completed
    FROM public.drasi_delivery_stream_v1 WHERE stream_key = $1 FOR UPDATE";

#[derive(Debug, thiserror::Error)]
pub enum PostgresDeliveryError {
    #[error("PostgreSQL delivery commit outcome is unknown: {0}")]
    CommitUnknown(#[source] tokio_postgres::Error),
    #[error("PostgreSQL delivery failed: {failure}; rollback also failed: {rollback}")]
    Rollback {
        #[source]
        failure: anyhow::Error,
        rollback: tokio_postgres::Error,
    },
}

/// Apply only effects participating in this borrowed PostgreSQL transaction.
/// Do not issue transaction-control SQL, use another connection, or perform
/// external effects. The adapter alone owns commit and rollback.
#[async_trait]
pub trait PostgresDeliveryEffect: Send + Sync {
    async fn apply(&mut self, transaction: &Transaction<'_>, item: DeliveryItem<'_>) -> Result<()>;
}

/// One PostgreSQL transaction commits an operation's effect and destination
/// cursor together. A row per stable producer/consumer stream replaces an
/// ever-growing per-operation receipt table. Old batches reject after rollover.
///
/// The caller owns the connection driver: no task is spawned here. Initialize
/// during component startup, not configuration-only construction. Before joining
/// the driver, drop this handler or recover its client with `into_parts`.
pub struct PostgresDeliveryHandler<E> {
    client: Client,
    effect: E,
    initialized: bool,
}

impl<E: PostgresDeliveryEffect> PostgresDeliveryHandler<E> {
    pub fn new(client: Client, effect: E) -> Self {
        Self {
            client,
            effect,
            initialized: false,
        }
    }

    pub async fn initialize(&mut self) -> Result<()> {
        self.initialized = false;
        let transaction = self
            .client
            .build_transaction()
            .isolation_level(IsolationLevel::ReadCommitted)
            .start()
            .await?;
        transaction
            .query_one("SELECT pg_advisory_xact_lock(1146241363, 1)", &[])
            .await?;
        transaction
            .batch_execute(SCHEMA)
            .await
            .context("creating delivery progress table")?;
        transaction
            .batch_execute("SET LOCAL synchronous_commit = on")
            .await?;
        transaction.commit().await?;
        self.initialized = true;
        Ok(())
    }

    pub fn into_parts(self) -> (Client, E) {
        (self.client, self.effect)
    }

    async fn apply(
        transaction: &Transaction<'_>,
        effect: &mut E,
        item: DeliveryItem<'_>,
    ) -> Result<()> {
        item.position.validate()?;
        let stream = item.position.stream().to_owned();
        let sequence = item.position.sequence();
        let operations = i64::try_from(item.position.operations())?;
        let operation = i64::try_from(item.position.operation())?;
        let digest = item.content_digest.as_slice();
        let mut row = transaction.query_opt(READ, &[&stream]).await?;
        if row.is_none() {
            // Serialize only first-stream admission; existing streams use their
            // own row locks. The lock is released with this actual transaction.
            transaction
                .query_one("SELECT pg_advisory_xact_lock(1146241363, 1)", &[])
                .await?;
            row = transaction.query_opt(READ, &[&stream]).await?;
        }
        let completed = match row {
            Some(row) => {
                let previous = Cursor::read(&row)?;
                anyhow::ensure!(
                    sequence >= previous.sequence,
                    "destination delivery receipt expired"
                );
                if sequence == previous.sequence {
                    anyhow::ensure!(
                        previous.digest.as_slice() == digest && previous.operations == operations,
                        "destination delivery payload conflicts with its receipt"
                    );
                    if operation < previous.completed {
                        return Ok(());
                    }
                    previous.completed
                } else {
                    anyhow::ensure!(
                        previous.completed == previous.operations && operation == 0,
                        "destination has unfinished operations or a missing batch prefix"
                    );
                    transaction
                        .execute(
                            "UPDATE public.drasi_delivery_stream_v1
                         SET sequence = $2, digest = $3, operations = $4, completed = 0
                         WHERE stream_key = $1",
                            &[&stream, &sequence.to_string(), &digest, &operations],
                        )
                        .await?;
                    0
                }
            }
            None => {
                anyhow::ensure!(
                    operation == 0,
                    "destination delivery is missing its first operation"
                );
                let count: i64 = transaction
                    .query_one("SELECT count(*) FROM public.drasi_delivery_stream_v1", &[])
                    .await?
                    .try_get(0)?;
                anyhow::ensure!(
                    count < MAX_STREAMS,
                    "destination delivery stream capacity is exhausted"
                );
                transaction
                    .execute(
                        "INSERT INTO public.drasi_delivery_stream_v1
                     (stream_key, version, sequence, digest, operations, completed)
                     VALUES ($1, 1, $2, $3, $4, 0)",
                        &[&stream, &sequence.to_string(), &digest, &operations],
                    )
                    .await?;
                0
            }
        };
        anyhow::ensure!(
            operation == completed,
            "destination delivery has an operation gap"
        );
        effect.apply(transaction, item).await?;
        let changed = transaction
            .execute(
                "UPDATE public.drasi_delivery_stream_v1 SET completed = completed + 1
             WHERE stream_key = $1 AND completed = $2",
                &[&stream, &completed],
            )
            .await?;
        anyhow::ensure!(
            changed == 1,
            "destination delivery cursor changed during its effect"
        );
        Ok(())
    }
}

struct Cursor {
    sequence: u64,
    digest: Vec<u8>,
    operations: i64,
    completed: i64,
}

impl Cursor {
    fn read(row: &Row) -> Result<Self> {
        let version: i16 = row.try_get("version")?;
        let encoded: String = row.try_get("sequence")?;
        let sequence = encoded
            .parse::<u64>()
            .context("invalid destination delivery sequence")?;
        let cursor = Self {
            sequence,
            digest: row.try_get("digest")?,
            operations: row.try_get("operations")?,
            completed: row.try_get("completed")?,
        };
        anyhow::ensure!(
            version == 1
                && sequence > 0
                && sequence.to_string() == encoded
                && cursor.digest.len() == 32
                && cursor.operations > 0
                && cursor.completed >= 0
                && cursor.completed <= cursor.operations,
            "invalid destination delivery progress"
        );
        Ok(cursor)
    }
}

#[async_trait]
impl<E: PostgresDeliveryEffect> DeliveryHandler for PostgresDeliveryHandler<E> {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            .is_some_and(|code| matches!(code.code(), "40001" | "40P01" | "55P03"))
    }

    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        anyhow::ensure!(
            self.initialized,
            "PostgreSQL delivery handler has not been initialized"
        );
        item.position.validate()?;
        let transaction = self
            .client
            .build_transaction()
            .isolation_level(IsolationLevel::ReadCommitted)
            .start()
            .await?;
        let result = async {
            Self::apply(&transaction, &mut self.effect, item).await?;
            transaction
                .batch_execute("SET LOCAL synchronous_commit = on")
                .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => transaction
                .commit()
                .await
                .map_err(|error| PostgresDeliveryError::CommitUnknown(error).into()),
            Err(failure) => match transaction.rollback().await {
                Ok(()) => Err(failure),
                Err(rollback) => Err(PostgresDeliveryError::Rollback { failure, rollback }.into()),
            },
        }
    }
}
