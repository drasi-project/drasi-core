// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Early real-library resource proof. This does not enable production Query loading.
#![allow(clippy::print_stdout)]

#[path = "../../tests/fixtures/query_proof/engine.rs"]
mod engine;
#[path = "../../tests/fixtures/query_proof/wire.rs"]
mod wire;

use super::transaction::{RequestHandler, RequestScope};
use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::*;
use drasi_computation_plugin_sdk::{
    abi,
    transport::{checked_table, take_status, OperationFuture, ReceivedBytes},
};
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationTransaction, InMemoryComputationProvider},
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    interface::{ElementStream, IndexSet, PushType},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryEvaluator,
};
use futures_util::{StreamExt, TryStreamExt};
use ordered_float::OrderedFloat;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

struct IndexRequests<'a> {
    indexes: &'a IndexSet,
    calls: &'a AtomicUsize,
}

async fn scan(stream: ElementStream) -> Result<Vec<u8>> {
    let elements: Vec<_> = stream
        .take(wire::SCAN_LIMIT + 1)
        .map_ok(|element| element.as_ref().clone())
        .try_collect()
        .await?;
    ensure!(
        elements.len() <= wire::SCAN_LIMIT,
        "proof scan requires paging beyond its explicit limit"
    );
    wire::encode(&elements)
}

#[async_trait::async_trait]
impl RequestHandler for IndexRequests<'_> {
    async fn apply(&self, code: u32, input: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            code == wire::RESOURCE_REQUEST,
            "unsupported query proof request"
        );
        self.calls.fetch_add(1, Ordering::Relaxed);
        use wire::Request::*;
        let indexes = self.indexes;
        match wire::decode(input)? {
            GetElement(reference) => wire::encode(
                &indexes
                    .element_index
                    .get_element(&reference)
                    .await?
                    .as_deref(),
            ),
            SetElement(element, slots) => {
                indexes.element_index.set_element(&element, &slots).await?;
                wire::encode(&())
            }
            DeleteElement(reference) => {
                indexes.element_index.delete_element(&reference).await?;
                wire::encode(&())
            }
            GetSlot(slot, reference) => wire::encode(
                &indexes
                    .element_index
                    .get_slot_element_by_ref(slot, &reference)
                    .await?
                    .as_deref(),
            ),
            ScanSlot(slot) => scan(indexes.element_index.get_slot_elements(slot).await?).await,
            ScanInbound(slot, reference) => {
                scan(
                    indexes
                        .element_index
                        .get_slot_elements_by_inbound(slot, &reference)
                        .await?,
                )
                .await
            }
            ScanOutbound(slot, reference) => {
                scan(
                    indexes
                        .element_index
                        .get_slot_elements_by_outbound(slot, &reference)
                        .await?,
                )
                .await
            }
            Configure { virtual_joins } => {
                ensure!(
                    !virtual_joins,
                    "the early query proof does not implement virtual-join setup"
                );
                wire::encode(&())
            }
            GetResult(key, owner) => wire::encode(
                &indexes
                    .result_index
                    .get(&key.decode()?, &owner.into())
                    .await?
                    .map(wire::Accumulator::from),
            ),
            SetResult(key, owner, value) => {
                indexes
                    .result_index
                    .set(key.decode()?, owner.into(), value.map(Into::into))
                    .await?;
                wire::encode(&())
            }
            NextValue(set, bits) => {
                let next = indexes
                    .result_index
                    .get_next(set, bits.map(|bits| OrderedFloat(f64::from_bits(bits))))
                    .await?;
                let next = next
                    .map(|(value, count)| {
                        Ok::<_, anyhow::Error>((value.0.to_bits(), i64::try_from(count)?))
                    })
                    .transpose()?;
                wire::encode(&next)
            }
            ValueCount(set, bits) => wire::encode(&i64::try_from(
                indexes
                    .result_index
                    .get_value_count(set, OrderedFloat(f64::from_bits(bits)))
                    .await?,
            )?),
            IncrementCount(set, bits, delta) => {
                indexes
                    .result_index
                    .increment_value_count(
                        set,
                        OrderedFloat(f64::from_bits(bits)),
                        isize::try_from(delta)?,
                    )
                    .await?;
                wire::encode(&())
            }
            GetSequence => {
                let value = indexes.result_index.get_sequence().await?;
                wire::encode(&(value.sequence, value.source_change_id.as_ref()))
            }
            ApplySequence(sequence, source) => {
                indexes
                    .result_index
                    .apply_sequence(sequence, &source)
                    .await?;
                wire::encode(&())
            }
            PushFuture(kind, position, group, reference, original, due) => {
                let kind = match kind {
                    0 => PushType::Always,
                    1 => PushType::IfNotExists,
                    2 => PushType::Overwrite,
                    _ => anyhow::bail!("invalid future push kind"),
                };
                wire::encode(
                    &indexes
                        .future_queue
                        .push(kind, position, group, &reference, original, due)
                        .await?,
                )
            }
            RemoveFuture(position, group) => {
                indexes.future_queue.remove(position, group).await?;
                wire::encode(&())
            }
            PopFuture => wire::encode(&indexes.future_queue.pop().await?),
            PeekFuture => wire::encode(&indexes.future_queue.peek_due_time().await?),
            Clear(kind) => {
                match kind {
                    0 => indexes.element_index.clear().await?,
                    1 => indexes.result_index.clear().await?,
                    2 => indexes.future_queue.clear().await?,
                    _ => anyhow::bail!("invalid query index family"),
                }
                wire::encode(&())
            }
        }
    }
}

