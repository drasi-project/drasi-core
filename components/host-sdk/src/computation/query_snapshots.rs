// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{
    abi::{self, query_snapshots::*},
    query_snapshots::{QuerySnapshotMetadata, QuerySnapshotRow},
    transport::{self, Failure},
    wire,
};
use drasi_lib::computation::v1::{ComponentId, QueryChangeCodec, QueryResultsCatalog};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// A scoped read capability over an existing graph-owned query catalogue.
pub struct NativeQuerySnapshotsResource {
    catalog: QueryResultsCatalog,
    queries: BTreeSet<String>,
    timeout: Duration,
    max_readers: usize,
}

impl NativeQuerySnapshotsResource {
    pub fn new(
        catalog: QueryResultsCatalog,
        queries: impl IntoIterator<Item = String>,
        timeout: Duration,
        max_readers: usize,
    ) -> anyhow::Result<Self> {
        let queries = Self::validate_settings(queries, timeout, max_readers)?;
        Ok(Self {
            catalog,
            queries,
            timeout,
            max_readers,
        })
    }

    pub(crate) fn validate_settings(
        queries: impl IntoIterator<Item = String>,
        timeout: Duration,
        max_readers: usize,
    ) -> anyhow::Result<BTreeSet<String>> {
        anyhow::ensure!(
            !timeout.is_zero() && std::time::Instant::now().checked_add(timeout).is_some(),
            "query snapshot timeout must be positive and representable"
        );
        anyhow::ensure!(
            (1..=1024).contains(&max_readers),
            "query snapshot maxReaders must be in 1..=1024"
        );
        let mut allowed = BTreeSet::new();
        for query in queries {
            ComponentId::try_new(query.as_str())?;
            anyhow::ensure!(allowed.insert(query), "duplicate allowed snapshot query");
        }
        anyhow::ensure!(
            !allowed.is_empty() && allowed.len() <= 4096,
            "query snapshots require 1..=4096 allowed queries"
        );
        Ok(allowed)
    }

    pub fn graph_id(&self) -> &str {
        self.catalog.graph_id()
    }
}

struct Epoch {
    active: bool,
    cancelled: CancellationToken,
    runtime: Option<tokio::runtime::Handle>,
}

struct State {
    resource: Weak<NativeQuerySnapshotsResource>,
    epoch: Mutex<Epoch>,
    readers: AtomicUsize,
}

pub(super) struct QuerySnapshotsBinding {
    _resource: Arc<NativeQuerySnapshotsResource>,
    state: Arc<State>,
}

impl QuerySnapshotsBinding {
    pub(super) fn new(resource: Arc<NativeQuerySnapshotsResource>) -> Self {
        Self {
            state: Arc::new(State {
                resource: Arc::downgrade(&resource),
                epoch: Mutex::new(Epoch {
                    active: false,
                    cancelled: CancellationToken::new(),
                    runtime: None,
                }),
                readers: AtomicUsize::new(0),
            }),
            _resource: resource,
        }
    }

    pub(super) fn activate(&self) -> anyhow::Result<()> {
        self.ensure_quiesced()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            anyhow::anyhow!("query snapshots need the owning host runtime: {error}")
        })?;
        let mut epoch = self
            .state
            .epoch
            .lock()
            .map_err(|_| anyhow::anyhow!("query-snapshot lifecycle poisoned"))?;
        anyhow::ensure!(!epoch.active, "query snapshots are already active");
        epoch.cancelled = CancellationToken::new();
        epoch.runtime = Some(runtime);
        epoch.active = true;
        Ok(())
    }

    pub(super) fn deactivate(&self) {
        let mut epoch = self.state.epoch.lock().unwrap_or_else(|error| {
            log::error!("Closing poisoned query-snapshot lifecycle");
            error.into_inner()
        });
        epoch.active = false;
        epoch.cancelled.cancel();
        epoch.runtime = None;
    }

    pub(super) fn ensure_quiesced(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state.readers.load(Ordering::Acquire) == 0,
            "query-snapshot readers still require cleanup"
        );
        Ok(())
    }

    pub(super) fn table(&self) -> QuerySnapshotsV1 {
        QuerySnapshotsV1 {
            header: abi::Header::new::<QuerySnapshotsV1>(),
            context: Arc::as_ptr(&self.state) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            open: Some(open),
        }
    }
}

