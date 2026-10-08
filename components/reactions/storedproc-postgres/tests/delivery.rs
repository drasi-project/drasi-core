// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider, evaluation::context::QueryPartEvaluationContext,
    interface::FailureMode,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    computation::v1::*,
    context::workers::{join_owned_worker, spawn_owned_worker, WorkerCompletion},
};
use drasi_reaction_storedproc_postgres::delivery::{
    PostgresDeliveryEffect, PostgresDeliveryHandler,
};
use std::{
    collections::BTreeMap,
    error::Error as _,
    num::NonZeroUsize,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use testcontainers::{core::ContainerPort, runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::{Notify, RwLock};
use tokio_postgres::{Client, NoTls, Transaction};

const IMAGE: &str =
    "16-alpine@sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea";

#[path = "delivery/http.rs"]
mod http;

type Driver =
    RwLock<Option<tokio::task::JoinHandle<std::result::Result<(), tokio_postgres::Error>>>>;

struct Database {
    container: ContainerAsync<Postgres>,
    drivers: Vec<Driver>,
}

impl Database {
    async fn new() -> Result<Self> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        Ok(Self {
            container: Postgres::default()
                .with_tag(IMAGE)
                .with_mapped_port(port, ContainerPort::Tcp(5432))
                .start()
                .await?,
            drivers: Vec::new(),
        })
    }

    async fn connect(&mut self) -> Result<Client> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(self.container.get_host().await?.to_string())
            .port(self.container.get_host_port_ipv4(5432).await?)
            .user("postgres")
            .password("postgres")
            .dbname("postgres");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let (client, connection) = loop {
            match tokio::time::timeout(Duration::from_secs(5), config.connect(NoTls)).await? {
                Ok(pair) => break pair,
                Err(error) => {
                    let restarting = error.code()
                        == Some(&tokio_postgres::error::SqlState::CANNOT_CONNECT_NOW)
                        || error
                            .source()
                            .and_then(|source| source.downcast_ref::<std::io::Error>())
                            .is_some_and(|error| {
                                matches!(
                                    error.kind(),
                                    std::io::ErrorKind::ConnectionRefused
                                        | std::io::ErrorKind::ConnectionReset
                                        | std::io::ErrorKind::UnexpectedEof
                                )
                            });
                    if !restarting || tokio::time::Instant::now() >= deadline {
                        return Err(error.into());
                    }
                    eprintln!("Waiting for owned PostgreSQL readiness: {error}");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        };
        let driver = RwLock::new(None);
        spawn_owned_worker(&driver, connection).await?;
        self.drivers.push(driver);
        Ok(client)
    }

    async fn join_drivers(&mut self, crashed: bool) -> Result<()> {
        for driver in &mut self.drivers {
            match join_owned_worker(driver.get_mut(), Duration::from_secs(5)).await? {
                WorkerCompletion::Completed(result) => {
                    if crashed {
                        anyhow::ensure!(
                            result.is_err(),
                            "killed database connection ended cleanly"
                        );
                    } else {
                        result?;
                    }
                }
                other => anyhow::bail!("database driver did not finish normally: {other:?}"),
            }
        }
        self.drivers.clear();
        Ok(())
    }

    async fn shutdown(mut self) -> Result<()> {
        self.join_drivers(false).await?;
        self.container.stop().await?;
        Ok(())
    }
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero")
}

async fn runner(path: &Path) -> Result<DeliveryRunner> {
    let indexes =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)))
            .create_indexes("delivery", "consumer")
            .await?;
    DeliveryRunner::new(
        DeliveryScope::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("sink")?,
        )?,
        indexes,
        codec()?,
        DeliveryOptions {
            scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
            max_streams: size(2),
            receipts_per_stream: size(2),
            retry: DeliveryRetryPolicy::default(),
        },
    )
}

fn codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(size(1 << 20));
    codec.register_schema(QueryChangeCodec::schema())?;
    Ok(codec)
}
fn input(sequence: u64, count: usize) -> Result<InputEnvelope> {
    let component = ComponentId::try_new("query")?;
    let changes = (0..count)
        .map(|index| QueryPartEvaluationContext::Adding {
            after: BTreeMap::new(),
            row_signature: index as u64,
        })
        .collect::<Vec<_>>();
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        &component,
        SystemMetadata::new(StreamId::try_new("out")?, sequence),
        &changes,
        QueryOutputMetadata {
            query_id: "query".into(),
            source_id: None,
            timestamp: chrono::DateTime::from_timestamp(1, 0).expect("timestamp"),
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .expect("nonempty batch");
    QueryRecoveryIdentity::try_new("graph", component.clone(), 42, None)?
        .annotate(&mut envelope, &component)?;
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope,
    })
}

#[derive(Default)]
struct Effect {
    fail_second: bool,
    hold: Option<(Arc<AtomicBool>, Arc<Notify>)>,
}

