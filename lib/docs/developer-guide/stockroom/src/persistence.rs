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

pub(super) const INSTANCE: &str = "stockroom-persistent";

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive example bound")
}

fn codec() -> anyhow::Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(size(1024 * 1024));
    codec.register_schema(QueryChangeCodec::schema())?;
    Ok(codec)
}

struct SaveReorder;

// ANCHOR: reorder-key
fn reorder_key(id: &RecordId) -> String {
    let bytes = id
        .value()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("reorder/{}/{bytes}", id.namespace())
}
// ANCHOR_END: reorder-key

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SavedReorder {
    sku: String,
    quantity: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedReorders {
    version: u32,
    handled: i64,
    rows: BTreeMap<String, SavedReorder>,
}

fn decode_reorders(
    value: Option<ElementValue>,
    handled: Option<ElementValue>,
) -> anyhow::Result<Option<BTreeMap<String, SavedReorder>>> {
    let count = match handled {
        None => 0,
        Some(ElementValue::Integer(count)) if count >= 0 => count,
        value => anyhow::bail!("invalid handled counter: {value:?}"),
    };
    match value {
        Some(ElementValue::String(value)) => {
            let saved: SavedReorders = serde_json::from_str(&value)?;
            ensure!(saved.version == 1, "unsupported saved reorder view version");
            Ok((saved.handled == count).then_some(saved.rows))
        }
        None if count == 0 => Ok(Some(BTreeMap::new())),
        None => Ok(None),
        Some(_) => anyhow::bail!("saved reorder view is not a string"),
    }
}

async fn saved_reorders(runner: &mut DeliveryRunner) -> anyhow::Result<Option<Vec<SavedReorder>>> {
    let value = runner.read_state("reorders").await?;
    let handled = runner.read_state("handled").await?;
    Ok(decode_reorders(value, handled)?.map(|rows| rows.into_values().collect()))
}

async fn print_saved_reorders(runner: &mut DeliveryRunner) -> anyhow::Result<()> {
    match saved_reorders(runner).await? {
        Some(rows) => println!("Saved reorders: {}", serde_json::to_string(&rows)?),
        None => println!("Saved reorders unavailable: enumeration index missing or stale; existing per-record state is preserved."),
    }
    Ok(())
}

// ANCHOR: save-reorder
#[async_trait]
impl TransactionalDeliveryHandler for SaveReorder {
    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        state: &TransactionContext<'_>,
    ) -> anyhow::Result<()> {
        let mut reorders =
            decode_reorders(state.get("reorders").await?, state.get("handled").await?)?;
        match item.operation {
            ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                let row = QueryChangeCodec::decode_row(after)?;
                let quantity = row
                    .values
                    .get("quantity")
                    .and_then(|value| value.as_i64())
                    .context("reorder requires integer quantity")?;
                let sku = row
                    .values
                    .get("sku")
                    .and_then(|value| value.as_str())
                    .context("reorder requires sku")?
                    .to_owned();
                let key = reorder_key(after.identity());
                state.put(&key, ElementValue::Integer(quantity)).await?;
                if let Some(reorders) = &mut reorders {
                    reorders.insert(key, SavedReorder { sku, quantity });
                }
            }
            ChangeOperation::Deleted { identity, .. } => {
                let key = reorder_key(identity.identity());
                state.remove(&key).await?;
                if let Some(reorders) = &mut reorders {
                    reorders.remove(&key);
                }
            }
        }
        let count = match state.get("handled").await? {
            None => 0,
            Some(ElementValue::Integer(value)) => value,
            value => anyhow::bail!("invalid handled counter: {value:?}"),
        };
        let handled = count.checked_add(1).context("handled counter exhausted")?;
        if let Some(rows) = reorders {
            state
                .put(
                    "reorders",
                    ElementValue::String(
                        serde_json::to_string(&SavedReorders {
                            version: 1,
                            handled,
                            rows,
                        })?
                        .into(),
                    ),
                )
                .await?;
        }
        state.put("handled", ElementValue::Integer(handled)).await?;
        Ok(())
    }
}
// ANCHOR_END: save-reorder

