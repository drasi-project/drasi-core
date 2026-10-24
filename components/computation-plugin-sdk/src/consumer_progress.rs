// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use drasi_computation_contracts::{
    decode_bounded_messagepack, encode_bounded_messagepack, ComponentId, ConsumerProgressProvider,
    ConsumerProgressSnapshot, ConsumerProgressUpdates, ConsumerProgressView,
    SourceProgressSnapshot,
};
use drasi_core_types::interface::StorageDurability;
use serde::{Deserialize, Serialize};

use crate::{
    abi::{self, consumer_progress::*},
    progress,
    transport::{checked_table, take_reply, take_status, Failure, OperationFuture},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryProgress {
    pub version: u32,
    pub consumer_progress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceProgress {
    pub version: u32,
    pub retention: Option<StorageDurability>,
    pub position_policy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressIdentity {
    pub version: u32,
    pub graph_id: String,
    pub producer_id: ComponentId,
    pub consumers: Vec<ComponentId>,
}

impl ProgressIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == VERSION,
            "unsupported consumer progress version"
        );
        ComponentId::try_new(self.graph_id.as_str())?;
        anyhow::ensure!(
            !self.consumers.is_empty() && self.consumers.len() <= MAX_PROGRESS_MEMBERS,
            "consumer progress membership limit exceeded"
        );
        let mut members = BTreeSet::new();
        for consumer in &self.consumers {
            anyhow::ensure!(
                consumer != &self.producer_id && members.insert(consumer),
                "duplicate or self-referencing consumer progress member"
            );
        }
        Ok(())
    }

    fn validate_snapshot(&self, snapshot: &ConsumerProgressSnapshot) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            snapshot.consumers.len() == self.consumers.len()
                && self
                    .consumers
                    .iter()
                    .all(|id| snapshot.consumers.contains_key(id)),
            "consumer progress snapshot changed its bound membership"
        );
        let mut entries = 0;
        for state in snapshot.consumers.values() {
            entries = count_entries(entries, state)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member<'a> {
    id: ComponentId,
    #[serde(borrow, with = "serde_bytes")]
    snapshot: Cow<'a, [u8]>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot<'a> {
    version: u32,
    #[serde(borrow)]
    consumers: Vec<Member<'a>>,
}

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(encode_bounded_messagepack(value, MAX_PROGRESS_BYTES)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> anyhow::Result<T> {
    Ok(decode_bounded_messagepack(bytes, MAX_PROGRESS_BYTES)?)
}

fn count_entries(previous: usize, state: &SourceProgressSnapshot) -> anyhow::Result<usize> {
    let entries = previous
        .checked_add(state.checkpoints.len())
        .and_then(|count| count.checked_add(state.transport_sequences.len()))
        .ok_or_else(|| anyhow::anyhow!("consumer progress entry count overflow"))?;
    anyhow::ensure!(
        entries <= MAX_PROGRESS_ENTRIES,
        "consumer progress entry limit exceeded"
    );
    Ok(entries)
}

pub fn encode_snapshot(
    snapshot: &ConsumerProgressSnapshot,
    identity: &ProgressIdentity,
) -> anyhow::Result<Vec<u8>> {
    identity.validate_snapshot(snapshot)?;
    let mut consumers = Vec::with_capacity(snapshot.consumers.len());
    let mut bytes = 0_usize;
    for (id, state) in &snapshot.consumers {
        // Each existing source frame is bounded before copying positions. The
        // running budget also bounds all retained member frames together.
        let snapshot = progress::encode_snapshot(state)?;
        bytes = bytes
            .checked_add(id.as_str().len())
            .and_then(|bytes| bytes.checked_add(snapshot.len()))
            .ok_or_else(|| anyhow::anyhow!("consumer progress byte count overflow"))?;
        anyhow::ensure!(
            bytes <= MAX_PROGRESS_BYTES,
            "consumer progress byte limit exceeded"
        );
        consumers.push(Member {
            id: id.clone(),
            snapshot: Cow::Owned(snapshot),
        });
    }
    encode(&Snapshot {
        version: VERSION,
        consumers,
    })
}

pub fn decode_snapshot(
    bytes: &[u8],
    identity: &ProgressIdentity,
) -> anyhow::Result<ConsumerProgressSnapshot> {
    identity.validate()?;
    let snapshot: Snapshot<'_> = decode(bytes)?;
    anyhow::ensure!(
        snapshot.version == VERSION,
        "unsupported consumer progress version"
    );
    anyhow::ensure!(
        snapshot.consumers.len() == identity.consumers.len(),
        "consumer progress snapshot changed its bound membership"
    );
    let mut consumers = BTreeMap::new();
    let mut entries = 0;
    for member in snapshot.consumers {
        anyhow::ensure!(
            identity.consumers.contains(&member.id) && !consumers.contains_key(&member.id),
            "unknown or duplicate consumer progress member"
        );
        let state = progress::decode_snapshot(&member.snapshot)?;
        entries = count_entries(entries, &state)?;
        consumers.insert(member.id, state);
    }
    Ok(ConsumerProgressSnapshot { consumers })
}

struct Provider {
    raw: ConsumerProgressV1,
    identity: Arc<ProgressIdentity>,
}
// Only producer callbacks access opaque host state, including cross-thread
// release; no Rust storage owner or runtime object crosses the boundary.
unsafe impl Send for Provider {}
unsafe impl Sync for Provider {}
impl Drop for Provider {
    fn drop(&mut self) {
        unsafe { self.raw.release.expect("validated")(self.raw.context) };
    }
}

#[derive(Clone)]
pub struct NativeConsumerProgress(Arc<Provider>);

impl NativeConsumerProgress {
    /// # Safety
    /// The producer must obey consumer-progress-v1 and remain callable until
    /// release. The table is borrowed for this call; its context is retained.
    pub unsafe fn from_borrowed(raw: *const ConsumerProgressV1) -> anyhow::Result<Self> {
        let raw = *unsafe { checked_table(raw)? };
        anyhow::ensure!(
            !raw.context.is_null()
                && raw.retain.is_some()
                && raw.release.is_some()
                && raw.describe.is_some()
                && raw.snapshot.is_some()
                && raw.subscribe.is_some(),
            "incomplete native consumer progress capability"
        );
        let description = unsafe { take_reply(raw.describe.expect("validated")(raw.context))? };
        let identity: ProgressIdentity = decode(&description)?;
        identity.validate()?;
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(Arc::new(Provider {
            raw,
            identity: Arc::new(identity),
        })))
    }

    pub fn reader(&self) -> ConsumerProgressView {
        ConsumerProgressView::new(self.0.clone())
    }

    /// Fixed, validated membership, available without reading mutable progress.
    pub fn consumer_ids(&self) -> &[ComponentId] {
        &self.0.identity.consumers
    }
}

impl ConsumerProgressProvider for Provider {
    fn graph_id(&self) -> &str {
        &self.identity.graph_id
    }
    fn producer_id(&self) -> &ComponentId {
        &self.identity.producer_id
    }
    fn snapshot(&self) -> anyhow::Result<ConsumerProgressSnapshot> {
        let bytes = unsafe { take_reply(self.raw.snapshot.expect("validated")(self.raw.context))? };
        decode_snapshot(&bytes, &self.identity)
    }
    fn subscribe(&self) -> anyhow::Result<Box<dyn ConsumerProgressUpdates>> {
        let mut raw = ConsumerProgressSubscriptionV1::null();
        unsafe {
            take_status(self.raw.subscribe.expect("validated")(
                self.raw.context,
                &mut raw,
            ))?;
        }
        raw.header.validate::<ConsumerProgressSubscriptionV1>()?;
        anyhow::ensure!(
            !raw.context.is_null() && raw.release.is_some(),
            "invalid native consumer progress subscription ownership"
        );
        let subscription = Subscription {
            raw,
            identity: self.identity.clone(),
        };
        anyhow::ensure!(
            raw.snapshot.is_some() && raw.wait.is_some(),
            "incomplete native consumer progress subscription"
        );
        Ok(Box::new(subscription))
    }
}

struct Subscription {
    raw: ConsumerProgressSubscriptionV1,
    identity: Arc<ProgressIdentity>,
}
unsafe impl Send for Subscription {}
unsafe impl Sync for Subscription {}
impl Drop for Subscription {
    fn drop(&mut self) {
        unsafe { self.raw.release.expect("validated")(self.raw.context) };
    }
}

#[async_trait]
impl ConsumerProgressUpdates for Subscription {
    fn snapshot(&mut self) -> anyhow::Result<ConsumerProgressSnapshot> {
        let bytes = unsafe { take_reply(self.raw.snapshot.expect("validated")(self.raw.context))? };
        decode_snapshot(&bytes, &self.identity)
    }

    async fn changed(&mut self) -> anyhow::Result<()> {
        let operation = {
            let mut operation = abi::OperationHandle::null();
            unsafe {
                take_status(self.raw.wait.expect("validated")(
                    self.raw.context,
                    &mut operation,
                ))?;
                OperationFuture::new(operation)?
            }
        };
        let bytes = operation.await?;
        if !bytes.is_empty() {
            return Err(
                Failure::protocol("consumer progress wait returned unexpected data").into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{SourceProgressKey, StreamId, MAX_CONSUMER_PROGRESS_MEMBERS};
    use drasi_core_types::interface::SourceCheckpoint;

    fn id(name: &str) -> ComponentId {
        ComponentId::try_new(name).unwrap()
    }
    fn identity() -> ProgressIdentity {
        ProgressIdentity {
            version: VERSION,
            graph_id: "graph".into(),
            producer_id: id("producer"),
            consumers: vec![id("first"), id("second")],
        }
    }
    fn snapshot() -> ConsumerProgressSnapshot {
        ConsumerProgressSnapshot {
            consumers: BTreeMap::from([
                (
                    id("first"),
                    Arc::new(SourceProgressSnapshot {
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
                    }),
                ),
                (id("second"), Arc::new(SourceProgressSnapshot::default())),
            ]),
        }
    }

    #[test]
    fn consumer_progress_wire_preserves_every_member_without_aggregation() -> anyhow::Result<()> {
        let original = snapshot();
        let bytes = encode_snapshot(&original, &identity())?;
        let decoded = decode_snapshot(&bytes, &identity())?;
        for (id, state) in &original.consumers {
            assert_eq!(
                progress::encode_snapshot(state)?,
                progress::encode_snapshot(&decoded.consumers[id])?
            );
        }
        assert!(!decoded.ready() && !decoded.recovered());
        assert_eq!(
            decoded.consumers[&id("first")].failure.as_deref(),
            Some("fenced")
        );
        assert!(!decoded.consumers[&id("second")].persistent);
        assert!(decoded.consumers[&id("second")].checkpoints.is_empty());
        assert_eq!(encode_snapshot(&decoded, &identity())?, bytes);
        Ok(())
    }

    #[test]
    fn consumer_progress_identity_rejects_invalid_membership() {
        assert_eq!(MAX_PROGRESS_MEMBERS, MAX_CONSUMER_PROGRESS_MEMBERS);
        for consumers in [
            vec![],
            vec![id("first"), id("first")],
            vec![id("producer")],
            (0..=MAX_PROGRESS_MEMBERS)
                .map(|i| id(&format!("consumer-{i}")))
                .collect(),
        ] {
            assert!(ProgressIdentity {
                consumers,
                ..identity()
            }
            .validate()
            .is_err());
        }
        assert!(ProgressIdentity {
            version: VERSION + 1,
            ..identity()
        }
        .validate()
        .is_err());
        assert!(ProgressIdentity {
            graph_id: String::new(),
            ..identity()
        }
        .validate()
        .is_err());
        let consumers = (0..MAX_PROGRESS_MEMBERS)
            .map(|i| id(&format!("consumer-{i}")))
            .collect();
        ProgressIdentity {
            consumers,
            ..identity()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn consumer_progress_wire_rejects_missing_extra_duplicate_and_changed_members(
    ) -> anyhow::Result<()> {
        let bytes = encode_snapshot(&snapshot(), &identity())?;
        for replacement in ["first", "unknown", "producer"] {
            let mut malformed: Snapshot<'_> = decode(&bytes)?;
            malformed.consumers[1].id = id(replacement);
            assert!(decode_snapshot(&encode(&malformed)?, &identity()).is_err());
        }
        let mut malformed: Snapshot<'_> = decode(&bytes)?;
        malformed.consumers.pop();
        assert!(decode_snapshot(&encode(&malformed)?, &identity()).is_err());
        malformed.consumers.push(Member {
            id: id("second"),
            snapshot: Cow::Borrowed(b"invalid"),
        });
        assert!(decode_snapshot(&encode(&malformed)?, &identity()).is_err());
        let mut malformed: Snapshot<'_> = decode(&bytes)?;
        malformed.version += 1;
        assert!(decode_snapshot(&encode(&malformed)?, &identity()).is_err());
        let mut original = snapshot();
        original.consumers.remove(&id("second"));
        assert!(encode_snapshot(&original, &identity()).is_err());
        original
            .consumers
            .insert(id("other"), Arc::new(SourceProgressSnapshot::default()));
        assert!(encode_snapshot(&original, &identity()).is_err());
        assert!(decode_snapshot(&vec![0; MAX_PROGRESS_BYTES + 1], &identity()).is_err());
        Ok(())
    }

    #[test]
    fn consumer_progress_limits_cover_the_whole_set() -> anyhow::Result<()> {
        let state = SourceProgressSnapshot {
            transport_sequences: (0..MAX_PROGRESS_ENTRIES / 2)
                .map(|i| (StreamId::try_new(format!("s{i}")).unwrap(), i as u64))
                .collect(),
            ..Default::default()
        };
        let mut states = ConsumerProgressSnapshot {
            consumers: identity()
                .consumers
                .into_iter()
                .map(|id| (id, Arc::new(state.clone())))
                .collect(),
        };
        encode_snapshot(&states, &identity())?;
        Arc::make_mut(states.consumers.get_mut(&id("first")).unwrap())
            .transport_sequences
            .insert(StreamId::try_new("overflow")?, 1);
        assert!(encode_snapshot(&states, &identity()).is_err());
        let malformed = Snapshot {
            version: VERSION,
            consumers: states
                .consumers
                .iter()
                .map(|(id, state)| {
                    Ok(Member {
                        id: id.clone(),
                        snapshot: Cow::Owned(progress::encode_snapshot(state)?),
                    })
                })
                .collect::<anyhow::Result<_>>()?,
        };
        assert!(decode_snapshot(&encode(&malformed)?, &identity()).is_err());

        let oversized = Arc::new(SourceProgressSnapshot {
            failure: Some("x".repeat(MAX_PROGRESS_BYTES / 2).into()),
            ..Default::default()
        });
        progress::encode_snapshot(&oversized)?;
        let states = ConsumerProgressSnapshot {
            consumers: identity()
                .consumers
                .into_iter()
                .map(|id| (id, oversized.clone()))
                .collect(),
        };
        assert!(encode_snapshot(&states, &identity()).is_err());
        Ok(())
    }
}