#[async_trait]
impl PostgresDeliveryEffect for Effect {
    async fn apply(&mut self, transaction: &Transaction<'_>, item: DeliveryItem<'_>) -> Result<()> {
        let isolation: String = transaction
            .query_one("SHOW transaction_isolation", &[])
            .await?
            .try_get(0)?;
        anyhow::ensure!(
            isolation == "read committed",
            "unexpected transaction isolation {isolation}"
        );
        let ordinal = i64::try_from(item.operation.ordinal())?;
        transaction
            .execute(
                "INSERT INTO effects(operation_id, ordinal) VALUES ($1, $2)",
                &[&item.id.as_str(), &ordinal],
            )
            .await?;
        if self.fail_second && ordinal == 1 {
            anyhow::bail!("failure after the actual SQL insert");
        }
        if let Some((hold, entered)) = &self.hold {
            if hold.load(Ordering::Acquire) {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
        }
        Ok(())
    }
}

async fn effects(client: &Client) -> Result<i64> {
    Ok(client
        .query_one("SELECT count(*) FROM effects", &[])
        .await?
        .try_get(0)?)
}

async fn setup(client: &Client) -> Result<()> {
    client
        .batch_execute("CREATE TABLE effects(operation_id TEXT NOT NULL, ordinal BIGINT NOT NULL)")
        .await?;
    Ok(())
}

struct LostResponse {
    inner: PostgresDeliveryHandler<Effect>,
    lose: bool,
}

#[async_trait]
impl DeliveryHandler for LostResponse {
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        self.inner.handle(item).await?;
        if self.lose {
            self.lose = false;
            anyhow::bail!("lost destination commit response");
        }
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn committed_effect_is_deduplicated_after_lost_response_and_local_reconstruction(
) -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut handler = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    handler.initialize().await?;
    let mut handler = LostResponse {
        inner: handler,
        lose: true,
    };
    let directory = tempfile::tempdir()?;
    let batch = input(1, 2)?;
    let mut delivery = runner(directory.path()).await?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    delivery.shutdown().await?;
    drop(delivery);
    let mut delivery = runner(directory.path()).await?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 2);
    assert_eq!(delivery.progress().await?[0].handled_sequence, 1);
    let rows: i64 = observer
        .query_one("SELECT count(*) FROM public.drasi_delivery_stream_v1", &[])
        .await?
        .try_get(0)?;
    assert_eq!(rows, 1);
    delivery.shutdown().await?;
    drop(handler);
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn failed_and_cancelled_effects_roll_back_with_their_destination_cursor() -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut handler = PostgresDeliveryHandler::new(
        database.connect().await?,
        Effect {
            fail_second: true,
            ..Default::default()
        },
    );
    handler.initialize().await?;
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await?;
    let batch = input(1, 2)?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    let completed: i64 = observer
        .query_one("SELECT completed FROM public.drasi_delivery_stream_v1", &[])
        .await?
        .try_get(0)?;
    assert_eq!(completed, 1);
    let (client, _) = handler.into_parts();
    let hold = Arc::new(AtomicBool::new(true));
    let entered = Arc::new(Notify::new());
    let mut handler = PostgresDeliveryHandler::new(
        client,
        Effect {
            hold: Some((hold.clone(), entered.clone())),
            ..Default::default()
        },
    );
    handler.initialize().await?;
    {
        let pending = delivery.deliver(&batch, &mut handler);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("expected held SQL effect: {result:?}"),
            _ = entered.notified() => {}
        }
    }
    assert_eq!(effects(&observer).await?, 1);
    hold.store(false, Ordering::Release);
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 2);
    delivery.shutdown().await?;
    drop(handler);
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_retries_commit_one_effect_per_operation() -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut first = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    let mut second = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    let (a, b) = tokio::join!(first.initialize(), second.initialize());
    a?;
    b?;
    let first_path = tempfile::tempdir()?;
    let second_path = tempfile::tempdir()?;
    let mut first_delivery = runner(first_path.path()).await?;
    let mut second_delivery = runner(second_path.path()).await?;
    let batch = input(1, 3)?;
    let (a, b) = tokio::join!(
        first_delivery.deliver(&batch, &mut first),
        second_delivery.deliver(&batch, &mut second)
    );
    a?;
    b?;
    assert_eq!(effects(&observer).await?, 3);
    assert_eq!(first_delivery.progress().await?[0].completed_operations, 3);
    assert_eq!(second_delivery.progress().await?[0].completed_operations, 3);
    first_delivery.shutdown().await?;
    second_delivery.shutdown().await?;
    drop(first);
    drop(second);
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn full_width_sequences_and_bounded_destination_history_reject_old_or_changed_input(
) -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut handler = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    handler.initialize().await?;
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await?;
    delivery.deliver(&input(u64::MAX, 1)?, &mut handler).await?;
    delivery.shutdown().await?;
    let saved: String = observer
        .query_one("SELECT sequence FROM public.drasi_delivery_stream_v1", &[])
        .await?
        .try_get(0)?;
    assert_eq!(saved, u64::MAX.to_string());
    for batch in [input(u64::MAX - 1, 1)?, input(u64::MAX, 2)?] {
        let path = tempfile::tempdir()?;
        let mut delivery = runner(path.path()).await?;
        assert!(delivery.deliver(&batch, &mut handler).await.is_err());
        delivery.shutdown().await?;
    }
    assert_eq!(effects(&observer).await?, 1);
    drop(handler);
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn exact_stream_capacity_rejects_before_effects_and_keeps_existing_streams_usable(
) -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut handler = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    handler.initialize().await?;
    observer
        .batch_execute(
            "INSERT INTO public.drasi_delivery_stream_v1
         SELECT 'fixture-' || i, 1, '1', decode(repeat('00', 32), 'hex'), 1, 1
         FROM generate_series(1,4096) i",
        )
        .await?;
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await?;
    assert!(delivery.deliver(&input(1, 1)?, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 0);
    observer
        .execute(
            "DELETE FROM public.drasi_delivery_stream_v1 WHERE stream_key='fixture-1'",
            &[],
        )
        .await?;
    delivery.deliver(&input(1, 1)?, &mut handler).await?;
    delivery.deliver(&input(2, 1)?, &mut handler).await?;
    assert_eq!(effects(&observer).await?, 2);
    let count: i64 = observer
        .query_one("SELECT count(*) FROM public.drasi_delivery_stream_v1", &[])
        .await?
        .try_get(0)?;
    assert_eq!(count, 4096);
    delivery.shutdown().await?;
    drop(handler);
    drop(observer);
    database.shutdown().await
}

