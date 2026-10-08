// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use drasi_core::interface::{SourceCheckpoint, StorageDurability};
use drasi_lib::computation::v1::{
    decode_bounded_messagepack, encode_bounded_messagepack, ComponentId, SourceProgressKey,
    SourceProgressProvider, SourceProgressReader, SourceProgressSnapshot, SourceProgressUpdates,
    StreamId,
};
use serde::{Deserialize, Serialize};

use crate::{
    abi::{self, recovery::*},
    transport::{checked_table, take_reply, take_status, Failure, OperationFuture},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryRecovery {
    pub version: u32,
    pub source_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceRecovery {
    pub version: u32,
    pub retention: Option<StorageDurability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressIdentity {
    pub version: u32,
    pub graph_id: String,
    pub component_id: ComponentId,
}

impl ProgressIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == VERSION,
            "unsupported source progress version"
        );
        ComponentId::try_new(self.graph_id.as_str())?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
enum Key {
    Source(String),
    Stream(StreamId),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    key: Key,
    sequence: u64,
    position: Option<serde_bytes::ByteBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    ready: bool,
    admitting: bool,
    recovered: bool,
    bootstrap_complete: bool,
    persistent: bool,
    reset_generation: u64,
    checkpoints: Vec<Checkpoint>,
    transport_sequences: Vec<(StreamId, u64)>,
    failure: Option<String>,
}

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(encode_bounded_messagepack(value, MAX_PROGRESS_BYTES)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> anyhow::Result<T> {
    Ok(decode_bounded_messagepack(bytes, MAX_PROGRESS_BYTES)?)
}

pub fn encode_snapshot(snapshot: &SourceProgressSnapshot) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        snapshot.checkpoints.len() + snapshot.transport_sequences.len() <= MAX_PROGRESS_ENTRIES,
        "source progress entry limit exceeded"
    );
    // Bound copies before allocating the wire frame. The encoder separately
    // enforces the exact serialized-byte limit, including field overhead.
    let mut bytes = snapshot.failure.as_ref().map_or(0, |failure| failure.len());
    for (key, checkpoint) in &snapshot.checkpoints {
        let key = match key {
            SourceProgressKey::Source(source) => source.as_str(),
            SourceProgressKey::Stream(stream) => stream.as_str(),
        };
        bytes = bytes
            .checked_add(key.len())
            .and_then(|bytes| {
                bytes.checked_add(checkpoint.source_position.as_ref().map_or(0, |p| p.len()))
            })
            .ok_or_else(|| anyhow::anyhow!("source progress byte count overflow"))?;
        anyhow::ensure!(
            bytes <= MAX_PROGRESS_BYTES,
            "source progress byte limit exceeded"
        );
    }
    for stream in snapshot.transport_sequences.keys() {
        bytes = bytes
            .checked_add(stream.as_str().len())
            .ok_or_else(|| anyhow::anyhow!("source progress byte count overflow"))?;
    }
    anyhow::ensure!(
        bytes <= MAX_PROGRESS_BYTES,
        "source progress byte limit exceeded"
    );
    encode(&Snapshot {
        version: VERSION,
        ready: snapshot.ready,
        admitting: snapshot.admitting,
        recovered: snapshot.recovered,
        bootstrap_complete: snapshot.bootstrap_complete,
        persistent: snapshot.persistent,
        reset_generation: snapshot.reset_generation,
        checkpoints: snapshot
            .checkpoints
            .iter()
            .map(|(key, checkpoint)| Checkpoint {
                key: match key {
                    SourceProgressKey::Source(source) => Key::Source(source.clone()),
                    SourceProgressKey::Stream(stream) => Key::Stream(stream.clone()),
                },
                sequence: checkpoint.sequence,
                position: checkpoint
                    .source_position
                    .as_ref()
                    .map(|position| serde_bytes::ByteBuf::from(position.to_vec())),
            })
            .collect(),
        transport_sequences: snapshot
            .transport_sequences
            .iter()
            .map(|(stream, sequence)| (stream.clone(), *sequence))
            .collect(),
        failure: snapshot.failure.as_ref().map(|failure| failure.to_string()),
    })
}

pub fn decode_snapshot(bytes: &[u8]) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
    let snapshot: Snapshot = decode(bytes)?;
    anyhow::ensure!(
        snapshot.version == VERSION,
        "unsupported source progress version"
    );
    anyhow::ensure!(
        snapshot.checkpoints.len() + snapshot.transport_sequences.len() <= MAX_PROGRESS_ENTRIES,
        "source progress entry limit exceeded"
    );
    let mut checkpoints = BTreeMap::new();
    for checkpoint in snapshot.checkpoints {
        let key = match checkpoint.key {
            Key::Source(source) => {
                ComponentId::try_new(source.as_str())?;
                SourceProgressKey::Source(source)
            }
            Key::Stream(stream) => SourceProgressKey::Stream(stream),
        };
        anyhow::ensure!(
            checkpoints
                .insert(
                    key,
                    SourceCheckpoint::new(
                        checkpoint.sequence,
                        checkpoint
                            .position
                            .map(|position| position.into_vec().into()),
                    ),
                )
                .is_none(),
            "duplicate source progress checkpoint"
        );
    }
    let mut transport_sequences = BTreeMap::new();
    for (stream, sequence) in snapshot.transport_sequences {
        anyhow::ensure!(
            transport_sequences.insert(stream, sequence).is_none(),
            "duplicate source progress transport sequence"
        );
    }
    Ok(Arc::new(SourceProgressSnapshot {
        ready: snapshot.ready,
        admitting: snapshot.admitting,
        recovered: snapshot.recovered,
        bootstrap_complete: snapshot.bootstrap_complete,
        persistent: snapshot.persistent,
        reset_generation: snapshot.reset_generation,
        checkpoints,
        transport_sequences,
        failure: snapshot.failure.map(Into::into),
    }))
}

