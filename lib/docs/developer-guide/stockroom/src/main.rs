// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Stockroom: save changes to inventory.json to update a continuous query.
//! Run `cargo run --locked`, optionally adding `-- totals` or `-- audit`.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context};
use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::watch};

const GRAPH: &str = "__drasi_lib_runtime__";
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOW_STOCK: &str = "
MATCH (p:Product)
WHERE p.quantity < p.minimum
RETURN p.sku AS sku, p.quantity AS quantity";
const WAREHOUSE_TOTALS: &str =
    "MATCH (p:Product) RETURN p.warehouse AS warehouse, sum(p.quantity) AS total";

#[derive(Clone, Copy)]
enum Scenario {
    LowStock,
    Totals,
    Audit,
}

impl Scenario {
    fn query(self) -> (&'static str, &'static str) {
        match self {
            Self::Totals => ("stock-by-warehouse", WAREHOUSE_TOTALS),
            Self::LowStock | Self::Audit => ("low-stock", LOW_STOCK),
        }
    }
}

fn endpoint(component: &str, port: &str) -> anyhow::Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
}

fn descriptor(
    id: &str,
    ports: &[(&str, PortDirection, SchemaDescriptor)],
) -> anyhow::Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        ports
            .iter()
            .map(|(name, direction, schema)| {
                Ok(PortDescriptor::new(
                    PortId::try_new(*name)?,
                    *direction,
                    schema.clone(),
                    PipeRequirements::default(),
                ))
            })
            .collect::<anyhow::Result<_>>()?,
    )?)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Product {
    sku: String,
    quantity: i64,
    minimum: i64,
    warehouse: String,
}

fn metadata(sku: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("inventory", sku),
        labels: vec!["Product".into()].into(),
        effective_from: time,
    }
}

impl Product {
    fn element(&self, time: u64) -> Element {
        Element::Node {
            metadata: metadata(&self.sku, time),
            properties: ElementPropertyMap::from(serde_json::json!({
                "sku": self.sku,
                "quantity": self.quantity,
                "minimum": self.minimum,
                "warehouse": self.warehouse,
            })),
        }
    }
}

fn parse_inventory(text: &str) -> anyhow::Result<BTreeMap<String, Product>> {
    let products: Vec<Product> = serde_json::from_str(text).context("invalid inventory JSON")?;
    let mut inventory = BTreeMap::new();
    for product in products {
        ensure!(
            !product.sku.trim().is_empty(),
            "product sku must not be empty"
        );
        let sku = product.sku.clone();
        ensure!(
            inventory.insert(sku.clone(), product).is_none(),
            "duplicate product sku: {sku}"
        );
    }
    Ok(inventory)
}

fn inventory_changes(
    previous: &BTreeMap<String, Product>,
    current: &BTreeMap<String, Product>,
    time: u64,
) -> VecDeque<SourceChange> {
    let mut changes = VecDeque::new();
    for (sku, product) in current {
        match previous.get(sku) {
            None => changes.push_back(SourceChange::Insert {
                element: product.element(time),
            }),
            Some(old) if old != product => changes.push_back(SourceChange::Update {
                element: product.element(time),
            }),
            Some(_) => {}
        }
    }
    for sku in previous.keys().filter(|sku| !current.contains_key(*sku)) {
        changes.push_back(SourceChange::Delete {
            metadata: metadata(sku, time),
        });
    }
    changes
}

struct Inventory {
    descriptor: ComponentDescriptor,
    path: PathBuf,
    current: BTreeMap<String, Product>,
    pending: VecDeque<SourceChange>,
    poll: tokio::time::Interval,
    sequence: u64,
    initialized: bool,
    last_error: Option<String>,
}

impl Inventory {
    fn new(path: &Path) -> anyhow::Result<Self> {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Ok(Self {
            descriptor: descriptor(
                "inventory",
                &[(
                    "out",
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                )],
            )?,
            path: path.into(),
            current: BTreeMap::new(),
            pending: VecDeque::new(),
            poll,
            sequence: 0,
            initialized: false,
            last_error: None,
        })
    }