struct Remote {
    handle: abi::ComponentHandle,
}
// Calls remain serialized; opaque handles are accessed only by their producer.
unsafe impl Send for Remote {}
unsafe impl Sync for Remote {}

impl Remote {
    fn new(query: &str) -> Result<Self> {
        static LIBRARY: OnceLock<Result<libloading::Library, String>> = OnceLock::new();
        let library = LIBRARY
            .get_or_init(|| {
                let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap();
                let target = std::env::var_os("CARGO_TARGET_DIR")
                    .map(PathBuf::from)
                    .map(|path| {
                        if path.is_absolute() {
                            path
                        } else {
                            workspace.join(path)
                        }
                    })
                    .unwrap_or_else(|| workspace.join("target"));
                let profile = if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                };
                let path = target.join(profile).join("examples").join(format!(
                    "{}native_query_proof{}",
                    std::env::consts::DLL_PREFIX,
                    std::env::consts::DLL_SUFFIX,
                ));
                if !path.is_file() {
                    return Err(format!(
                        "missing {}; build the native_query_proof example first",
                        path.display()
                    ));
                }
                unsafe { libloading::Library::new(path) }.map_err(|error| error.to_string())
            })
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let input = wire::encode(&wire::Create {
            query: query.into(),
        })?;
        let handle = unsafe {
            type Entry =
                unsafe extern "C" fn(abi::BorrowedBytes, *mut abi::ComponentHandle) -> abi::Status;
            let entry: libloading::Symbol<'_, Entry> = library.get(wire::ENTRY)?;
            let mut handle = abi::ComponentHandle::null();
            take_status(entry(abi::BorrowedBytes::new(&input), &mut handle))?;
            handle
        };
        let table = unsafe { checked_table(handle.vtable)? };
        ensure!(
            !handle.state.is_null() && table.release.is_some() && table.begin.is_some(),
            "invalid query proof handle"
        );
        Ok(Self { handle })
    }

    fn begin(
        &self,
        operation: u32,
        input: &[u8],
        scope: &abi::Transaction,
    ) -> Result<OperationFuture> {
        unsafe {
            let table = checked_table(self.handle.vtable)?;
            let mut output = abi::OperationHandle::null();
            take_status(table.begin.context("missing query operation")?(
                self.handle.state,
                operation,
                abi::BorrowedBytes::new(input),
                scope,
                &mut output,
            ))?;
            Ok(OperationFuture::new(output)?)
        }
    }

    async fn invoke(
        &self,
        owner: &ComputationTransaction,
        calls: &AtomicUsize,
        operation: u32,
        input: &[u8],
    ) -> Result<ReceivedBytes> {
        Ok(owner
            .run(async {
                let mut scope =
                    RequestScope::new(wire::RESOURCE_REQUEST..=wire::RESOURCE_REQUEST, wire::LIMIT);
                let operation = self
                    .begin(operation, input, &scope.table())
                    .map_err(wire::index_error)?;
                let handler = IndexRequests {
                    indexes: owner.resources().indexes(),
                    calls,
                };
                tokio::pin!(operation);
                let result = tokio::select! {
                    result = &mut operation => result.map_err(anyhow::Error::from),
                    result = scope.serve(&handler) => match result {
                        Err(error) => Err(error),
                        Ok(()) => Err(anyhow::anyhow!("query resource service ended unexpectedly")),
                    },
                };
                scope.finish().map_err(wire::index_error)?;
                Ok(result.map_err(wire::index_error)?)
            })
            .await?)
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if !self.handle.state.is_null() && !self.handle.vtable.is_null() {
            unsafe {
                if let Some(release) = (*self.handle.vtable).release {
                    release(self.handle.state);
                }
            }
        }
    }
}