impl Drop for QuerySnapshotsBinding {
    fn drop(&mut self) {
        self.deactivate();
    }
}

struct ReaderPermit(Arc<State>);
impl Drop for ReaderPermit {
    fn drop(&mut self) {
        self.0.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Reader {
    resource: Arc<NativeQuerySnapshotsResource>,
    query: String,
    cancelled: CancellationToken,
    runtime: tokio::runtime::Handle,
    busy: AtomicBool,
    failed: AtomicBool,
    rows: Mutex<Option<Box<dyn Iterator<Item = serde_json::Value> + Send>>>,
    _permit: ReaderPermit,
}

unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}

unsafe extern "C" fn open(
    context: *mut c_void,
    query: abi::BorrowedBytes,
    out: *mut QuerySnapshotHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol(
                "null query-snapshot capability or output",
            ));
        }
        let state = unsafe { &*context.cast::<State>() };
        let epoch = state
            .epoch
            .lock()
            .map_err(|_| Failure::failed("query-snapshot lifecycle poisoned"))?;
        if !epoch.active {
            return Err(Failure::closed());
        }
        let resource = state.resource.upgrade().ok_or_else(Failure::closed)?;
        let runtime = epoch.runtime.clone().ok_or_else(Failure::closed)?;
        let query = std::str::from_utf8(unsafe { transport::borrowed_bytes(query, 4096)? })
            .map_err(|error| Failure::protocol(error.to_string()))?;
        if !resource.queries.contains(query) {
            return Err(Failure::protocol(
                "query is outside the snapshot capability",
            ));
        }
        state
            .readers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < resource.max_readers).then_some(count + 1)
            })
            .map_err(|_| Failure::new(abi::status::BUSY, "query-snapshot reader limit reached"))?;
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let permit = ReaderPermit(unsafe { Arc::from_raw(context.cast::<State>()) });
        let reader = Arc::new(Reader {
            resource,
            query: query.to_owned(),
            cancelled: epoch.cancelled.child_token(),
            runtime,
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            rows: Mutex::new(None),
            _permit: permit,
        });
        unsafe {
            transport::write_out(
                out,
                QuerySnapshotHandle {
                    state: Box::into_raw(Box::new(reader)).cast(),
                    vtable: &READER,
                },
            )
        }
    })
}

static READER: QuerySnapshotVTable = QuerySnapshotVTable {
    header: abi::Header::new::<QuerySnapshotVTable>(),
    release: Some(release_reader),
    request: Some(request),
};

unsafe extern "C" fn release_reader(state: *mut c_void) {
    transport::drop_boundary(|| {
        if !state.is_null() {
            let reader = unsafe { Box::from_raw(state.cast::<Arc<Reader>>()) };
            reader.cancelled.cancel();
            drop(reader);
        }
    });
}