    async fn reload(&mut self) -> anyhow::Result<()> {
        let text = tokio::fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        let current = parse_inventory(&text)?;
        let time = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        self.pending = inventory_changes(&self.current, &current, time);
        self.current = current;
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for Inventory {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        if !self.initialized {
            self.reload().await?;
            self.initialized = true;
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Inventory {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        loop {
            if let Some(change) = self.pending.pop_front() {
                self.sequence = self.sequence.checked_add(1).context("sequence exhausted")?;
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: GraphChangeCodec::encode_change(
                        change,
                        StreamId::try_new("inventory/out")?,
                        self.sequence,
                        None,
                    )?,
                }));
            }
            self.poll.tick().await;
            match self.reload().await {
                Ok(()) => {
                    if self.last_error.take().is_some() {
                        eprintln!("Inventory file is valid again; watching for changes.");
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if self.last_error.as_ref() != Some(&message) {
                        eprintln!(
                            "Inventory file: {message}. Keeping the last valid inventory; retrying."
                        );
                        self.last_error = Some(message);
                    }
                }
            }
        }
    }
}

struct Audit {
    descriptor: ComponentDescriptor,
    sequence: u64,
}

#[async_trait]
impl ComputationComponent for Audit {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for Audit {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        self.sequence = self.sequence.checked_add(1).context("sequence exhausted")?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            StreamId::try_new("audit/out")?,
            self.sequence,
        )?;
        envelope.append_annotation(ContextEntry::try_new(
            self.descriptor.id().clone(),
            "checked-by",
            ContextValue::String("stockroom".into()),
        )?)?;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        }])
    }
}

#[derive(Clone, Default)]
struct ResultView(HashMap<RecordId, serde_json::Value>);

impl ResultView {
    fn apply(&mut self, envelope: &ChangeEnvelope) -> anyhow::Result<()> {
        ensure!(
            !QueryChangeCodec::is_snapshot(envelope)
                && !QueryChangeCodec::is_progress_only(envelope),
            "this live-only view requires a coordinated recovery owner for resets"
        );
        // Publish the batch only after every row has decoded successfully.
        let mut next = self.0.clone();
        for operation in envelope.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                    let row = QueryChangeCodec::decode_row(after)?;
                    next.insert(
                        after.identity().clone(),
                        QueryChangeCodec::row_values_to_json(&row.values),
                    );
                }
                ChangeOperation::Deleted { identity, .. } => {
                    next.remove(identity.identity());
                }
            }
        }
        self.0 = next;
        Ok(())
    }

    fn rows(&self) -> Vec<serde_json::Value> {
        let mut rows = self.0.values().cloned().collect::<Vec<_>>();
        rows.sort_by_cached_key(serde_json::Value::to_string);
        rows
    }
}

struct Alerts {
    descriptor: ComponentDescriptor,
    query_id: &'static str,
    view: watch::Sender<ResultView>,
    output: tokio::io::Stdout,
    audit: bool,
}

#[async_trait]
impl ComputationComponent for Alerts {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.output.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Alerts {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let mut view = self.view.borrow().clone();
        view.apply(&input.envelope)?;
        let mut message = String::new();
        if self.audit {
            ensure!(
                input
                    .envelope
                    .annotations()
                    .entries()
                    .any(|entry| entry.contributor() == "audit"
                        && entry.key() == "checked-by"
                        && entry.value() == ContextValue::String("stockroom".into())),
                "audit context was not preserved"
            );
            message.push_str("audit: checked-by=stockroom\n");
        }
        message.push_str(&format!(
            "{}: {}\n",
            self.query_id,
            serde_json::to_string(&view.rows())?
        ));
        self.output.write_all(message.as_bytes()).await?;
        self.output.flush().await?;
        self.view.send_replace(view);
        Ok(())
    }
}

async fn finish(core: &DrasiLib, work: anyhow::Result<()>) -> anyhow::Result<()> {
    let cleanup = core.shutdown().await;
    if let Err(error) = &cleanup {
        eprintln!("Stockroom shutdown failed: {error:#}");
    }
    work?;
    cleanup?;
    Ok(())
}