struct DurableAlerts {
    descriptor: ComponentDescriptor,
    runner: Arc<Mutex<DeliveryRunner>>,
    contract: ComponentRecovery,
    hold: bool,
    completed: watch::Sender<u64>,
    delivery_ready: watch::Receiver<bool>,
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
        saved_reorders(&mut *self.runner.lock().await).await?;
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
        self.delivery_ready.wait_for(|ready| *ready).await?;
        if self.hold {
            println!(
                "Alert retained but not handled. Restart without --hold-alerts to complete it."
            );
            std::future::pending::<()>().await;
        }
        let mut runner = self.runner.lock().await;
        // ANCHOR: durable-handle
        runner.deliver_transactional(&input, &SaveReorder).await?;
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        println!("Saved reorder changes through query sequence {sequence}.");
        print_saved_reorders(&mut runner).await?;
        self.completed.send_replace(sequence);
        // ANCHOR_END: durable-handle
        Ok(())
    }
}

pub(super) struct PersistentStockroom {
    core: DrasiLib,
    sql: SqliteHandle,
    hold: bool,
    #[cfg(test)]
    results: QueryResults,
    #[cfg(test)]
    journal: Arc<QosChannel>,
    completion: watch::Receiver<u64>,
    health: cli::Health,
    #[cfg(test)]
    runner: Arc<Mutex<DeliveryRunner>>,
    last_file: Option<BTreeMap<String, Product>>,
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
                table_keys: Vec::new(),
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
        // ANCHOR: persistent-query
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
        // ANCHOR_END: persistent-query
        let results = query.results();
        // ANCHOR: output-journal
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
        // ANCHOR_END: output-journal
        // ANCHOR: delivery-runner
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
        // ANCHOR_END: delivery-runner
        let (completed, completion) = watch::channel(0);
        let (allow_delivery, delivery_ready) = watch::channel(false);
        let progress_at_open = journal.progress().await?;
        let handled_at_open = progress_at_open
            .processed
            .get("alerts")
            .copied()
            .context("output journal omitted its alerts cursor")?;
        let pending_at_open = progress_at_open
            .accepted
            .checked_sub(handled_at_open)
            .context("output journal cursor is ahead of its head")?;
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
                runner: runner.clone(),
                contract,
                hold,
                completed,
                delivery_ready,
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
            let mut health = cli::Health::new(&core)?;
            core.start().await?;
            for component in ["inventory", "low-stock", "alerts"] {
                cli::wait_started(&core, component, Duration::from_secs(10)).await?;
            }
            tokio::time::timeout(Duration::from_secs(10), results.wait_ready())
                .await
                .context("wait for query 'low-stock' readiness")??;
            sql.execute(
                "CREATE TABLE IF NOT EXISTS Product (
                sku TEXT PRIMARY KEY, quantity INTEGER NOT NULL,
                minimum INTEGER NOT NULL, warehouse TEXT NOT NULL
            )",
            )
            .await?;
            let rows = results
                .snapshot()?
                .rows
                .values()
                .map(|record| {
                    QueryChangeCodec::decode_row(record)
                        .map(|row| QueryChangeCodec::row_values_to_json(&row.values))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            println!("Restored low-stock: {}", serde_json::to_string(&rows)?);
            print_saved_reorders(&mut *runner.lock().await).await?;
            if pending_at_open == 0 {
                println!("No pending alerts in the output journal at open.");
            } else {
                println!("Pending alerts in the output journal at open: {pending_at_open}.");
            }
            allow_delivery.send_replace(true);
            health.check()?;
            Ok(health)
        }
        .await;
        let health = match setup {
            Ok(health) => health,
            Err(error) => return finish(&core, Err(error)).await,
        };
        Ok(Self {
            core,
            sql,
            hold,
            #[cfg(test)]
            results,
            #[cfg(test)]
            journal,
            completion,
            health,
            #[cfg(test)]
            runner,
            last_file: None,
        })
    }

    async fn finish<T>(&self, work: anyhow::Result<T>) -> anyhow::Result<T> {
        if !self.hold {
            return finish(&self.core, work).await;
        }
        let interruption = async {
            let control = self.core.computation_control()?;
            let revision = control.desired_snapshot().revision;
            let stopped = control
                .stop_components(
                    revision,
                    GraphSelection::Exact(vec![
                        ComponentId::try_new("inventory")?,
                        ComponentId::try_new("low-stock")?,
                    ]),
                )
                .await?;
            if stopped.summary != OperationSummary::Completed {
                return Err(LifecycleReportError::Stop(stopped).into());
            }
            // Drain actual input owners before interrupting the deliberately held sink.
            let forced = control
                .force_cancel_components(revision, GraphSelection::All)
                .await?;
            if forced.cleanup.summary != OperationSummary::Completed {
                return Err(LifecycleReportError::Stop(forced.cleanup).into());
            }
            println!("Held alert handling interrupted; pending output remains saved for restart.");
            Ok(())
        };
        let interruption = tokio::time::timeout(Duration::from_secs(10), interruption)
            .await
            .context("timed out preparing the held-alert restart")
            .and_then(|result| result);
        let work = match (work, interruption) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
                "Stockroom work and held-alert interruption failed",
                vec![error, cleanup],
            )
            .into()),
        };
        finish(&self.core, work).await
    }

    async fn apply_file(&mut self, path: &Path) -> anyhow::Result<bool> {
        let desired = parse_inventory(&tokio::fs::read_to_string(path).await?)?;
        if self.last_file.as_ref() == Some(&desired) {
            return Ok(false);
        }
        let mut current = BTreeMap::new();
        for row in self
            .sql
            .query("SELECT sku, quantity, minimum, warehouse FROM Product")
            .await?
        {
            let product: Product = serde_json::from_value(serde_json::Value::Object(row))?;
            current.insert(product.sku.clone(), product);
        }
        if current == desired {
            self.last_file = Some(desired);
            return Ok(false);
        }
        let desired_for_transaction = desired.clone();
        self.sql
            .transaction(|tx| async move {
                for (sku, product) in &desired_for_transaction {
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
                for sku in current
                    .keys()
                    .filter(|sku| !desired_for_transaction.contains_key(*sku))
                {
                    tx.execute_parameterized(
                        "DELETE FROM Product WHERE sku=?",
                        vec![SqliteParam::Text(sku.clone())],
                    )
                    .await?;
                }
                Ok(())
            })
            .await?;
        self.last_file = Some(desired);
        Ok(true)
    }
}

