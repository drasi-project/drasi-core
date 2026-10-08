// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-scoped native bootstrap. Factories construct providers without I/O;
//! the query owns preparation, snapshot polling, final commit and cleanup.

use crate::{
    abi::{self, bootstrap::*},
    metadata::ConfigSchema,
    transaction::RetainedTransaction,
    transport::{self, Failure},
    wire::{self, CreateRequest},
    NativeSourceProgress,
};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::StorageDurability;
use drasi_lib::computation::v1::{
    decode_bounded_messagepack, encode_bounded_messagepack, BinaryEnvelopeCodec,
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentId,
    ComputationBootstrapProvider, ComputationBootstrapSnapshot, ImplementationIdentity,
    PluginIdentity, SourceProgressReader, StreamId,
};
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    future::poll_fn,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::Poll,
};

pub type StateBytes = serde_bytes::ByteBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFactoryMetadata {
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: ConfigSchema,
    pub source_progress: bool,
}
impl BootstrapFactoryMetadata {
    pub fn validate(&self, plugin: &PluginIdentity) -> anyhow::Result<()> {
        ComponentId::try_new(self.implementation.name.as_ref())?;
        ComponentId::try_new(self.implementation.version.as_ref())?;
        anyhow::ensure!(
            self.implementation.plugin.as_ref() == Some(plugin),
            "bootstrap factory plugin provenance mismatch"
        );
        anyhow::ensure!(
            self.configuration_version > 0,
            "invalid bootstrap configuration version"
        );
        for name in self.configuration.fields.keys() {
            ComponentId::try_new(name.as_str())?;
        }
        Ok(())
    }
    pub fn validate_request(&self, request: &CreateRequest) -> anyhow::Result<()> {
        anyhow::ensure!(
            request.implementation == self.implementation
                && request.configuration_version == self.configuration_version,
            "bootstrap implementation/configuration version mismatch"
        );
        if let Some(scope) = &request.scope {
            ComponentId::try_new(scope.graph_id.as_str())?;
            ComponentId::try_new(scope.instance_id.as_str())?;
        }
        self.configuration.validate(&request.configuration)
    }
}

