// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::ComputationIndexProvider, interface::FailureMode, models::ElementValue,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_source_sqlite::native::{
    SqliteConfig, SqliteHandle, SqliteOutput, SqliteParam, SqliteReplayConfig, SqliteSource,
};
use std::num::NonZeroU64;
use tokio::sync::Mutex;

const INSTANCE: &str = "stockroom-persistent";

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive example bound")
}

fn codec() -> anyhow::Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(size(1024 * 1024));
    codec.register_schema(QueryChangeCodec::schema())?;
    Ok(codec)
}

struct SaveReorder;

#[async_trait]
impl TransactionalDeliveryHandler for SaveReorder {
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        state: &TransactionContext<'_>,
    ) -> anyhow::Result<()> {
        let record_key = |id: &RecordId| {
            let bytes = id
                .value()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("reorder/{}/{bytes}", id.namespace())
        };
        match item.operation {
            ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                let row = QueryChangeCodec::decode_row(after)?;
                let quantity = row
                    .values
                    .get("quantity")
                    .and_then(|value| value.as_i64())
                    .context("reorder requires integer quantity")?;
                state
                    .put(
                        &record_key(after.identity()),
                        ElementValue::Integer(quantity),
                    )
                    .await?;
            }
            ChangeOperation::Deleted { identity, .. } => {
                state.remove(&record_key(identity.identity())).await?;
            }
        }
        let count = match state.get("handled").await? {
            None => 0,
            Some(ElementValue::Integer(value)) => value,
            value => anyhow::bail!("invalid handled counter: {value:?}"),
        };
        state
            .put(
                "handled",
                ElementValue::Integer(count.checked_add(1).context("handled counter exhausted")?),
            )
            .await?;
        Ok(())
    }
}

struct DurableAlerts {
    descriptor: ComponentDescriptor,
    runner: Arc<Mutex<DeliveryRunner>>,
    contract: ComponentRecovery,
    hold: bool,
    completed: watch::Sender<u64>,
}

#[async_trait]
impl ComputationComponent for DurableAlerts {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.contract.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.runner.lock().await.shutdown().await
    }
}

#[async_trait]
impl EnvelopeSink for DurableAlerts {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if self.hold {
            println!(
                "Alert retained but not handled. Restart without --hold-alerts to complete it."
            );
            std::future::pending::<()>().await;
        }
        self.runner
            .lock()
            .await
            .deliver_transactional(&input, &SaveReorder)
            .await?;
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        println!("Saved reorder changes through query sequence {sequence}.");
        self.completed.send_replace(sequence);
        Ok(())
    }
}

pub(super) struct PersistentStockroom {
    core: DrasiLib,
    sql: SqliteHandle,
    results: QueryResults,
    journal: Arc<QosChannel>,
    completion: watch::Receiver<u64>,
}