struct Provider {
    raw: SourceProgressV1,
    identity: ProgressIdentity,
}
// Only producer callbacks dereference opaque host state; the contract permits
// independent reads/subscriptions and cross-thread release.
unsafe impl Send for Provider {}
unsafe impl Sync for Provider {}
impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { self.raw.release.expect("validated")(self.raw.context) };
    }
}

#[derive(Clone)]
pub struct NativeSourceProgress(Arc<Provider>);

impl NativeSourceProgress {
    /// # Safety
    /// The producer must obey recovery-v1 and remain callable until release.
    pub unsafe fn from_borrowed(raw: *const SourceProgressV1) -> anyhow::Result<Self> {
        let raw = *unsafe { checked_table(raw)? };
        anyhow::ensure!(
            !raw.context.is_null()
                && raw.retain.is_some()
                && raw.release.is_some()
                && raw.describe.is_some()
                && raw.snapshot.is_some()
                && raw.subscribe.is_some(),
            "incomplete native source progress capability"
        );
        let description = unsafe { take_reply(raw.describe.expect("validated")(raw.context))? };
        let identity: ProgressIdentity = decode(&description)?;
        identity.validate()?;
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(Arc::new(Provider { raw, identity })))
    }

    pub fn reader(&self) -> SourceProgressReader {
        SourceProgressReader::External(self.0.clone())
    }
}

impl SourceProgressProvider for Provider {
    fn graph_id(&self) -> &str {
        &self.identity.graph_id
    }
    fn component_id(&self) -> &ComponentId {
        &self.identity.component_id
    }
    fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let bytes = unsafe { take_reply(self.raw.snapshot.expect("validated")(self.raw.context))? };
        decode_snapshot(&bytes)
    }
    fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
        let mut raw = ProgressSubscriptionV1::null();
        unsafe {
            take_status(self.raw.subscribe.expect("validated")(
                self.raw.context,
                &mut raw,
            ))?;
        }
        raw.header.validate::<ProgressSubscriptionV1>()?;
        anyhow::ensure!(
            !raw.context.is_null() && raw.release.is_some(),
            "invalid native progress subscription ownership"
        );
        let subscription = Subscription(raw);
        anyhow::ensure!(
            raw.snapshot.is_some() && raw.wait.is_some(),
            "incomplete native progress subscription"
        );
        Ok(Box::new(subscription))
    }
}

struct Subscription(ProgressSubscriptionV1);
unsafe impl Send for Subscription {}
unsafe impl Sync for Subscription {}
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.context) };
        }
    }
}

