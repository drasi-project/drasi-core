// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host side of the output-snapshot read capability.
//!
//! [`NativeOutputSnapshotsResource`] is an immutable policy: the output
//! endpoints a component may read and the reader limits. It holds no catalogue
//! and no graph authority. The native factory combines it with the graph's
//! actual [`OutputCatalogue`] when it creates the binding, so a snapshot is
//! always read from the same graph that constructs the component.

use drasi_computation_plugin_sdk::{
    abi::{self, output_snapshots::*},
    output_reads::{Outcome, WireCursor, WireRecord},
    output_snapshots::{OutputSnapshotHeader, SnapshotOpen},
    transport::{self, Failure},
    wire,
};
use drasi_lib::computation::v1::{
    ComponentId, OutputCatalogue, OutputEndpoint, OutputReadError, PortId, Record,
};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

/// One allow-listed output. An omitted port means the component's primary
/// output, resolved by the graph's shared port resolution, never by position.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputSelector {
    pub component: ComponentId,
    pub port: Option<PortId>,
}

impl OutputSelector {
    pub fn new(component: ComponentId, port: Option<PortId>) -> Self {
        Self { component, port }
    }
}

/// The immutable allow-list and limits of one output-snapshot capability.
#[derive(Debug)]
pub struct NativeOutputSnapshotsResource {
    outputs: BTreeSet<OutputSelector>,
    timeout: Duration,
    max_readers: usize,
}

impl NativeOutputSnapshotsResource {
    pub fn new(
        outputs: impl IntoIterator<Item = OutputSelector>,
        timeout: Duration,
        max_readers: usize,
    ) -> anyhow::Result<Self> {
        let outputs = Self::validate_settings(outputs, timeout, max_readers)?;
        Ok(Self {
            outputs,
            timeout,
            max_readers,
        })
    }

    pub(crate) fn validate_settings(
        outputs: impl IntoIterator<Item = OutputSelector>,
        timeout: Duration,
        max_readers: usize,
    ) -> anyhow::Result<BTreeSet<OutputSelector>> {
        anyhow::ensure!(
            !timeout.is_zero() && std::time::Instant::now().checked_add(timeout).is_some(),
            "output snapshot timeout must be positive and representable"
        );
        anyhow::ensure!(
            (1..=1024).contains(&max_readers),
            "output snapshot maxReaders must be in 1..=1024"
        );
        let mut allowed = BTreeSet::new();
        for output in outputs {
            anyhow::ensure!(allowed.insert(output), "duplicate allowed snapshot output");
        }
        anyhow::ensure!(
            !allowed.is_empty() && allowed.len() <= 4096,
            "output snapshots require 1..=4096 allowed outputs"
        );
        Ok(allowed)
    }

    /// Whether `endpoint` is an allowed output. A selector without a port allows
    /// exactly the component's primary output as the catalogue resolves it now.
    fn permits(&self, catalogue: &OutputCatalogue, endpoint: &OutputEndpoint) -> bool {
        self.outputs.iter().any(|selector| {
            selector.component == endpoint.component
                && match &selector.port {
                    Some(port) => *port == endpoint.port,
                    None => catalogue
                        .resolve_endpoint(&selector.component, None)
                        .is_ok_and(|resolved| resolved == *endpoint),
                }
        })
    }
}

/// The immutable policy plus the actual catalogue of the constructing graph.
pub struct OutputSnapshotsService {
    pub resource: Arc<NativeOutputSnapshotsResource>,
    pub catalogue: OutputCatalogue,
}

struct Epoch {
    active: bool,
    cancelled: CancellationToken,
    runtime: Option<tokio::runtime::Handle>,
}

struct State {
    resource: Weak<NativeOutputSnapshotsResource>,
    catalogue: OutputCatalogue,
    epoch: Mutex<Epoch>,
    readers: AtomicUsize,
}

pub(super) struct OutputSnapshotsBinding {
    _resource: Arc<NativeOutputSnapshotsResource>,
    state: Arc<State>,
}

impl OutputSnapshotsBinding {
    /// `catalogue` is the actual catalogue of the graph that constructs the
    /// component.
    pub(super) fn new(
        resource: Arc<NativeOutputSnapshotsResource>,
        catalogue: OutputCatalogue,
    ) -> Self {
        Self {
            state: Arc::new(State {
                resource: Arc::downgrade(&resource),
                catalogue,
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
            anyhow::anyhow!("output snapshots need the owning host runtime: {error}")
        })?;
        let mut epoch = self
            .state
            .epoch
            .lock()
            .map_err(|_| anyhow::anyhow!("output-snapshot lifecycle poisoned"))?;
        anyhow::ensure!(!epoch.active, "output snapshots are already active");
        epoch.cancelled = CancellationToken::new();
        epoch.runtime = Some(runtime);
        epoch.active = true;
        Ok(())
    }

    pub(super) fn deactivate(&self) {
        let mut epoch = self.state.epoch.lock().unwrap_or_else(|error| {
            log::error!("Closing poisoned output-snapshot lifecycle");
            error.into_inner()
        });
        epoch.active = false;
        epoch.cancelled.cancel();
        epoch.runtime = None;
    }

    pub(super) fn ensure_quiesced(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.state.readers.load(Ordering::Acquire) == 0,
            "output-snapshot readers still require cleanup"
        );
        Ok(())
    }

    pub(super) fn table(&self) -> OutputSnapshotsV1 {
        OutputSnapshotsV1 {
            header: abi::Header::new::<OutputSnapshotsV1>(),
            context: Arc::as_ptr(&self.state) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            open: Some(open),
        }
    }
}

