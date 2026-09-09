// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    computation::v1::*,
    wal::{WalError, WalProvider, WriteAheadLogConfig},
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
struct Wal {
    rows: Mutex<BTreeMap<u64, SourceChange>>,
    deletes: AtomicUsize,
}

#[async_trait]
impl WalProvider for Wal {
    async fn register(&self, _: &str, _: WriteAheadLogConfig) -> std::result::Result<(), WalError> {
        Ok(())
    }
    async fn append(&self, _: &str, event: &SourceChange) -> std::result::Result<u64, WalError> {
        let mut rows = self.rows.lock().expect("rows");
        let sequence = rows.keys().next_back().copied().unwrap_or(0) + 1;
        rows.insert(sequence, event.clone());
        Ok(sequence)
    }
    async fn read_from(
        &self,
        source: &str,
        sequence: u64,
    ) -> std::result::Result<Vec<(u64, SourceChange)>, WalError> {
        let rows = self.rows.lock().expect("rows");
        if rows.keys().next().is_some_and(|oldest| sequence < *oldest) {
            return Err(WalError::PositionUnavailable {
                source_id: source.into(),
                requested: sequence,
                oldest_available: rows.keys().next().copied(),
            });
        }
        Ok(rows
            .range(sequence..)
            .map(|(sequence, change)| (*sequence, change.clone()))
            .collect())
    }
    async fn prune_up_to(&self, _: &str, sequence: u64) -> std::result::Result<u64, WalError> {
        let mut rows = self.rows.lock().expect("rows");
        let before = rows.len();
        rows.retain(|seq, _| *seq > sequence);
        Ok((before - rows.len()) as u64)
    }
    async fn head_sequence(&self, _: &str) -> std::result::Result<u64, WalError> {
        Ok(self
            .rows
            .lock()
            .expect("rows")
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0))
    }
    async fn oldest_sequence(&self, _: &str) -> std::result::Result<Option<u64>, WalError> {
        Ok(self.rows.lock().expect("rows").keys().next().copied())
    }
    async fn event_count(&self, _: &str) -> std::result::Result<u64, WalError> {
        Ok(self.rows.lock().expect("rows").len() as u64)
    }
    async fn delete_wal(&self, _: &str) -> std::result::Result<(), WalError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn event(id: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &id.to_string()),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: id,
            },
            properties: ElementPropertyMap::new(),
        },
    }
}

#[tokio::test]
async fn wal_source_resumes_in_order_then_observes_live_appends_without_owning_partition_cleanup() {
    let wal = Arc::new(Wal::default());
    for index in 1..=3 {
        wal.append("source", &event(index)).await.expect("append");
    }
    let resource = Arc::new(WalSourceResource {
        provider: wal.clone(),
        partition: "source".into(),
    });
    let mut source = WalReplaySource::new(
        ComponentId::try_new("replay").expect("id"),
        StreamId::try_new("replay/out").expect("stream"),
        resource,
        1,
    );
    source.start().await.expect("resume after one");
    for sequence in [2, 3] {
        let output = source.next().await.expect("replay").expect("event");
        assert_eq!(
            GraphChangeCodec::source_metadata(&output.envelope)
                .expect("metadata")
                .expect("source")
                .sequence,
            Some(sequence)
        );
        assert_eq!(
            output.envelope.system().sequence(),
            sequence - 1,
            "adapter sequence is separate"
        );
    }
    wal.append("source", &event(4)).await.expect("live append");
    let live = source.next().await.expect("live").expect("event");
    assert_eq!(
        GraphChangeCodec::source_metadata(&live.envelope)
            .expect("metadata")
            .expect("source")
            .sequence,
        Some(4)
    );
    source.stop().await.expect("stop only adapter");
    assert_eq!(wal.deletes.load(Ordering::SeqCst), 0);
    source.start().await.expect("explicit replay restart");
    let replayed = source.next().await.expect("replay").expect("event");
    assert!(replayed.envelope.system().sequence() > live.envelope.system().sequence());
    assert_eq!(
        GraphChangeCodec::source_metadata(&replayed.envelope)
            .expect("metadata")
            .expect("source")
            .sequence,
        Some(2)
    );
    source.stop().await.expect("stop");
}

#[tokio::test]
async fn unavailable_wal_position_is_reported_instead_of_silently_skipped() {
    let wal = Arc::new(Wal::default());
    for index in 1..=3 {
        wal.append("source", &event(index)).await.expect("append");
    }
    wal.prune_up_to("source", 2).await.expect("prune");
    let mut source = WalReplaySource::new(
        ComponentId::try_new("replay").expect("id"),
        StreamId::try_new("replay/out").expect("stream"),
        Arc::new(WalSourceResource {
            provider: wal,
            partition: "source".into(),
        }),
        0,
    );
    assert!(source
        .start()
        .await
        .expect_err("strict replay")
        .downcast_ref::<WalError>()
        .is_some());
}