async fn start(
    path: &Path,
    scenario: Scenario,
) -> anyhow::Result<(DrasiLib, watch::Receiver<ResultView>)> {
    let audit = matches!(scenario, Scenario::Audit);
    let (query_id, query_text) = scenario.query();
    let inventory = Inventory::new(path)?;
    let (view, reader) = watch::channel(ResultView::default());
    let alerts = Alerts {
        descriptor: descriptor(
            "alerts",
            &[(
                "in",
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
            )],
        )?,
        query_id,
        view,
        output: tokio::io::stdout(),
        audit,
    };
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: ComponentId::try_new(query_id)?,
            query: query_text.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{query_id}/out"))?,
            outbox_capacity: NonZeroUsize::new(128).expect("positive capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?;
    let results = query.results();
    let mut batch = ComponentBatch::builder()
        .source(Box::new(inventory))
        .query(Box::new(query))
        .sink(Box::new(alerts));
    let mut producers = vec!["inventory", query_id];
    let edges = if audit {
        let schema = GraphChangeCodec::schema().descriptor().clone();
        batch = batch.transformer(Box::new(Audit {
            descriptor: descriptor(
                "audit",
                &[
                    ("in", PortDirection::Input, schema.clone()),
                    ("out", PortDirection::Output, schema),
                ],
            )?,
            sequence: 0,
        }));
        producers.push("audit");
        vec![("inventory", "audit"), ("audit", query_id), (query_id, "alerts")]
    } else {
        vec![("inventory", query_id), (query_id, "alerts")]
    };
    for producer in producers {
        batch = batch.bind_stream(
            endpoint(producer, "out")?,
            StreamId::try_new(format!("{producer}/out"))?,
        );
    }
    for (from, to) in edges {
        batch = batch.connect(
            EdgeDefinition::new(endpoint(from, "out")?, endpoint(to, "in")?),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        );
    }
    let core = DrasiLib::builder()
        .with_id("stockroom")
        .with_components(batch.build()?)
        .build()
        .await?;
    let setup = async {
        core.start().await?;
        for component in ["inventory", query_id, "alerts"] {
            tokio::time::timeout(
                Duration::from_secs(5),
                core.computation_component(component)?.wait_started(),
            )
            .await??;
        }
        tokio::time::timeout(Duration::from_secs(5), results.wait_ready()).await??;
        Ok(())
    }
    .await;
    if let Err(error) = setup {
        finish(&core, Err(error)).await?;
    }
    Ok((core, reader))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let scenario = match std::env::args().nth(1).as_deref() {
        None => Scenario::LowStock,
        Some("totals") => Scenario::Totals,
        Some("audit") => Scenario::Audit,
        Some(value) => anyhow::bail!("unknown mode {value:?}; use no argument, totals, or audit"),
    };
    let path = Path::new("inventory.json");
    let (core, _view) = start(path, scenario).await?;
    println!(
        "Watching {} for {}. Save the file to change inventory; press Ctrl-C to stop.",
        path.display(),
        scenario.query().0
    );
    let work = tokio::signal::ctrl_c()
        .await
        .context("cannot listen for Ctrl-C");
    finish(&core, work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(quantity: i64) -> serde_json::Value {
        serde_json::json!([{
            "sku": "bolts", "quantity": quantity, "minimum": 5, "warehouse": "west"
        }])
    }

    async fn save(path: &Path, data: &serde_json::Value) -> anyhow::Result<()> {
        tokio::fs::write(path, serde_json::to_vec(data)?).await?;
        Ok(())
    }

    async fn expect_view(
        reader: &mut watch::Receiver<ResultView>,
        expected: Vec<serde_json::Value>,
    ) -> anyhow::Result<()> {
        tokio::time::timeout(
            Duration::from_secs(5),
            reader.wait_for(|view| view.rows() == expected),
        )
        .await
        .with_context(|| format!("expected view {expected:?}"))??;
        Ok(())
    }

    async fn file_changes(scenario: Scenario) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(8)).await?;
        let (core, mut view) = start(&path, scenario).await?;
        let work = async {
            if matches!(scenario, Scenario::Totals) {
                expect_view(
                    &mut view,
                    vec![serde_json::json!({"warehouse": "west", "total": 8.0})],
                )
                .await?;
            } else {
                ensure!(
                    view.borrow().rows().is_empty(),
                    "startup invented a low-stock row"
                );
            }
            for quantity in [3, 2, 8, 3] {
                save(&path, &inventory(quantity)).await?;
                let expected = if matches!(scenario, Scenario::Totals) {
                    vec![serde_json::json!({"warehouse": "west", "total": quantity as f64})]
                } else if quantity < 5 {
                    vec![serde_json::json!({"sku": "bolts", "quantity": quantity})]
                } else {
                    vec![]
                };
                expect_view(&mut view, expected.clone()).await?;
                ensure!(core.get_query_results(scenario.query().0).await? == expected);
            }
            save(&path, &serde_json::json!([])).await?;
            let expected = if matches!(scenario, Scenario::Totals) {
                vec![serde_json::json!({"warehouse": "west", "total": 0.0})]
            } else {
                vec![]
            };
            expect_view(&mut view, expected.clone()).await?;
            ensure!(core.get_query_results(scenario.query().0).await? == expected);
            Ok(())
        }
        .await;
        finish(&core, work).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saved_file_changes_update_and_retract_rows() -> anyhow::Result<()> {
        file_changes(Scenario::LowStock).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn warehouse_totals_follow_saved_file_changes() -> anyhow::Result<()> {
        file_changes(Scenario::Totals).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_preserves_results_and_context() -> anyhow::Result<()> {
        file_changes(Scenario::Audit).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_changes_work_on_a_multithreaded_executor() -> anyhow::Result<()> {
        file_changes(Scenario::LowStock).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_file_keeps_last_valid_inventory() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(3)).await?;
        let mut source = Inventory::new(&path)?;
        source.start().await?;
        source.pending.clear();
        tokio::fs::write(&path, "[").await?;
        ensure!(source.reload().await.is_err());
        ensure!(source.current["bolts"].quantity == 3 && source.pending.is_empty());
        save(&path, &inventory(2)).await?;
        source.reload().await?;
        ensure!(matches!(
            source.pending.front(),
            Some(SourceChange::Update { .. })
        ));
        source.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_start_preserves_unemitted_file_changes() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(3)).await?;
        let mut source = Inventory::new(&path)?;
        source.start().await?;
        source.stop().await?;
        source.start().await?;
        let output = tokio::time::timeout(Duration::from_secs(1), source.next())
            .await??
            .context("file source unexpectedly exhausted")?;
        ensure!(output.envelope.system().sequence() == 1);
        ensure!(matches!(
            GraphChangeCodec::decode_changes(&output.envelope)?.as_slice(),
            [SourceChange::Insert { .. }]
        ));
        source.stop().await?;
        Ok(())
    }

    #[test]
    fn unchanged_file_emits_nothing_and_duplicate_skus_reject() -> anyhow::Result<()> {
        let data = inventory(3);
        let snapshot = parse_inventory(&data.to_string())?;
        ensure!(inventory_changes(&snapshot, &snapshot, 1).is_empty());
        let duplicate = serde_json::json!([data[0], data[0]]);
        ensure!(parse_inventory(&duplicate.to_string()).is_err());
        ensure!(parse_inventory(r#"[{"sku":"bolts","quantity":3}]"#).is_err());
        Ok(())
    }

    #[test]
    fn equal_rows_keep_distinct_identities_and_deletes_need_no_image() -> anyhow::Result<()> {
        let first = QueryChangeCodec::encode_row(
            "low-stock",
            1,
            &Default::default(),
            QueryRowKind::Row,
            RecordImage::Full,
        )?;
        let second = QueryChangeCodec::encode_row(
            "low-stock",
            2,
            &Default::default(),
            QueryRowKind::Row,
            RecordImage::Full,
        )?;
        let envelope = |sequence, operations| -> anyhow::Result<ChangeEnvelope> {
            let stream = StreamId::try_new("low-stock/out")?;
            Ok(ChangeEnvelope::new(
                emission_id(&stream, sequence)?,
                ChangeSet::try_new(
                    ChangeSetId::try_new("test", sequence.to_be_bytes().to_vec().into())?,
                    QueryChangeCodec::schema().descriptor().clone(),
                    operations,
                )?,
                SystemMetadata::new(stream, sequence),
            ))
        };
        let mut view = ResultView::default();
        view.apply(&envelope(
            1,
            vec![
                ChangeOperation::Added {
                    ordinal: 0,
                    after: first.clone(),
                },
                ChangeOperation::Added {
                    ordinal: 1,
                    after: second.clone(),
                },
            ],
        )?)?;
        ensure!(view.0.len() == 2, "equal values must not collapse two rows");
        view.apply(&envelope(
            2,
            vec![ChangeOperation::Deleted {
                ordinal: 0,
                identity: first.reference().clone(),
                before: None,
            }],
        )?)?;
        ensure!(view.0.len() == 1 && view.0.contains_key(second.identity()));
        Ok(())
    }
}