enum Backend {
    Direct(QueryEvaluator),
    Remote(Remote),
}

enum ProofOutput {
    Direct(Vec<u8>),
    Dynamic(ReceivedBytes),
}

impl AsRef<[u8]> for ProofOutput {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Direct(value) => value,
            Self::Dynamic(value) => value,
        }
    }
}

struct Driver {
    owner: ComputationTransaction,
    backend: Backend,
    calls: AtomicUsize,
}

impl Driver {
    async fn new(
        query: &str,
        remote: bool,
        provider: &dyn ComputationIndexProvider,
    ) -> Result<Self> {
        let owner = ComputationTransaction::for_query(
            provider.create_indexes("query-proof", "query").await?,
        );
        let calls = AtomicUsize::new(0);
        let backend = if remote {
            let remote = Remote::new(query)?;
            remote
                .invoke(&owner, &calls, abi::operation::START, &[])
                .await?;
            Backend::Remote(remote)
        } else {
            let indexes = owner.resources().indexes();
            Backend::Direct(
                owner
                    .run(async {
                        engine::build(
                            query,
                            indexes.element_index.clone(),
                            indexes.result_index.clone(),
                            indexes.future_queue.clone(),
                        )
                        .await
                        .map_err(wire::index_error)
                        .map_err(Into::into)
                    })
                    .await?,
            )
        };
        Ok(Self {
            owner,
            backend,
            calls,
        })
    }

    async fn evaluate(&self, input: &[u8]) -> Result<ProofOutput> {
        match &self.backend {
            Backend::Direct(query) => Ok(ProofOutput::Direct(
                self.owner
                    .run(async {
                        engine::evaluate(query, input)
                            .await
                            .map_err(wire::index_error)
                            .map_err(Into::into)
                    })
                    .await?,
            )),
            Backend::Remote(remote) => remote
                .invoke(&self.owner, &self.calls, abi::operation::TRANSFORM, input)
                .await
                .map(ProofOutput::Dynamic),
        }
    }

    async fn close(self) -> Result<()> {
        self.owner.shutdown().await?;
        Ok(())
    }
}

fn input(sequence: usize) -> Result<Vec<u8>> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", &format!("key-{}", sequence % 64)),
            labels: Arc::from([Arc::from("Sample")]),
            effective_from: sequence as u64 + 1,
        },
        properties: ElementPropertyMap::from(serde_json::json!({
            "key": sequence % 64, "value": sequence + 1,
        })),
    };
    let change = if sequence < 64 {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    };
    let envelope = GraphChangeCodec::encode_change(
        change,
        StreamId::try_new("source/out")?,
        sequence as u64 + 1,
        Some(chrono::DateTime::from_timestamp_millis(1_000 + sequence as i64).unwrap()),
    )?;
    Ok(engine::codec().encode(&envelope)?.to_vec())
}

fn results(output: &[u8]) -> Result<Vec<QueryPartEvaluationContext>> {
    let output: Option<serde_bytes::ByteBuf> = wire::decode(output)?;
    match output {
        Some(bytes) => Ok(QueryChangeCodec::decode_evaluation(
            &engine::codec().decode(&bytes)?,
        )?),
        None => Ok(Vec::new()),
    }
}

fn apply(rows: &mut BTreeMap<u64, QueryVariables>, changes: Vec<QueryPartEvaluationContext>) {
    use QueryPartEvaluationContext::*;
    for change in changes {
        match change {
            Adding {
                after,
                row_signature,
            }
            | Updating {
                after,
                row_signature,
                ..
            }
            | Aggregation {
                after,
                row_signature,
                ..
            } => {
                rows.insert(row_signature, after);
            }
            Removing { row_signature, .. } => {
                rows.remove(&row_signature);
            }
            Noop => {}
        }
    }
}