#[async_trait]
impl SourceProgressUpdates for Subscription {
    fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let bytes = unsafe { take_reply(self.0.snapshot.expect("validated")(self.0.context))? };
        decode_snapshot(&bytes)
    }

    async fn changed(&mut self) -> anyhow::Result<()> {
        let operation = {
            let mut operation = abi::OperationHandle::null();
            unsafe {
                take_status(self.0.wait.expect("validated")(
                    self.0.context,
                    &mut operation,
                ))?;
                OperationFuture::new(operation)?
            }
        };
        let bytes = operation.await?;
        if !bytes.is_empty() {
            return Err(Failure::protocol("progress wait returned unexpected data").into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> SourceProgressSnapshot {
        SourceProgressSnapshot {
            ready: true,
            admitting: true,
            recovered: true,
            bootstrap_complete: true,
            persistent: true,
            reset_generation: u64::MAX,
            checkpoints: BTreeMap::from([
                (
                    SourceProgressKey::Source("source".into()),
                    SourceCheckpoint::new(0, Some(vec![0, 255, 128].into())),
                ),
                (
                    SourceProgressKey::Stream(StreamId::try_new("source").unwrap()),
                    SourceCheckpoint::new(u64::MAX, None),
                ),
            ]),
            transport_sequences: BTreeMap::from([(
                StreamId::try_new("source/out").unwrap(),
                u64::MAX,
            )]),
            failure: Some("fenced".into()),
        }
    }

    #[test]
    fn progress_wire_preserves_flags_keys_positions_and_unsigned_sequences() -> anyhow::Result<()> {
        let original = state();
        let bytes = encode_snapshot(&original)?;
        let decoded = decode_snapshot(&bytes)?;
        assert!(decoded.ready && decoded.admitting && decoded.recovered);
        assert!(decoded.bootstrap_complete && decoded.persistent);
        assert_eq!(decoded.reset_generation, u64::MAX);
        assert_eq!(decoded.failure.as_deref(), Some("fenced"));
        assert_eq!(decoded.checkpoints, original.checkpoints);
        assert_eq!(decoded.transport_sequences, original.transport_sequences);
        assert_eq!(encode_snapshot(&decoded)?, bytes);
        Ok(())
    }

    #[test]
    fn progress_wire_rejects_duplicate_invalid_and_incompatible_state() -> anyhow::Result<()> {
        let bytes = encode_snapshot(&state())?;
        let mut snapshot: Snapshot = decode(&bytes)?;
        snapshot.version += 1;
        assert!(decode_snapshot(&encode(&snapshot)?).is_err());
        snapshot.version = VERSION;
        snapshot.checkpoints.push(Checkpoint {
            key: Key::Source("source".into()),
            sequence: 1,
            position: None,
        });
        assert!(decode_snapshot(&encode(&snapshot)?).is_err());
        snapshot.checkpoints.last_mut().unwrap().key = Key::Source("invalid source".into());
        assert!(decode_snapshot(&encode(&snapshot)?).is_err());
        snapshot.checkpoints.pop();
        snapshot
            .transport_sequences
            .push(snapshot.transport_sequences[0].clone());
        assert!(decode_snapshot(&encode(&snapshot)?).is_err());
        assert!(decode_snapshot(&[0xc1]).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_snapshot(&trailing).is_err());
        let mut identity = ProgressIdentity {
            version: VERSION,
            graph_id: "graph".into(),
            component_id: ComponentId::try_new("query")?,
        };
        identity.validate()?;
        identity.graph_id.clear();
        assert!(identity.validate().is_err());
        identity.graph_id = "graph".into();
        identity.version += 1;
        assert!(identity.validate().is_err());
        // Identifier newtypes validate on deserialization, including stream keys.
        assert!(decode::<StreamId>(&encode(&"invalid stream")?).is_err());
        assert!(decode::<ComponentId>(&encode(&"")?).is_err());
        Ok(())
    }

    #[test]
    fn progress_wire_enforces_entry_and_byte_limits_in_both_directions() -> anyhow::Result<()> {
        let mut snapshot = SourceProgressSnapshot {
            transport_sequences: (0..MAX_PROGRESS_ENTRIES)
                .map(|index| {
                    (
                        StreamId::try_new(format!("s{index}")).unwrap(),
                        index as u64,
                    )
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            decode_snapshot(&encode_snapshot(&snapshot)?)?
                .transport_sequences
                .len(),
            MAX_PROGRESS_ENTRIES
        );
        snapshot
            .transport_sequences
            .insert(StreamId::try_new("extra")?, 0);
        assert!(encode_snapshot(&snapshot).is_err());
        let wire = Snapshot {
            version: VERSION,
            ready: false,
            admitting: false,
            recovered: false,
            bootstrap_complete: false,
            persistent: false,
            reset_generation: 0,
            checkpoints: vec![],
            transport_sequences: snapshot.transport_sequences.into_iter().collect(),
            failure: None,
        };
        assert!(decode_snapshot(&encode(&wire)?).is_err());
        snapshot = state();
        snapshot.failure = Some("x".repeat(MAX_PROGRESS_BYTES).into());
        assert!(encode_snapshot(&snapshot).is_err());
        snapshot.failure = None;
        snapshot.checkpoints.insert(
            SourceProgressKey::Source("large".into()),
            SourceCheckpoint::new(0, Some(vec![0; MAX_PROGRESS_BYTES].into())),
        );
        assert!(encode_snapshot(&snapshot).is_err());
        assert!(decode_snapshot(&vec![0; MAX_PROGRESS_BYTES + 1]).is_err());
        Ok(())
    }

    struct Producer {
        releases: std::sync::atomic::AtomicUsize,
        subscription_releases: std::sync::atomic::AtomicUsize,
        mode: u8,
    }

    unsafe extern "C" fn retain(context: *mut std::ffi::c_void) {
        unsafe { Arc::increment_strong_count(context.cast::<Producer>()) };
    }
    unsafe extern "C" fn release(context: *mut std::ffi::c_void) {
        let owner = unsafe { Arc::from_raw(context.cast::<Producer>()) };
        owner
            .releases
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    unsafe extern "C" fn release_subscription(context: *mut std::ffi::c_void) {
        assert!(!context.is_null());
        let owner = unsafe { Arc::from_raw(context.cast::<Producer>()) };
        owner
            .subscription_releases
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    unsafe extern "C" fn describe(_: *mut std::ffi::c_void) -> abi::Reply {
        crate::transport::reply_boundary(|| {
            encode(&ProgressIdentity {
                version: VERSION,
                graph_id: "graph".into(),
                component_id: ComponentId::try_new("query").unwrap(),
            })
            .map_err(Failure::from)
        })
    }
    unsafe extern "C" fn snapshot(_: *mut std::ffi::c_void) -> abi::Reply {
        crate::transport::reply_boundary(|| encode_snapshot(&state()).map_err(Failure::from))
    }
    unsafe extern "C" fn wait(
        _: *mut std::ffi::c_void,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        crate::transport::status_boundary(|| unsafe {
            crate::transport::write_out(
                out,
                crate::transport::export_operation(async { Ok(vec![1]) }, None),
            )
        })
    }
    unsafe extern "C" fn subscribe(
        context: *mut std::ffi::c_void,
        out: *mut ProgressSubscriptionV1,
    ) -> abi::Status {
        crate::transport::status_boundary(|| {
            let owner = unsafe { &*context.cast::<Producer>() };
            let raw = ProgressSubscriptionV1 {
                header: abi::Header::new::<ProgressSubscriptionV1>(),
                context: if owner.mode == 0 {
                    std::ptr::null_mut()
                } else {
                    context
                },
                release: Some(release_subscription),
                snapshot: Some(snapshot),
                wait: (owner.mode != 1).then_some(wait),
            };
            if !raw.context.is_null() {
                unsafe { retain(context) };
            }
            unsafe { crate::transport::write_out(out, raw) }
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_tables_validate_ownership_and_release_malformed_subscriptions_once(
    ) -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for mode in 0..3 {
            let owner = Arc::new(Producer {
                releases: AtomicUsize::new(0),
                subscription_releases: AtomicUsize::new(0),
                mode,
            });
            let table = SourceProgressV1 {
                header: abi::Header::new::<SourceProgressV1>(),
                context: Arc::as_ptr(&owner) as *mut _,
                retain: Some(retain),
                release: Some(release),
                describe: Some(describe),
                snapshot: Some(snapshot),
                subscribe: Some(subscribe),
            };
            let mut invalid = table;
            invalid.retain = None;
            assert!(unsafe { NativeSourceProgress::from_borrowed(&invalid) }.is_err());
            assert_eq!(Arc::strong_count(&owner), 1);
            let progress = unsafe { NativeSourceProgress::from_borrowed(&table)? };
            assert_eq!(Arc::strong_count(&owner), 2);
            assert!(progress.reader().snapshot()?.ready);
            let subscription = progress.reader().subscribe();
            if mode == 2 {
                let mut subscription = subscription?;
                assert!(subscription
                    .changed()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("unexpected data"));
                drop(subscription);
            } else {
                assert!(subscription.is_err());
            }
            assert_eq!(
                owner.subscription_releases.load(Ordering::Relaxed),
                usize::from(mode != 0)
            );
            assert_eq!(Arc::strong_count(&owner), 2);
            drop(progress);
            assert_eq!(owner.releases.load(Ordering::Relaxed), 1);
            assert_eq!(Arc::strong_count(&owner), 1);
        }
        Ok(())
    }
}