impl PersistentStockroom {
    async fn open(directory: &Path, hold: bool) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(directory).await?;
        let provider: Arc<dyn ComputationIndexProvider> =
            Arc::new(RocksDbComputationProvider::new(
                directory.join("indexes"),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(128 * 1024 * 1024)?,
                ),
            ));
        let limits = SourceTransactionLimits {
            max_changes: size(128),
            max_bytes: size(1024 * 1024),
            max_duration_ms: NonZeroU64::new(5_000).expect("positive duration"),
        };
        let progress = Arc::new(QuerySourceProgress::new(
            GRAPH,
            ComponentId::try_new("low-stock")?,
        )?);
        let (source, bootstrap) = SqliteSource::coordinated(
            ComponentId::try_new("inventory")?,
            StreamId::try_new("inventory/out")?,
            SqliteConfig {
                path: Some(directory.join("inventory.sqlite")),
                tables: vec!["Product".into()],
                output: SqliteOutput::Transactions,
                replay: Some(SqliteReplayConfig {
                    max_transactions: size(128),
                    max_bytes: size(8 * 1024 * 1024),
                }),
                transactions: limits,
                max_sql_bytes: size(65536),
                command_capacity: size(8),
                output_capacity: size(8),
                shutdown_timeout_ms: NonZeroU64::new(5_000).expect("positive duration"),
            },
            progress.clone(),
        )?;
        let sql = source.handle();
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: GRAPH.into(),
                id: ComponentId::try_new("low-stock")?,
                query: LOW_STOCK.into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("low-stock/out")?,
                outbox_capacity: size(128),
            },
            provider.clone(),
            QueryOptions::default(),
            QueryExecutionSettings {
                source_transactions: Some(limits),
                ..Default::default()
            },
            None,
        )
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_source_progress(progress)?
        .with_bootstrap(bootstrap)
        .with_delivery_tracking();
        let results = query.results();
        let journal = QosChannel::persistent_with_recovery(
            QosChannelDefinition {
                stream: StreamId::try_new("low-stock/out")?,
                capacity: size(128),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: [("alerts".into(), SubscriptionStart::Earliest)].into(),
            },
            provider.create_indexes(GRAPH, "output-journal").await?,
            codec()?,
            "low-stock",
            QosRecoveryOptions::Replay(ReplayOptions {
                failure_scope: FailureMode::ProcessRestart,
                receipt_capacity: size(128),
            }),
        )
        .await?;
        let indexes = provider.create_indexes(GRAPH, "reorder-completion").await?;
        let contract = ComponentRecovery::transactional_consumer(&indexes)?;
        let runner = Arc::new(Mutex::new(DeliveryRunner::new_transactional(
            DeliveryScope::new(
                INSTANCE.into(),
                GRAPH.into(),
                ComponentId::try_new("alerts")?,
            )?,
            indexes,
            codec()?,
            DeliveryOptions {
                scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
                max_streams: size(1),
                receipts_per_stream: size(128),
                retry: DeliveryRetryPolicy::default(),
            },
        )?));
        let (completed, completion) = watch::channel(0);
        let handle = journal.resource();
        let journal_id = ResourceId::try_new("output-journal")?;
        let batch = ComponentBatch::builder()
            .source(Box::new(source))
            .query(Box::new(query))
            .sink(Box::new(DurableAlerts {
                descriptor: descriptor(
                    "alerts",
                    &[(
                        "in",
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                    )],
                )?,
                runner,
                contract,
                hold,
                completed,
            }))
            .declare_resource(ResourceSpecification {
                id: journal_id.clone(),
                role: handle.role(),
                ownership: ResourceOwnership::Graph,
                binding: "output-journal".into(),
            })?
            .provide_resource(journal_id.clone(), handle)?
            .bind_stream(
                endpoint("inventory", "out")?,
                StreamId::try_new("inventory/out")?,
            )
            .bind_stream(
                endpoint("low-stock", "out")?,
                StreamId::try_new("low-stock/out")?,
            )
            .connect(
                EdgeDefinition::new(endpoint("inventory", "out")?, endpoint("low-stock", "in")?),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("low-stock", "out")?, endpoint("alerts", "in")?),
                Box::new(journal.definition().pipe(journal_id, "alerts")),
            )
            .build()?;
        let core = DrasiLib::builder()
            .with_id(INSTANCE)
            .with_components(batch)
            .build()
            .await?;
        let setup = async {
            core.start().await?;
            for component in ["inventory", "low-stock", "alerts"] {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    core.computation_component(component)?.wait_started(),
                )
                .await??;
            }
            tokio::time::timeout(Duration::from_secs(10), results.wait_ready()).await??;
            sql.execute(
                "CREATE TABLE IF NOT EXISTS Product (
                sku TEXT PRIMARY KEY, quantity INTEGER NOT NULL,
                minimum INTEGER NOT NULL, warehouse TEXT NOT NULL
            )",
            )
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = setup {
            finish(&core, Err(error)).await?;
        }
        Ok(Self {
            core,
            sql,
            results,
            journal,
            completion,
        })
    }

    async fn apply_file(&self, path: &Path) -> anyhow::Result<()> {
        let desired = parse_inventory(&tokio::fs::read_to_string(path).await?)?;
        self.sql
            .transaction(|tx| async move {
                let mut current = BTreeMap::new();
                for row in tx
                    .query("SELECT sku, quantity, minimum, warehouse FROM Product")
                    .await?
                {
                    let product: Product = serde_json::from_value(serde_json::Value::Object(row))?;
                    current.insert(product.sku.clone(), product);
                }
                for (sku, product) in &desired {
                    ensure!(
                        product.payload.is_none(),
                        "persistent inventory expects plain products"
                    );
                    if current.get(sku) != Some(product) {
                        tx.execute_parameterized(
                            "INSERT INTO Product(sku,quantity,minimum,warehouse) VALUES(?,?,?,?)
                         ON CONFLICT(sku) DO UPDATE SET quantity=excluded.quantity,
                         minimum=excluded.minimum, warehouse=excluded.warehouse",
                            vec![
                                SqliteParam::Text(sku.clone()),
                                SqliteParam::Integer(product.quantity),
                                SqliteParam::Integer(product.minimum),
                                SqliteParam::Text(product.warehouse.clone()),
                            ],
                        )
                        .await?;
                    }
                }
                for sku in current.keys().filter(|sku| !desired.contains_key(*sku)) {
                    tx.execute_parameterized(
                        "DELETE FROM Product WHERE sku=?",
                        vec![SqliteParam::Text(sku.clone())],
                    )
                    .await?;
                }
                Ok(())
            })
            .await
    }
}