async fn verify_memory() -> Result<()> {
    for (query, aggregate) in [
        (
            "MATCH (n:Sample) RETURN n.key AS key, n.value AS value",
            false,
        ),
        ("MATCH (n:Sample) RETURN sum(n.value) AS total", true),
    ] {
        let direct = Driver::new(query, false, &InMemoryComputationProvider).await?;
        let remote = Driver::new(query, true, &InMemoryComputationProvider).await?;
        let mut rows = BTreeMap::new();
        let mut expected = [0usize; 64];
        for sequence in 0..192 {
            let input = input(sequence)?;
            let baseline = results(direct.evaluate(&input).await?.as_ref())?;
            let actual = results(remote.evaluate(&input).await?.as_ref())?;
            assert_eq!(actual.len(), baseline.len());
            for (actual, baseline) in actual.iter().zip(&baseline) {
                assert!(actual.data_eq(baseline));
                assert_eq!(actual.row_signature(), baseline.row_signature());
            }
            apply(&mut rows, actual);
            expected[sequence % 64] = sequence + 1;
            if aggregate {
                assert_eq!(rows.len(), 1);
                let value = QueryChangeCodec::row_values_to_json(rows.values().next().unwrap());
                assert_eq!(
                    value["total"].as_f64(),
                    Some(expected.iter().sum::<usize>() as f64)
                );
            } else {
                assert_eq!(rows.len(), (sequence + 1).min(64));
                for row in rows.values() {
                    let row = QueryChangeCodec::row_values_to_json(row);
                    let key = row["key"].as_u64().unwrap() as usize;
                    assert_eq!(row["value"].as_u64(), Some(expected[key] as u64));
                }
            }
        }
        assert!(remote.calls.load(Ordering::Relaxed) > 192);
        direct.close().await?;
        remote.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_query_resource_proof_matches_direct_current_thread() -> Result<()> {
    verify_memory().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_query_resource_proof_matches_direct_multi_thread() -> Result<()> {
    verify_memory().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "explicit interleaved resource-boundary measurement, not ordinary conformance"]
async fn dynamic_query_resource_proof_measurement() -> Result<()> {
    const WARMUP: usize = 256;
    const EVENTS: usize = 2000;
    let inputs: Vec<_> = (0..WARMUP + EVENTS).map(input).collect::<Result<_>>()?;
    for (workload, query) in [
        (
            "projection",
            "MATCH (n:Sample) RETURN n.key AS key, n.value AS value",
        ),
        ("aggregate", "MATCH (n:Sample) RETURN sum(n.value) AS total"),
    ] {
        for repetition in 0..5 {
            for remote in if repetition % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let driver = Driver::new(query, remote, &InMemoryComputationProvider).await?;
                let mut rows = BTreeMap::new();
                for value in &inputs[..WARMUP] {
                    apply(&mut rows, results(driver.evaluate(value).await?.as_ref())?);
                }
                driver.calls.store(0, Ordering::Relaxed);
                let mut elapsed = Vec::with_capacity(EVENTS);
                for value in &inputs[WARMUP..] {
                    let start = Instant::now();
                    let output = driver.evaluate(value).await?;
                    elapsed.push(start.elapsed().as_nanos());
                    apply(&mut rows, results(output.as_ref())?);
                }
                let count = WARMUP + EVENTS;
                let mut expected = [0usize; 64];
                for sequence in count - 64..count {
                    expected[sequence % 64] = sequence + 1;
                }
                if workload == "aggregate" {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        QueryChangeCodec::row_values_to_json(rows.values().next().unwrap())
                            ["total"]
                            .as_f64(),
                        Some(expected.iter().sum::<usize>() as f64),
                    );
                } else {
                    assert_eq!(rows.len(), 64);
                    for row in rows.values() {
                        let value = QueryChangeCodec::row_values_to_json(row);
                        assert_eq!(
                            value["value"].as_u64(),
                            Some(expected[value["key"].as_u64().unwrap() as usize] as u64)
                        );
                    }
                }
                let total: u128 = elapsed.iter().sum();
                elapsed.sort_unstable();
                println!(
                    "QUERY_PROOF {}",
                    serde_json::json!({
                        "workload": workload,
                        "remote": remote,
                        "repetition": repetition,
                        "events": EVENTS,
                        "total_ns": total,
                        "p50_ns": elapsed[EVENTS / 2],
                        "p95_ns": elapsed[EVENTS * 95 / 100],
                        "resource_calls": driver.calls.load(Ordering::Relaxed),
                        "verified_rows": rows.len(),
                    })
                );
                driver.close().await?;
            }
        }
    }
    Ok(())
}