pub trait BootstrapFactory: Send + Sync + 'static {
    fn metadata(&self) -> BootstrapFactoryMetadata;
    /// Configuration only. A progress binding, when declared, is the actual
    /// query-owned reader; it is not a private checkpoint store.
    fn create(
        &self,
        request: &CreateRequest,
        progress: Option<NativeSourceProgress>,
    ) -> anyhow::Result<Arc<dyn ComputationBootstrapProvider>>;
}

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(encode_bounded_messagepack(value, MAX_CONTROL_BYTES)?)
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> anyhow::Result<T> {
    Ok(decode_bounded_messagepack(bytes, MAX_CONTROL_BYTES)?)
}
pub fn validate_state(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_STATE_BYTES,
        "native bootstrap state must contain 1..={MAX_STATE_BYTES} bytes"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub preparation: Preparation,
    pub pending: bool,
}
#[derive(Serialize, Deserialize)]
pub enum Preparation {
    Ready,
    RefreshVolatile,
    ResetRequired,
}
impl From<BootstrapPreparation> for Preparation {
    fn from(value: BootstrapPreparation) -> Self {
        match value {
            BootstrapPreparation::Ready => Self::Ready,
            BootstrapPreparation::RefreshVolatile => Self::RefreshVolatile,
            BootstrapPreparation::ResetRequired => Self::ResetRequired,
        }
    }
}
impl From<Preparation> for BootstrapPreparation {
    fn from(value: Preparation) -> Self {
        match value {
            Preparation::Ready => Self::Ready,
            Preparation::RefreshVolatile => Self::RefreshVolatile,
            Preparation::ResetRequired => Self::ResetRequired,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watermark {
    pub stream: StreamId,
    pub source_id: Option<String>,
    pub sequence: u64,
    pub position: Option<serde_bytes::ByteBuf>,
}
impl From<BootstrapWatermark> for Watermark {
    fn from(value: BootstrapWatermark) -> Self {
        Self {
            stream: value.stream,
            source_id: value.source_id,
            sequence: value.sequence,
            position: value.position.map(|bytes| bytes.to_vec().into()),
        }
    }
}
impl From<Watermark> for BootstrapWatermark {
    fn from(value: Watermark) -> Self {
        Self {
            stream: value.stream,
            source_id: value.source_id,
            sequence: value.sequence,
            position: value.position.map(|bytes| bytes.into_vec().into()),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completed {
    pub watermarks: Vec<Watermark>,
    pub state: Option<serde_bytes::ByteBuf>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStarted {
    pub generation: u64,
    pub watermarks: Vec<Watermark>,
}
pub fn validate_watermarks(values: &[Watermark]) -> anyhow::Result<()> {
    anyhow::ensure!(
        values.len() <= MAX_WATERMARKS,
        "native bootstrap watermark limit exceeded"
    );
    let mut keys = std::collections::BTreeSet::new();
    for value in values {
        let key = value.source_id.as_deref().unwrap_or(value.stream.as_str());
        anyhow::ensure!(
            !key.is_empty() && keys.insert((value.source_id.is_some(), key)),
            "duplicate or empty bootstrap watermark"
        );
    }
    Ok(())
}

struct RemoteState {
    requests: RetainedTransaction,
    durability: StorageDurability,
}
impl RemoteState {
    unsafe fn new(raw: *const BootstrapStateV1) -> anyhow::Result<Self> {
        let raw = unsafe { transport::checked_table(raw)? };
        anyhow::ensure!(raw.reserved == 0, "invalid bootstrap state reserved field");
        let durability =
            decode(unsafe { transport::borrowed_bytes(raw.durability, MAX_CONTROL_BYTES)? })?;
        Ok(Self {
            requests: unsafe { RetainedTransaction::new(&raw.requests)? },
            durability,
        })
    }
}
#[async_trait]
impl BootstrapState for RemoteState {
    fn durability(&self) -> StorageDurability {
        self.durability
    }
    async fn read(&self) -> anyhow::Result<Option<Bytes>> {
        let state: Option<serde_bytes::ByteBuf> = self.requests.call(READ_STATE, &()).await?;
        state
            .map(|bytes| {
                validate_state(&bytes)?;
                Ok(bytes.into_vec().into())
            })
            .transpose()
    }
    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        validate_state(&state)?;
        self.requests
            .call(WRITE_STATE, &serde_bytes::Bytes::new(&state))
            .await
    }
}

type SnapshotStream =
    Pin<Box<dyn Stream<Item = anyhow::Result<drasi_lib::computation::v1::ChangeEnvelope>> + Send>>;
struct SnapshotSlot {
    generation: u64,
    stream: Option<SnapshotStream>,
    waker: Option<std::task::Waker>,
}
struct Instance {
    provider: Arc<dyn ComputationBootstrapProvider>,
    codec: Arc<BinaryEnvelopeCodec>,
    stream: Mutex<SnapshotSlot>,
    busy: AtomicBool,
    cleanup_required: AtomicBool,
    prepared: AtomicBool,
    started: AtomicBool,
    ended: AtomicBool,
    damaged: AtomicBool,
    pending: AtomicBool,
    reader: Option<SourceProgressReader>,
}
impl Instance {
    fn validate_reader(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            match (&self.reader, self.provider.recovery_reader()) {
                (None, None) => true,
                (Some(expected), Some(actual)) => expected.same_owner(&actual),
                _ => false,
            },
            "native bootstrap changed its recovery reader"
        );
        Ok(())
    }
    fn cancel(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.stream.lock().unwrap_or_else(|error| {
            self.damaged.store(true, Ordering::Release);
            error.into_inner()
        });
        if generation != 0 && generation != slot.generation {
            return Err(Failure::closed().into());
        }
        self.cleanup_required.store(true, Ordering::Release);
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native bootstrap generation exhausted"))?;
        let stream = slot.stream.take();
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
        drop(stream);
        Ok(())
    }
    async fn call(
        &self,
        code: u32,
        generation: u64,
        state: Option<RemoteState>,
    ) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            code == STOP
                || (!self.cleanup_required.load(Ordering::Acquire)
                    && !self.damaged.load(Ordering::Acquire)),
            "native bootstrap requires successful cleanup before reuse"
        );
        match code {
            PREPARE => {
                anyhow::ensure!(
                    !self.prepared.load(Ordering::Acquire),
                    "bootstrap already prepared"
                );
                self.validate_reader()?;
                let preparation = match state {
                    Some(state) => self.provider.prepare_with_state(&state).await?,
                    None => self.provider.prepare().await?,
                };
                self.validate_reader()?;
                let pending = self.provider.has_pending_snapshot()?;
                self.pending.store(pending, Ordering::Release);
                self.prepared.store(true, Ordering::Release);
                encode(&Prepared {
                    preparation: preparation.into(),
                    pending,
                })
            }
            SNAPSHOT => {
                anyhow::ensure!(
                    self.prepared.load(Ordering::Acquire)
                        && !self.started.load(Ordering::Acquire)
                        && self.pending.load(Ordering::Acquire),
                    "bootstrap snapshot must follow preparation exactly once"
                );
                let ComputationBootstrapSnapshot {
                    changes,
                    watermarks,
                } = match state {
                    Some(state) => self.provider.snapshot_with_state(&state).await?,
                    None => self.provider.snapshot().await?,
                };
                let watermarks: Vec<Watermark> = watermarks.into_iter().map(Into::into).collect();
                validate_watermarks(&watermarks)?;
                let mut slot = self
                    .stream
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native bootstrap stream poisoned"))?;
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("native bootstrap generation exhausted"))?;
                let response = encode(&SnapshotStarted {
                    generation: slot.generation,
                    watermarks,
                })?;
                slot.stream = Some(changes);
                self.started.store(true, Ordering::Release);
                Ok(response)
            }
            NEXT => {
                let next = poll_fn(|cx| {
                    let mut stream = match self.stream.lock() {
                        Ok(stream) => stream,
                        Err(_) => {
                            return Poll::Ready(Err(anyhow::anyhow!(
                                "native bootstrap stream poisoned"
                            )))
                        }
                    };
                    if generation == 0 || generation != stream.generation {
                        return Poll::Ready(Err(Failure::closed().into()));
                    }
                    let Some(value) = stream.stream.as_mut() else {
                        return Poll::Ready(Err(anyhow::anyhow!(
                            "no active native bootstrap stream"
                        )));
                    };
                    match value.as_mut().poll_next(cx) {
                        Poll::Pending => {
                            stream.waker = Some(cx.waker().clone());
                            Poll::Pending
                        }
                        Poll::Ready(Some(value)) => Poll::Ready(value.map(Some)),
                        Poll::Ready(None) => {
                            stream.stream.take();
                            stream.waker = None;
                            self.ended.store(true, Ordering::Release);
                            Poll::Ready(Ok(None))
                        }
                    }
                })
                .await?;
                let encoded = next
                    .map(|envelope| self.codec.encode(&envelope).map(serde_bytes::ByteBuf::from))
                    .transpose()?;
                wire::encode(&encoded)
            }
            COMPLETE => {
                anyhow::ensure!(
                    self.ended.load(Ordering::Acquire),
                    "bootstrap stream has not completed"
                );
                let watermarks: Vec<Watermark> = self
                    .provider
                    .complete_snapshot()
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect();
                validate_watermarks(&watermarks)?;
                let state = self.provider.completion_state()?;
                if let Some(state) = &state {
                    validate_state(state)?;
                }
                let response = encode(&Completed {
                    watermarks,
                    state: state.map(|bytes| bytes.to_vec().into()),
                })?;
                self.ended.store(false, Ordering::Release);
                Ok(response)
            }
            STOP => {
                self.cancel(0)?;
                self.provider.stop().await?;
                self.prepared.store(false, Ordering::Release);
                self.started.store(false, Ordering::Release);
                self.ended.store(false, Ordering::Release);
                self.cleanup_required.store(false, Ordering::Release);
                encode(&())
            }
            _ => anyhow::bail!("unsupported native bootstrap operation"),
        }
    }
}

struct OperationGuard {
    instance: Arc<Instance>,
    completed: bool,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.instance.busy);
        if !self.completed {
            // Poisoning prevents reuse even if dropping an ill-behaved stream panics.
            self.instance
                .cleanup_required
                .store(true, Ordering::Release);
            if self.instance.cancel(0).is_err() {
                self.instance.damaged.store(true, Ordering::Release);
            }
        }
    }
}