pub(super) async fn run(
    directory: &Path,
    hold: bool,
    shutdown: &mut cli::Shutdown,
) -> anyhow::Result<()> {
    println!("Starting persistent Stockroom in {}.", directory.display());
    let mut app = PersistentStockroom::open(directory, hold).await?;
    let work = async {
        println!("Ready. Watching inventory.json; save changes and press Ctrl-C to stop.");
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            tokio::select! {
                biased;
                result = app.health.failed() => { result?; }
                result = shutdown.requested() => { result?; break; }
                _ = interval.tick() => { app.apply_file(Path::new("inventory.json")).await?; },
                result = app.completion.changed() => {
                    result.context("persistent alert owner closed")?;
                    println!("low-stock: {}", serde_json::to_string(
                        &app.core.get_query_results("low-stock").await?)?);
                }
            }
        }
        app.health.check()?;
        Ok(())
    }
    .await;
    let work = match work {
        Ok(()) => {
            async {
                let control = app.core.computation_control()?;
                let mut members =
                    vec![ComponentId::try_new("inventory")?, ComponentId::try_new("low-stock")?];
                if !hold {
                    members.push(ComponentId::try_new("alerts")?);
                }
                tokio::time::timeout(
                    Duration::from_secs(10),
                    control.quiesce_components(
                        control.desired_snapshot().revision,
                        GraphSelection::Exact(members),
                    ),
                )
                .await??;
                Ok(())
            }
            .await
        }
        Err(error) => Err(error),
    };
    app.finish(work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PreviousHandler;

    #[async_trait]
    impl TransactionalDeliveryHandler for PreviousHandler {
        async fn handle(
            &self,
            item: DeliveryItem<'_>,
            state: &TransactionContext<'_>,
        ) -> anyhow::Result<()> {
            let ChangeOperation::Added { after, .. } = item.operation else {
                anyhow::bail!("legacy fixture expects an addition");
            };
            state
                .put(&reorder_key(after.identity()), ElementValue::Integer(3))
                .await?;
            state.put("handled", ElementValue::Integer(1)).await?;
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn previous_per_record_state_keeps_processing_without_inventing_a_view(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let input_path = directory.path().join("inventory.json");
        tokio::fs::write(
            &input_path,
            r#"[{"sku":"bolts","quantity":3,"minimum":5,"warehouse":"west"}]"#,
        )
        .await?;
        let mut app = PersistentStockroom::open(directory.path(), true).await?;
        let work = async {
            app.apply_file(&input_path).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while app.journal.progress().await?.accepted == 0 {
                    tokio::task::yield_now().await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let envelope = app
                .results
                .replay(0)?
                .pop()
                .context("missing saved query output")?;
            let mut runner = app.runner.lock().await;
            runner
                .deliver_transactional(
                    &InputEnvelope {
                        port: PortId::try_new("in")?,
                        envelope,
                    },
                    &PreviousHandler,
                )
                .await?;
            ensure!(saved_reorders(&mut runner).await?.is_none());
            Ok(())
        }
        .await;
        app.finish(work).await?;
        drop(app);

        let mut app = PersistentStockroom::open(directory.path(), false).await?;
        let work = async {
            tokio::time::timeout(
                Duration::from_secs(10),
                app.completion.wait_for(|sequence| *sequence >= 1),
            )
            .await??;
            let record = app
                .results
                .snapshot()?
                .rows
                .values()
                .next()
                .context("restored query has no product")?
                .identity()
                .clone();
            ensure!(
                app.runner
                    .lock()
                    .await
                    .read_state(&reorder_key(&record))
                    .await?
                    == Some(ElementValue::Integer(3))
            );
            tokio::fs::write(
                &input_path,
                r#"[{"sku":"bolts","quantity":8,"minimum":5,"warehouse":"west"}]"#,
            )
            .await?;
            app.apply_file(&input_path).await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                app.completion.wait_for(|sequence| *sequence >= 2),
            )
            .await??;
            let mut runner = app.runner.lock().await;
            ensure!(runner.read_state(&reorder_key(&record)).await?.is_none());
            ensure!(saved_reorders(&mut runner).await?.is_none());
            Ok(())
        }
        .await;
        app.finish(work).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn held_alert_recovers_without_resubmission() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("inventory.json");
        tokio::fs::write(
            &input,
            r#"[{"sku":"bolts","quantity":3,"minimum":5,"warehouse":"west"}]"#,
        )
        .await?;
        let mut app = PersistentStockroom::open(directory.path(), true).await?;
        let work = async {
            ensure!(app.apply_file(&input).await?);
            ensure!(
                !app.apply_file(&input).await?,
                "unchanged files must not start another write"
            );
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
            ensure!(saved_reorders(&mut *app.runner.lock().await).await? == Some(Vec::new()));
            Ok(())
        }
        .await;
        app.finish(work).await?;
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
            ensure!(
                saved_reorders(&mut *app.runner.lock().await).await?
                    == Some(vec![SavedReorder {
                        sku: "bolts".into(),
                        quantity: 3
                    }])
            );
            Ok(())
        }
        .await;
        app.finish(work).await?;
        drop(app);

        let mut app = PersistentStockroom::open(directory.path(), false).await?;
        let work = async {
            let progress = app.journal.progress().await?;
            ensure!(progress.processed.get("alerts").copied() == Some(progress.accepted));
            ensure!(
                app.core.get_query_results("low-stock").await?
                    == vec![serde_json::json!({"sku":"bolts","quantity":3})]
            );
            ensure!(
                saved_reorders(&mut *app.runner.lock().await).await?
                    == Some(vec![SavedReorder {
                        sku: "bolts".into(),
                        quantity: 3
                    }])
            );
            tokio::fs::write(
                &input,
                r#"[{"sku":"bolts","quantity":8,"minimum":5,"warehouse":"west"}]"#,
            )
            .await?;
            ensure!(app.apply_file(&input).await?);
            tokio::time::timeout(
                Duration::from_secs(10),
                app.completion.wait_for(|sequence| *sequence >= 2),
            )
            .await??;
            ensure!(saved_reorders(&mut *app.runner.lock().await).await? == Some(Vec::new()));
            Ok(())
        }
        .await;
        app.finish(work).await
    }

    #[test]
    fn legacy_or_corrupt_reorder_state_is_not_reported_as_empty() {
        assert!(decode_reorders(None, Some(ElementValue::Integer(1)))
            .unwrap()
            .is_none());
        assert!(decode_reorders(Some(ElementValue::Integer(3)), None).is_err());
        assert!(decode_reorders(Some(ElementValue::String("bad json".into())), None).is_err());
    }
}