pub(super) async fn run(directory: &Path, hold: bool) -> anyhow::Result<()> {
    println!("Starting persistent Stockroom in {}.", directory.display());
    let mut app = PersistentStockroom::open(directory, hold).await?;
    let work = async {
        let rows = app
            .results
            .snapshot()?
            .rows
            .values()
            .map(|record| {
                QueryChangeCodec::decode_row(record)
                    .map(|row| QueryChangeCodec::row_values_to_json(&row.values))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        println!("Restored low-stock: {}", serde_json::to_string(&rows)?);
        let progress = app.journal.progress().await?;
        if progress.processed.get("alerts").copied() == Some(progress.accepted) {
            println!("No pending alerts in the output journal.");
        }
        println!("Ready. Watching inventory.json; save changes and press Ctrl-C to stop.");
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; break; }
                _ = interval.tick() => app.apply_file(Path::new("inventory.json")).await?,
                result = app.completion.changed() => {
                    result.context("persistent alert owner closed")?;
                    println!("low-stock: {}", serde_json::to_string(
                        &app.core.get_query_results("low-stock").await?)?);
                }
            }
        }
        Ok(())
    }
    .await;
    finish(&app.core, work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn held_alert_recovers_without_resubmission() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("inventory.json");
        tokio::fs::write(
            &input,
            r#"[{"sku":"bolts","quantity":3,"minimum":5,"warehouse":"west"}]"#,
        )
        .await?;
        let app = PersistentStockroom::open(directory.path(), true).await?;
        let work = async {
            app.apply_file(&input).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if app.journal.progress().await?.accepted > 0 {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
            ensure!(!app.results.snapshot()?.rows.is_empty());
            Ok(())
        }
        .await;
        finish(&app.core, work).await?;
        drop(app);

        let mut app = PersistentStockroom::open(directory.path(), false).await?;
        let work = async {
            tokio::time::timeout(
                Duration::from_secs(10),
                app.completion.wait_for(|sequence| *sequence > 0),
            )
            .await??;
            ensure!(
                app.core.get_query_results("low-stock").await?
                    == vec![serde_json::json!({"sku":"bolts","quantity":3})]
            );
            Ok(())
        }
        .await;
        finish(&app.core, work).await?;
        drop(app);

        let app = PersistentStockroom::open(directory.path(), false).await?;
        let work = async {
            let progress = app.journal.progress().await?;
            ensure!(progress.processed.get("alerts").copied() == Some(progress.accepted));
            ensure!(
                app.core.get_query_results("low-stock").await?
                    == vec![serde_json::json!({"sku":"bolts","quantity":3})]
            );
            Ok(())
        }
        .await;
        finish(&app.core, work).await
    }
}