pub(crate) fn export_provider(
    provider: Arc<dyn ComputationBootstrapProvider>,
    codec: Arc<BinaryEnvelopeCodec>,
    reader: Option<SourceProgressReader>,
) -> BootstrapHandle {
    BootstrapHandle {
        state: Box::into_raw(Box::new(Arc::new(Instance {
            provider,
            codec,
            stream: Mutex::new(SnapshotSlot {
                generation: 0,
                stream: None,
                waker: None,
            }),
            busy: AtomicBool::new(false),
            cleanup_required: AtomicBool::new(false),
            prepared: AtomicBool::new(false),
            started: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            damaged: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            reader,
        })))
        .cast(),
        vtable: &VTABLE,
    }
}
unsafe fn instance<'a>(state: *mut c_void) -> Result<&'a Arc<Instance>, Failure> {
    if state.is_null() {
        return Err(Failure::protocol("null native bootstrap handle"));
    }
    Ok(unsafe { &*state.cast::<Arc<Instance>>() })
}
static VTABLE: BootstrapVTable = BootstrapVTable {
    header: abi::Header::new::<BootstrapVTable>(),
    release: Some(release),
    begin: Some(begin),
    cancel_snapshot: Some(cancel),
};
unsafe extern "C" fn release(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<Instance>>())) });
}
unsafe extern "C" fn cancel(state: *mut c_void, generation: u64) -> abi::Status {
    transport::status_boundary(|| {
        unsafe { instance(state)? }
            .cancel(generation)
            .map_err(Failure::from)
    })
}
unsafe extern "C" fn begin(
    raw: *mut c_void,
    code: u32,
    generation: u64,
    state: *const BootstrapStateV1,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null()
            || !(PREPARE..=STOP).contains(&code)
            || (!state.is_null() && code != PREPARE && code != SNAPSHOT)
            || (code != NEXT && generation != 0)
        {
            return Err(Failure::protocol("invalid bootstrap operation arguments"));
        }

        let state = if state.is_null() {
            None
        } else {
            Some(unsafe { RemoteState::new(state) }.map_err(Failure::from)?)
        };
        let instance = unsafe { instance(raw)? }.clone();
        if code == NEXT {
            let slot = instance
                .stream
                .lock()
                .map_err(|_| Failure::failed("native bootstrap stream poisoned"))?;
            if generation == 0 || generation != slot.generation || slot.stream.is_none() {
                return Err(Failure::closed());
            }
        }
        instance
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                Failure::new(
                    abi::status::BUSY,
                    "native bootstrap operation already active",
                )
            })?;
        let mut guard = OperationGuard {
            instance,
            completed: false,
        };
        let runtime = crate::export::io_runtime()?;
        let operation = transport::export_operation(
            async move {
                let result = guard
                    .instance
                    .call(code, generation, state)
                    .await
                    .map_err(Failure::from);
                let result = result.and_then(|bytes| {
                    if code != STOP && guard.instance.cleanup_required.load(Ordering::Acquire) {
                        Err(Failure::cancelled())
                    } else {
                        Ok(bytes)
                    }
                });
                guard.completed = result.is_ok();
                result
            },
            Some(runtime),
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_wire_bounds_and_distinct_checkpoint_namespaces() -> anyhow::Result<()> {
        assert!(validate_state(&[]).is_err());
        validate_state(&vec![255; MAX_STATE_BYTES])?;
        assert!(validate_state(&vec![0; MAX_STATE_BYTES + 1]).is_err());
        let make = |source_id| Watermark {
            stream: StreamId::try_new("same").unwrap(),
            source_id,
            sequence: u64::MAX,
            position: Some(vec![0, 255].into()),
        };
        let values = vec![make(None), make(Some("same".into()))];
        validate_watermarks(&values)?;
        let values: Vec<Watermark> = decode(&encode(&values)?)?;
        assert_eq!(values[1].sequence, u64::MAX);
        assert_eq!(
            values[0].position.as_ref().map(|bytes| bytes.as_slice()),
            Some([0, 255].as_slice())
        );
        assert!(validate_watermarks(&[make(None), make(None)]).is_err());
        assert!(encode(&vec![255u8; MAX_CONTROL_BYTES]).is_err());
        assert!(decode::<Prepared>(&[0xc1]).is_err());
        Ok(())
    }
}
