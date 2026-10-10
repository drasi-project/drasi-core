// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at http://www.apache.org/licenses/LICENSE-2.0
// Unless required by applicable law or agreed to in writing, software distributed
// under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR
// CONDITIONS OF ANY KIND, either express or implied. See the License for the
// specific language governing permissions and limitations under the License.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use drasi_lib::computation::v1::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Bounded,
    ByteBounded,
    Broadcast,
    Ranked,
    RankedLossy,
    Retained,
    RetainedLossy,
    Qos,
    QosLossy,
}

impl Profile {
    pub const ALL: [Self; 9] = [
        Self::Bounded,
        Self::ByteBounded,
        Self::Broadcast,
        Self::Ranked,
        Self::RankedLossy,
        Self::Retained,
        Self::RetainedLossy,
        Self::Qos,
        Self::QosLossy,
    ];

    pub fn backpressure(self) -> bool {
        matches!(
            self,
            Self::Bounded | Self::ByteBounded | Self::Ranked | Self::Retained | Self::Qos
        )
    }

    pub fn acknowledgement(self) -> bool {
        matches!(
            self,
            Self::Retained | Self::RetainedLossy | Self::Qos | Self::QosLossy
        )
    }

    pub fn provider(
        self,
        name: &str,
        stream: StreamId,
        capacity: NonZeroUsize,
    ) -> (Box<dyn PipeProvider>, BTreeMap<ResourceId, ResourceHandle>) {
        let resource = ResourceId::try_new(name).expect("resource ID");
        let policy = if self.backpressure() {
            RetentionPolicy::Backpressure
        } else {
            RetentionPolicy::PruneOldest
        };
        match self {
            Self::Bounded => (
                Box::new(BoundedPipeConfig {
                    capacity: capacity.get(),
                }),
                BTreeMap::new(),
            ),
            Self::ByteBounded => (
                Box::new(ByteBoundedPipeConfig {
                    capacity: capacity.get(),
                    max_bytes: capacity.get()
                        * BinaryEnvelopeCodec::encoded_size(&timed_event(1)).expect("fixture size"),
                }),
                BTreeMap::new(),
            ),
            Self::Broadcast => (
                Box::new(BroadcastPipeConfig {
                    capacity: capacity.get(),
                    lag_policy: BroadcastLagPolicy::Report,
                }),
                BTreeMap::new(),
            ),
            Self::Ranked | Self::RankedLossy => (
                Box::new(RankedInputPipeConfig {
                    queue: resource.clone(),
                    capacity: capacity.get(),
                    source_rank: 0,
                    source_id: None,
                    drop_when_full: !self.backpressure(),
                }),
                BTreeMap::from([(
                    resource,
                    RankedInputQueue::new(capacity.get())
                        .expect("ranked queue")
                        .resource(),
                )]),
            ),
            Self::Retained | Self::RetainedLossy => {
                let store = Arc::new(RetainedStoreResource(Arc::new(MemoryEnvelopeStore::new(
                    capacity, policy,
                ))));
                (
                    Box::new(RetainedPipeConfig {
                        resource: resource.clone(),
                        capacity,
                        durable: false,
                        retention: policy,
                        gap_policy: ReplayGapPolicy::Strict,
                    }),
                    BTreeMap::from([(
                        resource,
                        ResourceHandle::new(ResourceRole::StateStore, store.clone())
                            .with_cleanup(store),
                    )]),
                )
            }
            Self::Qos | Self::QosLossy => {
                let definition = QosChannelDefinition {
                    stream,
                    capacity,
                    durable: false,
                    retention: policy,
                    subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
                };
                let channel = QosChannel::volatile(definition.clone()).expect("QoS channel");
                (
                    Box::new(definition.pipe(resource.clone(), "consumer")),
                    BTreeMap::from([(resource, channel.resource())]),
                )
            }
        }
    }
}

pub fn timed_event(sequence: u64) -> ChangeEnvelope {
    let original = super::computation_support::root("source", sequence, &[sequence as u16]);
    ChangeEnvelope::new(
        original.id().clone(),
        original.changes().clone(),
        SystemMetadata::new(original.system().stream().clone(), sequence).with_timestamp(
            chrono::DateTime::from_timestamp(sequence as i64, 0).expect("event time"),
        ),
    )
}
