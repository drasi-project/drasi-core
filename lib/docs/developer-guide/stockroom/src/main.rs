// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Stockroom: save changes to inventory.json to update a continuous query.
//! Run `cargo run --locked -- help` for the book's runnable lessons.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context};
use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{broadcast, watch},
};

mod cli;
#[cfg(feature = "management")]
mod managed;
#[cfg(feature = "plugins")]
mod network;
#[cfg(feature = "persistence")]
mod persistence;

const GRAPH: &str = ComponentBatch::GRAPH_ID;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOW_STOCK: &str = "
MATCH (p:Product)
WHERE p.quantity < p.minimum
RETURN p.sku AS sku, p.quantity AS quantity";
const WAREHOUSE_TOTALS: &str =
    "MATCH (p:Product) RETURN p.warehouse AS warehouse, sum(p.quantity) AS total";
const NONEMPTY_TOTALS: &str =
    "MATCH (p:Product) WITH p.warehouse AS warehouse, sum(p.quantity) AS total WHERE total > 0 RETURN warehouse, total";
const WAREHOUSE_ALERTS: &str =
    "MATCH (p:Product)-[:STORED_IN]->(w:Warehouse) WHERE p.quantity < p.minimum RETURN p.sku AS sku, p.quantity AS quantity, w.name AS warehouse";
const SUSTAINED: &str =
    "MATCH (p:Product) WHERE drasi.trueFor(p.quantity < p.minimum, duration({seconds:5})) RETURN p.sku AS sku, p.quantity AS quantity";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    LowStock,
    Totals,
    Audit,
    Join,
    Sustained,
    Normalize,
    Reorders,
    Stream,
    NonemptyTotals,
    Callback,
}

impl Scenario {
    fn query(self) -> (&'static str, &'static str) {
        match self {
            Self::Totals => ("stock-by-warehouse", WAREHOUSE_TOTALS),
            Self::NonemptyTotals => ("stock-by-warehouse", NONEMPTY_TOTALS),
            Self::Join => ("warehouse-alerts", WAREHOUSE_ALERTS),
            Self::Sustained => ("sustained-low-stock", SUSTAINED),
            Self::LowStock
            | Self::Audit
            | Self::Normalize
            | Self::Reorders
            | Self::Stream
            | Self::Callback => ("low-stock", LOW_STOCK),
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Product {
    sku: String,
    quantity: i64,
    minimum: i64,
    warehouse: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

// ANCHOR: element-metadata
fn metadata(sku: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("inventory", sku),
        labels: vec!["Product".into()].into(),
        effective_from: time,
    }
}
// ANCHOR_END: element-metadata

// ANCHOR: product-element
impl Product {
    fn element(&self, time: u64) -> Element {
        let mut properties = serde_json::json!({
            "sku": self.sku,
            "quantity": self.quantity,
            "minimum": self.minimum,
            "warehouse": self.warehouse,
        });
        if let Some(payload) = &self.payload {
            properties["payload"] = serde_json::json!(payload);
        }
        Element::Node {
            metadata: metadata(&self.sku, time),
            properties: ElementPropertyMap::from(properties),
        }
    }
}

// ANCHOR_END: product-element

#[cfg(any(test, feature = "persistence"))]
fn parse_inventory(text: &str) -> anyhow::Result<BTreeMap<String, Product>> {
    parse_rows(text)
}

trait FileRow: Clone + PartialEq + serde::de::DeserializeOwned + Send + Sync + 'static {
    const SOURCE: &'static str;
    const LABEL: &'static str;
    fn key(&self) -> &str;
    fn element(&self, time: u64) -> Element;
}

impl FileRow for Product {
    const SOURCE: &'static str = "inventory";
    const LABEL: &'static str = "Product";
    fn key(&self) -> &str {
        &self.sku
    }
    fn element(&self, time: u64) -> Element {
        self.element(time)
    }
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Warehouse {
    code: String,
    name: String,
}

impl FileRow for Warehouse {
    const SOURCE: &'static str = "catalog";
    const LABEL: &'static str = "Warehouse";
    fn key(&self) -> &str {
        &self.code
    }
    fn element(&self, time: u64) -> Element {
        Element::Node {
            metadata: row_metadata::<Self>(&self.code, time),
            properties: ElementPropertyMap::from(
                serde_json::json!({"code": self.code, "name": self.name}),
            ),
        }
    }
}

fn row_metadata<T: FileRow>(key: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(T::SOURCE, key),
        labels: vec![T::LABEL.into()].into(),
        effective_from: time,
    }
}

fn parse_rows<T: FileRow>(text: &str) -> anyhow::Result<BTreeMap<String, T>> {
    let rows: Vec<T> =
        serde_json::from_str(text).with_context(|| format!("invalid {} JSON", T::SOURCE))?;
    let mut result = BTreeMap::new();
    for row in rows {
        let key = row.key().to_owned();
        ensure!(!key.trim().is_empty(), "record key must not be empty");
        ensure!(
            result.insert(key.clone(), row).is_none(),
            "duplicate record key: {key}"
        );
    }
    Ok(result)
}

fn inventory_changes<T: FileRow>(
    previous: &BTreeMap<String, T>,
    current: &BTreeMap<String, T>,
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
                removed_properties: BTreeSet::new(),
            }),
            Some(_) => {}
        }
    }
    for sku in previous.keys().filter(|sku| !current.contains_key(*sku)) {
        changes.push_back(SourceChange::Delete {
            metadata: row_metadata::<T>(sku, time),
        });
    }
    changes
}