struct OperationGuard(Arc<Reader>);
impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn request(
    state: *mut c_void,
    code: u32,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if state.is_null() || out.is_null() {
            return Err(Failure::protocol("null query-snapshot reader or output"));
        }
        if !matches!(code, operation::PREPARE | operation::NEXT) {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "unknown snapshot operation",
            ));
        }
        let reader = unsafe { &*state.cast::<Arc<Reader>>() };
        if reader.cancelled.is_cancelled() {
            return Err(Failure::closed());
        }
        if reader.failed.load(Ordering::Acquire) {
            return Err(Failure::failed("query-snapshot reader has failed"));
        }
        if reader.busy.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                abi::status::BUSY,
                "query-snapshot operation is pending",
            ));
        }
        let guard = OperationGuard(reader.clone());
        let runtime = reader.runtime.clone();
        let mut work = Box::pin(async move {
            let result = tokio::select! {
                biased;
                _ = guard.0.cancelled.cancelled() => Err(Failure::closed()),
                result = read(&guard.0, code) => result,
            };
            if result.is_err() {
                guard.0.failed.store(true, Ordering::Release);
            }
            result
        });
        // Timers belong to the host's Tokio instance, not the calling library's
        // independently linked runtime. No runtime object crosses the ABI.
        let operation = transport::export_operation(
            std::future::poll_fn(move |context| {
                let _entered = runtime.enter();
                std::future::Future::poll(work.as_mut(), context)
            }),
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

async fn read(reader: &Reader, code: u32) -> Result<Vec<u8>, Failure> {
    if code == operation::PREPARE {
        if reader
            .rows
            .lock()
            .map_err(|_| Failure::failed("snapshot rows poisoned"))?
            .is_some()
        {
            return Err(Failure::protocol("query snapshot is already prepared"));
        }
        let snapshot = reader
            .resource
            .catalog
            .snapshot(&reader.query, reader.resource.timeout)
            .await
            .map_err(Failure::from)?;
        let metadata = QuerySnapshotMetadata {
            as_of_sequence: snapshot.as_of_sequence,
            generation: snapshot.generation,
        };
        let rows = QueryChangeCodec::legacy_snapshot_rows(&reader.query, snapshot.rows)
            .map_err(|error| Failure::failed(error.to_string()))?;
        let response = wire::encode(&metadata).map_err(Failure::from)?;
        *reader
            .rows
            .lock()
            .map_err(|_| Failure::failed("snapshot rows poisoned"))? =
            Some(Box::new(rows.map(|(_, value)| value)));
        return Ok(response);
    }
    let mut rows = reader
        .rows
        .lock()
        .map_err(|_| Failure::failed("snapshot rows poisoned"))?;
    let rows = rows
        .as_mut()
        .ok_or_else(|| Failure::protocol("snapshot was not prepared"))?;
    wire::encode(&rows.next().map(|value| QuerySnapshotRow { value })).map_err(Failure::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::NativeQuerySnapshots;
    use futures_util::FutureExt;

    fn binding() -> anyhow::Result<(QuerySnapshotsBinding, NativeQuerySnapshots)> {
        let resource = Arc::new(NativeQuerySnapshotsResource::new(
            QueryResultsCatalog::new("snapshots")?,
            ["q".into()],
            Duration::from_secs(30),
            1,
        )?);
        let binding = QuerySnapshotsBinding::new(resource);
        let client = unsafe { NativeQuerySnapshots::from_borrowed(&binding.table())? };
        Ok((binding, client))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_scope_capacity_and_cancelled_open_are_enforced() -> anyhow::Result<()> {
        let (binding, client) = binding()?;
        assert!(client.snapshot("q").await.is_err());
        binding.activate()?;
        assert!(client.snapshot("other").await.is_err());
        let mut pending = Box::pin(client.snapshot("q"));
        assert!(pending.as_mut().now_or_never().is_none());
        assert_eq!(binding.state.readers.load(Ordering::Acquire), 1);
        let error = client.snapshot("q").await.err().expect("reader limit");
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().code,
            abi::status::BUSY
        );
        drop(pending);
        assert_eq!(binding.state.readers.load(Ordering::Acquire), 0);
        binding.deactivate();
        binding.ensure_quiesced()?;
        binding.activate()?;
        let mut replacement = Box::pin(client.snapshot("q"));
        assert!(replacement.as_mut().now_or_never().is_none());
        binding.deactivate();
        assert!(replacement.await.is_err());
        binding.ensure_quiesced()?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stopped_or_dropped_snapshot_bindings_cannot_be_reused() -> anyhow::Result<()> {
        let (binding, client) = binding()?;
        binding.activate()?;
        let mut pending = Box::pin(client.snapshot("q"));
        assert!(pending.as_mut().now_or_never().is_none());
        binding.deactivate();
        assert!(
            binding.activate().is_err(),
            "retained reader must block restart"
        );
        assert!(pending.await.is_err());
        binding.activate()?;
        drop(binding);
        assert!(client.snapshot("q").await.is_err());
        Ok(())
    }

    #[test]
    fn invalid_snapshot_limits_and_duplicate_queries_reject() -> anyhow::Result<()> {
        let catalog = QueryResultsCatalog::new("snapshots")?;
        for (queries, timeout, readers) in [
            (vec![], Duration::from_secs(1), 1),
            (vec!["q".into(), "q".into()], Duration::from_secs(1), 1),
            (vec!["q".into()], Duration::ZERO, 1),
            (vec!["q".into()], Duration::from_secs(1), 0),
            (vec!["q".into()], Duration::from_secs(1), 1025),
        ] {
            assert!(
                NativeQuerySnapshotsResource::new(catalog.clone(), queries, timeout, readers)
                    .is_err()
            );
        }
        Ok(())
    }
}