struct Gap {
    inner: PostgresDeliveryHandler<Effect>,
}

#[async_trait]
impl DeliveryHandler for Gap {
    async fn handle(&mut self, mut item: DeliveryItem<'_>) -> Result<()> {
        let mut value = serde_json::to_value(&item.position)?;
        value["operation"] = serde_json::json!(1);
        item.position = serde_json::from_value(value)?;
        self.inner.handle(item).await
    }
}

#[tokio::test(flavor = "current_thread")]
async fn gaps_unfinished_batches_and_corrupt_cursors_never_become_new_effects() -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let mut handler = PostgresDeliveryHandler::new(
        database.connect().await?,
        Effect {
            fail_second: true,
            ..Default::default()
        },
    );
    handler.initialize().await?;
    let mut gap = Gap { inner: handler };
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await?;
    assert!(delivery.deliver(&input(1, 2)?, &mut gap).await.is_err());
    assert_eq!(effects(&observer).await?, 0);
    let mut handler = gap.inner;
    assert!(delivery.deliver(&input(1, 2)?, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    delivery.shutdown().await?;
    let next = tempfile::tempdir()?;
    let mut delivery = runner(next.path()).await?;
    assert!(delivery.deliver(&input(2, 1)?, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    delivery.shutdown().await?;
    observer
        .execute(
            "UPDATE public.drasi_delivery_stream_v1 SET sequence='18446744073709551616'",
            &[],
        )
        .await?;
    let next = tempfile::tempdir()?;
    let mut delivery = runner(next.path()).await?;
    assert!(delivery.deliver(&input(1, 2)?, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    delivery.shutdown().await?;
    drop(handler);
    drop(observer);
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn database_sigkill_retains_committed_effects_and_deduplicates_unconfirmed_delivery(
) -> Result<()> {
    let mut database = Database::new().await?;
    let observer = database.connect().await?;
    setup(&observer).await?;
    let client = database.connect().await?;
    client
        .batch_execute(
            "SET default_transaction_isolation = 'repeatable read'; SET synchronous_commit = off",
        )
        .await?;
    let mut handler = PostgresDeliveryHandler::new(client, Effect::default());
    handler.initialize().await?;
    let mut handler = LostResponse {
        inner: handler,
        lose: true,
    };
    let directory = tempfile::tempdir()?;
    let batch = input(1, 2)?;
    let mut delivery = runner(directory.path()).await?;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(effects(&observer).await?, 1);
    let killed = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("docker")
            .args(["kill", "--signal=KILL", database.container.id()])
            .output(),
    )
    .await??;
    anyhow::ensure!(
        killed.status.success(),
        "owned database kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    database.join_drivers(true).await?;
    drop(handler);
    drop(observer);
    delivery.shutdown().await?;
    drop(delivery);
    tokio::time::timeout(Duration::from_secs(30), database.container.start()).await??;
    let observer = database.connect().await?;
    assert_eq!(
        effects(&observer).await?,
        1,
        "committed effect must survive the actual database crash"
    );
    let mut handler = PostgresDeliveryHandler::new(database.connect().await?, Effect::default());
    handler.initialize().await?;
    let mut delivery = runner(directory.path()).await?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(
        effects(&observer).await?,
        2,
        "unconfirmed first operation must not repeat"
    );
    delivery.shutdown().await?;
    drop(handler);
    drop(observer);
    database.shutdown().await
}