struct FileSource<T: FileRow> {
    descriptor: ComponentDescriptor,
    path: PathBuf,
    current: BTreeMap<String, T>,
    pending: VecDeque<SourceChange>,
    poll: tokio::time::Interval,
    sequence: u64,
    initialized: bool,
    last_error: Option<String>,
}

type Inventory = FileSource<Product>;

impl<T: FileRow> FileSource<T> {
    fn new(path: &Path) -> anyhow::Result<Self> {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Ok(Self {
            descriptor: descriptor(
                T::SOURCE,
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
        let current = parse_rows::<T>(&text)?;
        let time = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        self.pending = inventory_changes(&self.current, &current, time);
        self.current = current;
        Ok(())
    }
}

#[async_trait]
impl<T: FileRow> ComputationComponent for FileSource<T> {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"path": self.path}))
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
impl<T: FileRow> EnvelopeSource for FileSource<T> {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        loop {
            // ANCHOR: source-publication
            if let Some(change) = self.pending.pop_front() {
                self.sequence = self.sequence.checked_add(1).context("sequence exhausted")?;
                return Ok(Some(OutputEnvelope {
                    port: PortId::try_new("out")?,
                    envelope: GraphChangeCodec::encode_change(
                        change,
                        StreamId::try_new(format!("{}/out", T::SOURCE))?,
                        self.sequence,
                        None,
                    )?,
                }));
            }
            // ANCHOR_END: source-publication
            self.poll.tick().await;
            match self.reload().await {
                Ok(()) => {
                    if self.last_error.take().is_some() {
                        eprintln!(
                            "{} is valid again; watching for changes.",
                            self.path.display()
                        );
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if self.last_error.as_ref() != Some(&message) {
                        eprintln!(
                            "{}: {message}. Keeping the last valid state; retrying.",
                            self.path.display()
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

// ANCHOR: audit-transform
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

// ANCHOR_END: audit-transform

// ANCHOR: view-shape
#[derive(Clone, Default)]
struct ResultView(HashMap<RecordId, serde_json::Value>);
// ANCHOR_END: view-shape

// ANCHOR: view-methods
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

// ANCHOR_END: view-methods

struct Alerts {
    descriptor: ComponentDescriptor,
    query_id: &'static str,
    view: watch::Sender<ResultView>,
    output: tokio::io::Stdout,
    audit: bool,
    reorders: bool,
    display: bool,
}

fn reorder_messages(envelope: &ChangeEnvelope, previous: &ResultView) -> anyhow::Result<String> {
    let values = |record: &Record| -> anyhow::Result<_> {
        Ok(QueryChangeCodec::row_values_to_json(
            &QueryChangeCodec::decode_row(record)?.values,
        ))
    };
    let mut message = String::new();
    for operation in envelope.changes().operations() {
        match operation {
            ChangeOperation::Added { after, .. } => {
                message.push_str(&format!("reorder created: {}\n", values(after)?));
            }
            ChangeOperation::Updated { before, after, .. } => {
                let before = before
                    .as_ref()
                    .map(values)
                    .transpose()?
                    .or_else(|| previous.0.get(after.identity()).cloned())
                    .context("reorder update has no previous row")?;
                message.push_str(&format!(
                    "reorder revised: {before} -> {}\n",
                    values(after)?
                ));
            }
            ChangeOperation::Deleted {
                identity, before, ..
            } => {
                let before = before
                    .as_ref()
                    .map(values)
                    .transpose()?
                    .or_else(|| previous.0.get(identity.identity()).cloned())
                    .context("reorder cancellation has no previous row")?;
                message.push_str(&format!("reorder cancelled: {before}\n"));
            }
        }
    }
    Ok(message)
}

#[async_trait]
impl ComputationComponent for Alerts {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.output.flush().await?;
        Ok(())
    }
}

// ANCHOR: alert-sink
#[async_trait]
impl EnvelopeSink for Alerts {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let mut view = self.view.borrow().clone();
        let mut message = String::new();
        if self.reorders {
            message.push_str(&reorder_messages(&input.envelope, &view)?);
        }
        view.apply(&input.envelope)?;
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
        if self.display {
            message.push_str(&format!(
                "{}: {}\n",
                self.query_id,
                serde_json::to_string(&view.rows())?
            ));
            self.output.write_all(message.as_bytes()).await?;
            self.output.flush().await?;
        }
        self.view.send_replace(view);
        Ok(())
    }
}

// ANCHOR_END: alert-sink

async fn finish<T>(core: &DrasiLib, work: anyhow::Result<T>) -> anyhow::Result<T> {
    let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
    match (work, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
            "Stockroom work and cleanup failed",
            vec![error, cleanup],
        )
        .into()),
    }
}

#[cfg(feature = "callbacks")]
// ANCHOR: application-callback
fn application_callback(view: watch::Sender<ResultView>) -> anyhow::Result<Box<dyn EnvelopeSink>> {
    use drasi_reaction_application::NativeApplicationReaction;
    let sink = NativeApplicationReaction::callback(
        "alerts",
        QueryChangeCodec::schema().descriptor().clone(),
        move |input: InputEnvelope| {
            let view = view.clone();
            async move {
                let mut next = view.borrow().clone();
                next.apply(&input.envelope)?;
                let message = format!("callback: {}\n", serde_json::to_string(&next.rows())?);
                let mut output = tokio::io::stdout();
                output.write_all(message.as_bytes()).await?;
                output.flush().await?;
                view.send_replace(next);
                Ok(())
            }
        },
    )?;
    Ok(Box::new(sink))
}
// ANCHOR_END: application-callback

#[cfg(feature = "normalize")]
// ANCHOR: normalization
fn normalization() -> anyhow::Result<MiddlewareTransformer> {
    MiddlewareTransformer::new(
        MiddlewareTransformerDefinition {
            id: ComponentId::try_new("normalize")?,
            output_stream: StreamId::try_new("normalize/out")?,
            middleware: serde_json::from_value(serde_json::json!([
                {"kind":"parse_json","name":"read-payload","config":{
                    "target_property":"payload","output_property":"parsed","on_error":"fail",
                    "max_json_size":65536,"max_nesting_depth":8
                }},
                {"kind":"promote","name":"quantity-at-root","config":{
                    "mappings":[{"path":"$.parsed.quantity","target_name":"quantity"}],
                    "on_conflict":"overwrite","on_error":"fail"
                }}
            ]))?,
            pipeline: vec!["read-payload".into(), "quantity-at-root".into()],
        },
        DrasiLib::default_middleware_registry(),
    )
}
// ANCHOR_END: normalization

async fn low_stock_query() -> anyhow::Result<(ContinuousQueryTransformer, QueryResults)> {
    // ANCHOR: query-construction
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: ComponentId::try_new("low-stock")?,
            query: LOW_STOCK.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("low-stock/out")?,
            outbox_capacity: NonZeroUsize::new(128).expect("positive capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?;
    let results = query.results();
    // ANCHOR_END: query-construction
    Ok((query, results))
}

async fn build_application(
    path: &Path,
    scenario: Scenario,
    plugins: Option<(&Path, u16, u16)>,
) -> anyhow::Result<(
    DrasiLib,
    watch::Receiver<ResultView>,
    Option<broadcast::Receiver<ChangeEnvelope>>,
    cli::Health,
)> {
    let audit = matches!(scenario, Scenario::Audit);
    let (query_id, query_text) = scenario.query();
    let (view, reader) = watch::channel(ResultView::default());
    let alerts: Box<dyn EnvelopeSink> = if scenario == Scenario::Callback {
        #[cfg(feature = "callbacks")]
        {
            application_callback(view)?
        }
        #[cfg(not(feature = "callbacks"))]
        anyhow::bail!("callback mode requires --features callbacks");
    } else {
        Box::new(Alerts {
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
            reorders: scenario == Scenario::Reorders,
            display: scenario != Scenario::Stream,
        })
    };
    let execution = if scenario == Scenario::Join {
        use drasi_lib::config::{QueryJoinConfig, QueryJoinKeyConfig};
        QueryExecutionSettings {
            joins: vec![QueryJoinConfig {
                id: "STORED_IN".into(),
                keys: vec![
                    QueryJoinKeyConfig {
                        label: "Product".into(),
                        property: "warehouse".into(),
                    },
                    QueryJoinKeyConfig {
                        label: "Warehouse".into(),
                        property: "code".into(),
                    },
                ],
            }],
            ..Default::default()
        }
    } else {
        QueryExecutionSettings::default()
    };
    let (mut query, results) = if scenario == Scenario::LowStock {
        low_stock_query().await?
    } else {
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: GRAPH.into(),
                id: ComponentId::try_new(query_id)?,
                query: query_text.into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new(format!("{query_id}/out"))?,
                outbox_capacity: NonZeroUsize::new(128).expect("positive capacity"),
            },
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            execution,
            None,
        )
        .await?;
        let results = query.results();
        (query, results)
    };
    let catalog = (scenario == Scenario::Stream)
        .then(|| QueryResultsCatalog::new(GRAPH))
        .transpose()?;
    let events = catalog.as_ref().map(QueryResultsCatalog::subscribe);
    if let Some(catalog) = &catalog {
        query = query.with_result_catalog(catalog)?;
    }
    let mut batch = ComponentBatch::builder()
        .query(Box::new(query))
        .sink(alerts);
    if let Some((directory, http_port, sse_port)) = plugins {
        #[cfg(feature = "plugins")]
        {
            batch = network::connect(batch, directory, query_id, http_port, sse_port)?;
        }
        #[cfg(not(feature = "plugins"))]
        anyhow::bail!(
            "plugins at {} (ports {http_port}/{sse_port}) require --features plugins",
            directory.display()
        );
    } else {
        batch = batch.source(Box::new(Inventory::new(path)?));
    }
    let mut producers = vec!["inventory", query_id];
    let mut edges = if audit {
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
    } else if scenario == Scenario::Normalize {
        #[cfg(feature = "normalize")]
        {
            let normalize = normalization()?;
            batch = batch.transformer(Box::new(normalize));
            producers.push("normalize");
            vec![("inventory", "normalize"), ("normalize", query_id), (query_id, "alerts")]
        }
        #[cfg(not(feature = "normalize"))]
        anyhow::bail!("normalize mode requires --features normalize");
    } else {
        vec![("inventory", query_id), (query_id, "alerts")]
    };
    if scenario == Scenario::Join {
        batch = batch.source(Box::new(FileSource::<Warehouse>::new(
            &path.with_file_name("catalog.json"),
        )?));
        producers.push("catalog");
        edges.push(("catalog", query_id));
    }
    if let Some(catalog) = catalog {
        batch = batch.sink(Box::new(QueryResultsOutlet::new(
            ComponentId::try_new("live-results")?,
            catalog,
        )));
        edges.push((query_id, "live-results"));
    }
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
    // ANCHOR: instance-build
    let core = DrasiLib::builder()
        .with_id("stockroom")
        .with_components(batch.build()?)
        .build()
        .await?;
    // ANCHOR_END: instance-build
    let setup = async {
        let mut health = cli::Health::new(&core)?;
        core.start().await.context("start Stockroom")?;
        let mut required = vec!["inventory", query_id, "alerts"];
        if plugins.is_some() {
            required.push("notifications");
        }
        if scenario == Scenario::Join {
            required.push("catalog");
        }
        if scenario == Scenario::Stream {
            required.push("live-results");
        }
        for component in required {
            cli::wait_started(&core, component, Duration::from_secs(5)).await?;
        }
        tokio::time::timeout(Duration::from_secs(5), results.wait_ready())
            .await
            .with_context(|| format!("wait for query '{query_id}' readiness"))??;
        health.check()?;
        Ok(health)
    }
    .await;
    let health = match setup {
        Ok(health) => health,
        Err(error) => return finish(&core, Err(error)).await,
    };
    Ok((core, reader, events, health))
}

#[cfg(test)]
async fn start(
    path: &Path,
    scenario: Scenario,
) -> anyhow::Result<(DrasiLib, watch::Receiver<ResultView>)> {
    let (core, view, _, _) = build_application(path, scenario, None).await?;
    Ok((core, view))
}

// ANCHOR: host-tracing
fn init_tracing() -> anyhow::Result<()> {
    use drasi_lib::observability::{get_or_init_global_registry, ComponentLogLayer};
    use tracing_subscriber::{prelude::*, EnvFilter};

    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value)?,
        Err(std::env::VarError::NotPresent) => EnvFilter::new("warn"),
        Err(error) => return Err(error.into()),
    };
    let (layer, reload) = tracing_subscriber::reload::Layer::new(None::<ComponentLogLayer>);
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init()?;
    reload.reload(Some(ComponentLogLayer::new(get_or_init_global_registry())))?;
    Ok(())
}
// ANCHOR_END: host-tracing

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run_cli().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            for message in cli::error_messages(&error) {
                eprintln!("Stockroom: {message}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> anyhow::Result<()> {
    init_tracing()?;
    let mut args = std::env::args().skip(1);
    let mode = args.next();
    if mode.as_deref() == Some("keygen") {
        let path = args.next().unwrap_or_else(|| "stockroom.key".into());
        ensure!(args.next().is_none(), "keygen accepts one output path");
        return cli::keygen(Path::new(&path)).await;
    }
    let scenario = match mode.as_deref() {
        None => Scenario::LowStock,
        Some("totals") => Scenario::Totals,
        Some("totals-filtered") => Scenario::NonemptyTotals,
        Some("audit") => Scenario::Audit,
        Some("join") => Scenario::Join,
        Some("sustained") => Scenario::Sustained,
        Some("normalize") => Scenario::Normalize,
        Some("reorders") => Scenario::Reorders,
        Some("stream") => Scenario::Stream,
        Some("callback") => Scenario::Callback,
        Some("plugins") | Some("webhook") | Some("live") | Some("--state") | Some("managed") => {
            Scenario::LowStock
        }
        Some("help") => {
            println!(
                "stockroom [totals|totals-filtered|audit|join|sustained|normalize|reorders|stream|callback]"
            );
            println!("stockroom plugins <native-library-directory> [http-port] [sse-port] (requires --features plugins)");
            println!(
                "stockroom webhook <native-library-directory> <url> (requires --features plugins)"
            );
            println!("stockroom live <native-library-directory> (requires --features plugins)");
            println!(
                "stockroom --state <directory> [--hold-alerts] (requires --features persistence)"
            );
            println!("stockroom managed <directory> <key-file> [threshold] (requires --features management)");
            println!("stockroom keygen [key-file]");
            return Ok(());
        }
        Some(value) => anyhow::bail!("unknown mode {value:?}; run stockroom help"),
    };
    let mut shutdown = cli::Shutdown::new()?;
    if mode.as_deref() == Some("managed") {
        let directory = args.next().context("managed requires a directory")?;
        let key = args.next().context("managed requires a 32-byte key file")?;
        let threshold = args.next().map(|value| value.parse::<i64>()).transpose()?;
        ensure!(args.next().is_none(), "unexpected additional arguments");
        #[cfg(feature = "management")]
        return managed::run(
            Path::new(&directory),
            Path::new(&key),
            threshold,
            &mut shutdown,
        )
        .await;
        #[cfg(not(feature = "management"))]
        anyhow::bail!("managed {directory} {key} {threshold:?} requires --features management");
    }
    if mode.as_deref() == Some("--state") {
        let directory = args.next().context("--state requires a directory")?;
        let hold = match args.next().as_deref() {
            None => false,
            Some("--hold-alerts") => true,
            Some(value) => anyhow::bail!("unsupported state option {value:?}"),
        };
        ensure!(args.next().is_none(), "unexpected additional arguments");
        #[cfg(feature = "persistence")]
        return persistence::run(Path::new(&directory), hold, &mut shutdown).await;
        #[cfg(not(feature = "persistence"))]
        anyhow::bail!("--state {directory} (hold={hold}) requires --features persistence");
    }
    let native_directory = if matches!(mode.as_deref(), Some("plugins" | "webhook" | "live")) {
        Some(PathBuf::from(
            args.next()
                .context("this mode requires a native library directory")?,
        ))
    } else {
        None
    };
    let webhook_url = if mode.as_deref() == Some("webhook") {
        Some(args.next().context("webhook requires a destination URL")?)
    } else {
        None
    };
    let (http_port, sse_port) = if mode.as_deref() == Some("plugins") {
        let http = args
            .next()
            .map(|value| value.parse::<u16>())
            .transpose()
            .context("parse HTTP input port")?
            .unwrap_or(9080);
        let sse = args
            .next()
            .map(|value| value.parse::<u16>())
            .transpose()
            .context("parse SSE output port")?
            .unwrap_or(9081);
        ensure!(
            http != 0 && sse != 0 && http != sse,
            "HTTP and SSE ports must be nonzero and distinct"
        );
        (http, sse)
    } else {
        (9080, 9081)
    };
    ensure!(args.next().is_none(), "unexpected additional arguments");
    let path = Path::new(if scenario == Scenario::Normalize {
        "encoded-inventory.json"
    } else {
        "inventory.json"
    });
    println!(
        "Starting Stockroom for {}. Press Ctrl-C to stop.",
        scenario.query().0,
    );
    let plugins = native_directory
        .as_deref()
        .filter(|_| mode.as_deref() == Some("plugins"))
        .map(|directory| (directory, http_port, sse_port));
    let (core, _view, mut events, mut health) = build_application(path, scenario, plugins).await?;
    let work = async {
        if mode.as_deref() == Some("live") {
            let directory = native_directory.as_deref().context("missing native library directory")?;
            #[cfg(feature = "plugins")]
            return network::watch_destination(&core, directory, &mut shutdown, &mut health).await;
            #[cfg(not(feature = "plugins"))]
            anyhow::bail!("live {} requires --features plugins", directory.display());
        }
        if let Some(url) = webhook_url {
            let directory = native_directory.as_deref().context("missing native library directory")?;
            #[cfg(feature = "plugins")]
            network::add_webhook(&core, directory, &url).await?;
            #[cfg(not(feature = "plugins"))]
            anyhow::bail!("webhook {} {url} requires --features plugins", directory.display());
        }
        if plugins.is_some() {
            println!("Ready: HTTP input at 127.0.0.1:{http_port}; SSE output at 127.0.0.1:{sse_port}/events.");
        } else {
            println!("Ready. Watching {} for saved changes.", path.display());
        }
        if let Some(events) = &mut events {
            loop {
                tokio::select! {
                    biased;
                    result = health.failed() => { result?; }
                    signal = shutdown.requested() => { signal?; break; }
                    event = events.recv() => {
                        let event = event.context("live result stream closed or lagged; rebuild the view")?;
                        println!("stream sequence {}: {} operations",
                            QueryChangeCodec::query_sequence(&event)?,
                            event.changes().operations().len());
                    }
                }
            }
        } else {
            tokio::select! {
                biased;
                result = health.failed() => result?,
                result = shutdown.requested() => result?,
            }
        }
        health.check()?;
        Ok(())
    }.await;
    finish(&core, work).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailsLater {
        descriptor: ComponentDescriptor,
        fail: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ComputationComponent for FailsLater {
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
    impl ComputationService for FailsLater {
        async fn run(&mut self) -> anyhow::Result<()> {
            self.fail.notified().await;
            anyhow::bail!("injected destination failure")
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn health_reports_a_failure_even_after_stop_acknowledges_it() -> anyhow::Result<()> {
        let fail = Arc::new(tokio::sync::Notify::new());
        let core = DrasiLib::builder()
            .with_id("health-test")
            .with_components(
                ComponentBatch::builder()
                    .service(Box::new(FailsLater {
                        descriptor: descriptor("destination", &[])?,
                        fail: fail.clone(),
                    }))
                    .build()?,
            )
            .build()
            .await?;
        let work = async {
            let mut health = cli::Health::new(&core)?;
            core.start().await?;
            cli::wait_started(&core, "destination", Duration::from_secs(5)).await?;
            health.check()?;
            let control = core.computation_control()?;
            let mut observed = control.subscribe_observed();
            fail.notify_one();
            tokio::time::timeout(
                Duration::from_secs(5),
                observed.wait_for(|snapshot| {
                    snapshot
                        .components
                        .values()
                        .any(|component| component.failure.is_some())
                }),
            )
            .await??;
            core.computation_component("destination")?.stop().await?;
            ensure!(core
                .computation_component("destination")?
                .observed()?
                .failure
                .is_none());
            let error = health
                .check()
                .expect_err("retained failure must be reported");
            let messages = cli::error_messages(&error).join("\n");
            ensure!(
                messages.contains("component 'destination' failed"),
                "{messages}"
            );
            ensure!(
                messages.contains("injected destination failure"),
                "{messages}"
            );
            Ok(())
        }
        .await;
        finish(&core, work).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_connection_is_a_wait_with_a_named_readiness_deadline() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        tokio::fs::write(&path, "[]").await?;
        let (query, results) = low_stock_query().await?;
        let (view, _) = watch::channel(ResultView::default());
        let core = DrasiLib::builder()
            .with_id("missing-connection-test")
            .with_components(
                ComponentBatch::builder()
                    .source(Box::new(Inventory::new(&path)?))
                    .query(Box::new(query))
                    .sink(Box::new(Alerts {
                        descriptor: descriptor(
                            "alerts",
                            &[(
                                "in",
                                PortDirection::Input,
                                QueryChangeCodec::schema().descriptor().clone(),
                            )],
                        )?,
                        query_id: "low-stock",
                        view,
                        output: tokio::io::stdout(),
                        audit: false,
                        reorders: false,
                        display: true,
                    }))
                    .bind_stream(
                        endpoint("inventory", "out")?,
                        StreamId::try_new("inventory/out")?,
                    )
                    .bind_stream(
                        endpoint("low-stock", "out")?,
                        StreamId::try_new("low-stock/out")?,
                    )
                    .connect(
                        EdgeDefinition::new(
                            endpoint("low-stock", "out")?,
                            endpoint("alerts", "in")?,
                        ),
                        Box::new(BoundedPipeConfig { capacity: 8 }),
                    )
                    .build()?,
            )
            .build()
            .await?;
        let work = async {
            let mut health = cli::Health::new(&core)?;
            core.start().await?;
            health.check()?;
            let error = cli::wait_started(&core, "inventory", Duration::from_millis(30))
                .await
                .expect_err("an unconnected source cannot become ready");
            let messages = cli::error_messages(&error).join("\n");
            ensure!(
                messages.contains("component 'inventory' readiness"),
                "{messages}"
            );
            ensure!(
                messages.contains("connection for 'inventory:out'"),
                "{messages}"
            );
            ensure!(messages.contains("deadline has elapsed"), "{messages}");
            ensure!(!messages.contains("Blocked"), "{messages}");
            health.check()?;
            let report = core
                .computation_control()?
                .connect(
                    EdgeDefinition::new(
                        endpoint("inventory", "out")?,
                        endpoint("low-stock", "in")?,
                    ),
                    Box::new(BoundedPipeConfig { capacity: 8 }),
                    RelationshipPolicy::default(),
                )
                .await?;
            ensure!(report.summary == OperationSummary::Completed);
            for component in ["inventory", "low-stock", "alerts"] {
                cli::wait_started(&core, component, Duration::from_secs(5)).await?;
            }
            tokio::time::timeout(Duration::from_secs(5), results.wait_ready()).await??;
            health.check()?;
            Ok(())
        }
        .await;
        finish(&core, work).await
    }

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
    async fn joined_view_changes_when_either_file_changes() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        let catalog = directory.path().join("catalog.json");
        save(&path, &inventory(3)).await?;
        save(
            &catalog,
            &serde_json::json!([{"code":"west","name":"West warehouse"}]),
        )
        .await?;
        let (core, mut view) = start(&path, Scenario::Join).await?;
        let work = async {
            expect_view(
                &mut view,
                vec![serde_json::json!({
                    "sku":"bolts","quantity":3,"warehouse":"West warehouse"
                })],
            )
            .await?;
            save(
                &catalog,
                &serde_json::json!([{"code":"west","name":"West depot"}]),
            )
            .await?;
            expect_view(
                &mut view,
                vec![serde_json::json!({
                    "sku":"bolts","quantity":3,"warehouse":"West depot"
                })],
            )
            .await?;
            save(&path, &inventory(8)).await?;
            expect_view(&mut view, vec![]).await
        }
        .await;
        finish(&core, work).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sustained_condition_fires_without_another_edit() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(3)).await?;
        let (core, mut view) = start(&path, Scenario::Sustained).await?;
        let work = async {
            ensure!(
                tokio::time::timeout(
                    Duration::from_millis(500),
                    view.wait_for(|view| !view.0.is_empty())
                )
                .await
                .is_err(),
                "sustained condition fired early"
            );
            tokio::time::timeout(
                Duration::from_secs(8),
                view.wait_for(|view| !view.0.is_empty()),
            )
            .await??;
            save(&path, &inventory(8)).await?;
            expect_view(&mut view, vec![]).await
        }
        .await;
        finish(&core, work).await
    }

    #[cfg(feature = "normalize")]
    #[tokio::test(flavor = "current_thread")]
    async fn normalization_uses_the_payload_quantity() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("encoded-inventory.json");
        let mut data = inventory(8);
        data[0]["payload"] = serde_json::json!(r#"{"quantity":3}"#);
        save(&path, &data).await?;
        let (core, mut view) = start(&path, Scenario::Normalize).await?;
        let work = async {
            expect_view(
                &mut view,
                vec![serde_json::json!({"sku":"bolts","quantity":3})],
            )
            .await?;
            data[0]["payload"] = serde_json::json!(r#"{"quantity":8}"#);
            save(&path, &data).await?;
            expect_view(&mut view, vec![]).await
        }
        .await;
        finish(&core, work).await
    }

    #[cfg(feature = "normalize")]
    #[tokio::test(flavor = "current_thread")]
    async fn normalization_enforces_the_documented_size_and_depth_limits() -> anyhow::Result<()> {
        let oversized = serde_json::json!({"quantity":3,"padding":"x".repeat(65_536)}).to_string();
        let mut deep = serde_json::json!(0);
        for _ in 0..12 {
            deep = serde_json::json!({"nested":deep});
        }
        let too_deep = serde_json::json!({"quantity":3,"deep":deep}).to_string();
        for (payload, expected) in
            [(oversized, "maximum allowed size"), (too_deep, "JSON nesting depth")]
        {
            let product = Product {
                sku: "bolts".into(),
                quantity: 8,
                minimum: 5,
                warehouse: "west".into(),
                payload: Some(payload),
            };
            let mut transformer = normalization()?;
            transformer.start().await?;
            let error = transformer
                .transform(InputEnvelope {
                    port: PortId::try_new("in")?,
                    envelope: GraphChangeCodec::encode_change(
                        SourceChange::Insert {
                            element: product.element(1),
                        },
                        StreamId::try_new("inventory/out")?,
                        1,
                        None,
                    )?,
                })
                .await
                .expect_err("the configured limit must reject this input");
            transformer.stop().await?;
            ensure!(format!("{error:#}").contains(expected), "{error:#}");
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn filtered_total_retracts_its_empty_group() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(3)).await?;
        let (core, mut view) = start(&path, Scenario::NonemptyTotals).await?;
        let work = async {
            expect_view(
                &mut view,
                vec![serde_json::json!({"warehouse":"west","total":3.0})],
            )
            .await?;
            save(&path, &serde_json::json!([])).await?;
            expect_view(&mut view, vec![]).await
        }
        .await;
        finish(&core, work).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn east_group_contrast_preserves_the_west_group() -> anyhow::Result<()> {
        for scenario in [Scenario::Totals, Scenario::NonemptyTotals] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("inventory.json");
            let west = serde_json::json!({"warehouse":"west","total":8.0});
            save(&path, &inventory(8)).await?;
            let (core, mut view) = start(&path, scenario).await?;
            let work = async {
                expect_view(&mut view, vec![west.clone()]).await?;
                let mut both = inventory(8);
                both.as_array_mut()
                    .expect("inventory array")
                    .push(serde_json::json!({
                        "sku":"washers","quantity":3,"minimum":5,"warehouse":"east"
                    }));
                save(&path, &both).await?;
                expect_view(
                    &mut view,
                    vec![serde_json::json!({"warehouse":"east","total":3.0}), west.clone()],
                )
                .await?;
                save(&path, &inventory(8)).await?;
                let expected = if scenario == Scenario::Totals {
                    vec![serde_json::json!({"warehouse":"east","total":0.0}), west.clone()]
                } else {
                    vec![west.clone()]
                };
                expect_view(&mut view, expected).await
            }
            .await;
            finish(&core, work).await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_outlet_stream_preserves_before_and_after() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("inventory.json");
        save(&path, &inventory(8)).await?;
        let (core, mut view, events, _) = build_application(&path, Scenario::Stream, None).await?;
        let mut events = events.context("stream mode did not create an outlet")?;
        let work = async {
            save(&path, &inventory(3)).await?;
            let added = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
            ensure!(reorder_messages(&added, &ResultView::default())?.contains("reorder created"));
            expect_view(
                &mut view,
                vec![serde_json::json!({"sku":"bolts","quantity":3})],
            )
            .await?;
            let previous = view.borrow().clone();
            save(&path, &inventory(2)).await?;
            let updated = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
            let message = reorder_messages(&updated, &previous)?;
            ensure!(message.contains(r#""quantity":3"#) && message.contains(r#""quantity":2"#));
            save(&path, &inventory(8)).await?;
            let deleted = tokio::time::timeout(Duration::from_secs(5), events.recv()).await??;
            ensure!(reorder_messages(&deleted, &view.borrow())?.contains("reorder cancelled"));
            Ok(())
        }
        .await;
        finish(&core, work).await
    }

    #[test]
    fn batch_graph_identity_is_the_public_constant() -> anyhow::Result<()> {
        ensure!(ComponentBatch::builder().build()?.definition.graph_id == ComponentBatch::GRAPH_ID);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn audit_preserves_results_and_context() -> anyhow::Result<()> {
        file_changes(Scenario::Audit).await
    }

    #[cfg(feature = "callbacks")]
    #[tokio::test(flavor = "current_thread")]
    async fn native_application_callback_updates_the_view() -> anyhow::Result<()> {
        file_changes(Scenario::Callback).await
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
        let projector = ResultNodeProjector::try_new("low-stock/out", None)?;
        let first = QueryChangeCodec::encode_row(&projector, 1, &Default::default(), 1)?;
        let second = QueryChangeCodec::encode_row(&projector, 2, &Default::default(), 1)?;
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