impl Drop for OutputSnapshotsBinding {
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
    resource: Arc<NativeOutputSnapshotsResource>,
    catalogue: OutputCatalogue,
    endpoint: OutputEndpoint,
    cancelled: CancellationToken,
    runtime: tokio::runtime::Handle,
    busy: AtomicBool,
    failed: AtomicBool,
    records: Mutex<Option<std::vec::IntoIter<Record>>>,
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
    request: abi::BorrowedBytes,
    out: *mut OutputSnapshotHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol(
                "null output-snapshot capability or output",
            ));
        }
        let state = unsafe { &*context.cast::<State>() };
        let epoch = state
            .epoch
            .lock()
            .map_err(|_| Failure::failed("output-snapshot lifecycle poisoned"))?;
        if !epoch.active {
            return Err(Failure::closed());
        }
        let resource = state.resource.upgrade().ok_or_else(Failure::closed)?;
        let runtime = epoch.runtime.clone().ok_or_else(Failure::closed)?;
        let bytes = unsafe { transport::borrowed_bytes(request, MAX_OPEN_BYTES)? };
        let open: SnapshotOpen =
            wire::decode(bytes).map_err(|error| Failure::protocol(error.to_string()))?;
        let endpoint = open
            .endpoint()
            .map_err(|error| Failure::protocol(error.to_string()))?;
        if !resource.permits(&state.catalogue, &endpoint) {
            return Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "output is outside the snapshot capability",
            ));
        }
        state
            .readers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < resource.max_readers).then_some(count + 1)
            })
            .map_err(|_| Failure::new(abi::status::BUSY, "output-snapshot reader limit reached"))?;
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let permit = ReaderPermit(unsafe { Arc::from_raw(context.cast::<State>()) });
        let reader = Arc::new(Reader {
            resource,
            catalogue: state.catalogue.clone(),
            endpoint,
            cancelled: epoch.cancelled.child_token(),
            runtime,
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            records: Mutex::new(None),
            _permit: permit,
        });
        unsafe {
            transport::write_out(
                out,
                OutputSnapshotHandle {
                    state: Box::into_raw(Box::new(reader)).cast(),
                    vtable: &READER,
                },
            )
        }
    })
}

static READER: OutputSnapshotVTable = OutputSnapshotVTable {
    header: abi::Header::new::<OutputSnapshotVTable>(),
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
            return Err(Failure::protocol("null output-snapshot reader or output"));
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
            return Err(Failure::failed("output-snapshot reader has failed"));
        }
        if reader.busy.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                abi::status::BUSY,
                "output-snapshot operation is pending",
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

/// A typed read outcome travels in a successful reply so the plugin can tell an
/// absent capability or a not-ready output from a transport failure.
fn outcome<T: serde::Serialize>(result: Result<T, OutputReadError>) -> Result<Vec<u8>, Failure> {
    wire::encode(&Outcome::from_result(result)).map_err(Failure::from)
}

async fn read(reader: &Reader, code: u32) -> Result<Vec<u8>, Failure> {
    if code == operation::PREPARE {
        if reader
            .records
            .lock()
            .map_err(|_| Failure::failed("snapshot records poisoned"))?
            .is_some()
        {
            return Err(Failure::protocol("output snapshot is already prepared"));
        }
        // A primary selector follows the graph's current primary output, so the
        // scope is checked again at the read, not only when the reader opened.
        if !reader.resource.permits(&reader.catalogue, &reader.endpoint) {
            return Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "output is outside the snapshot capability",
            ));
        }
        let snapshot = match capture(reader).await {
            Ok(snapshot) => snapshot,
            Err(error) => return outcome::<OutputSnapshotHeader>(Err(error)),
        };
        let mut records: Vec<Record> = snapshot.records().values().cloned().collect();
        records.sort_by(|left, right| {
            (left.identity().namespace(), left.identity().value())
                .cmp(&(right.identity().namespace(), right.identity().value()))
        });
        let header = OutputSnapshotHeader {
            component: reader.endpoint.component.to_string(),
            port: reader.endpoint.port.to_string(),
            cursor: WireCursor::new(snapshot.cursor()),
            dataset: snapshot.dataset().to_owned(),
            schema: snapshot.schema().clone(),
        };
        let response = outcome(Ok(header))?;
        *reader
            .records
            .lock()
            .map_err(|_| Failure::failed("snapshot records poisoned"))? = Some(records.into_iter());
        return Ok(response);
    }
    let next = reader
        .records
        .lock()
        .map_err(|_| Failure::failed("snapshot records poisoned"))?
        .as_mut()
        .ok_or_else(|| Failure::protocol("snapshot was not prepared"))?
        .next();
    let wire_record = next.as_ref().map(|record| WireRecord {
        namespace: Cow::Borrowed(record.identity().namespace()),
        identity: Cow::Borrowed(&record.identity().value()[..]),
        image: 0,
        payload: Cow::Borrowed(&record.payload()[..]),
    });
    wire::encode(&wire_record).map_err(Failure::from)
}

async fn capture(
    reader: &Reader,
) -> Result<drasi_lib::computation::v1::OutputSnapshot, OutputReadError> {
    let handle = reader
        .catalogue
        .resolve_component(&reader.endpoint.component, Some(&reader.endpoint.port))?;
    match tokio::time::timeout(reader.resource.timeout, handle.snapshot()).await {
        Ok(result) => result,
        Err(_) => Err(OutputReadError::NotReady(format!(
            "snapshot of {} timed out",
            reader.endpoint
        ))),
    }
}
