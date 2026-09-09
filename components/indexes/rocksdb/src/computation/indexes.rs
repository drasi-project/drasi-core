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

//! Thin delegates retain ownership of existing backend futures until their own
//! blocking jobs finish, including lazy stream pulls. No index algorithms change.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, ElementArchiveIndex, ElementIndex, ElementStream, FutureElementRef,
        FutureQueue, IndexError, LazySortedSetStore, PushType, ResultIndex, ResultKey, ResultOwner,
        ResultSequence, ResultSequenceCounter,
    },
    models::{Element, ElementReference, ElementTimestamp, QueryJoin, TimestampRange},
    path_solver::match_path::MatchPath,
};
use futures::StreamExt;
use ordered_float::OrderedFloat;

use super::blocking::BlockingScope;

pub(super) struct ScopedIndex<T> {
    inner: Arc<T>,
    work: Arc<BlockingScope>,
}

impl<T> ScopedIndex<T> {
    pub(super) fn new(inner: T, work: Arc<BlockingScope>) -> Self {
        Self {
            inner: Arc::new(inner),
            work,
        }
    }
}

fn scoped_stream(stream: ElementStream, work: Arc<BlockingScope>) -> ElementStream {
    Box::pin(futures::stream::try_unfold(
        (stream, work),
        |(mut stream, work)| async move {
            let (stream, item) = work
                .run_async(async move {
                    let item = stream.next().await;
                    Ok((stream, item))
                })
                .await?;
            match item {
                Some(Ok(element)) => Ok(Some((element, (stream, work)))),
                Some(Err(error)) => Err(error),
                None => Ok(None),
            }
        },
    ))
}

#[async_trait]
impl<T: ElementIndex + 'static> ElementIndex for ScopedIndex<T> {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        self.work
            .run_async(async move { inner.get_element(&reference).await })
            .await
    }

    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let element = element.clone();
        let slots = slot_affinity.clone();
        self.work
            .run_async(async move { inner.set_element(&element, &slots).await })
            .await
    }

    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        self.work
            .run_async(async move { inner.delete_element(&reference).await })
            .await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        self.work
            .run_async(async move { inner.get_slot_element_by_ref(slot, &reference).await })
            .await
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let inner = self.inner.clone();
        let reference = inbound_ref.clone();
        let stream = self
            .work
            .run_async(async move { inner.get_slot_elements_by_inbound(slot, &reference).await })
            .await?;
        Ok(scoped_stream(stream, self.work.clone()))
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let inner = self.inner.clone();
        let reference = outbound_ref.clone();
        let stream = self
            .work
            .run_async(async move { inner.get_slot_elements_by_outbound(slot, &reference).await })
            .await?;
        Ok(scoped_stream(stream, self.work.clone()))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.clear().await })
            .await
    }

    async fn set_joins(&self, match_path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.inner.set_joins(match_path, joins).await;
    }
}

#[async_trait]
impl<T: ElementArchiveIndex + 'static> ElementArchiveIndex for ScopedIndex<T> {
    async fn get_element_as_at(
        &self,
        element_ref: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        self.work
            .run_async(async move { inner.get_element_as_at(&reference, time).await })
            .await
    }

    async fn get_element_versions(
        &self,
        element_ref: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        let stream = self
            .work
            .run_async(async move { inner.get_element_versions(&reference, range).await })
            .await?;
        Ok(scoped_stream(stream, self.work.clone()))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.clear().await })
            .await
    }
}

#[async_trait]
impl<T: FutureQueue + 'static> FutureQueue for ScopedIndex<T> {
    async fn push(
        &self,
        push_type: PushType,
        position_in_query: usize,
        group_signature: u64,
        element_ref: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let inner = self.inner.clone();
        let reference = element_ref.clone();
        self.work
            .run_async(async move {
                inner
                    .push(
                        push_type,
                        position_in_query,
                        group_signature,
                        &reference,
                        original_time,
                        due_time,
                    )
                    .await
            })
            .await
    }

    async fn remove(
        &self,
        position_in_query: usize,
        group_signature: u64,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.remove(position_in_query, group_signature).await })
            .await
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let inner = self.inner.clone();
        self.work.run_async(async move { inner.pop().await }).await
    }

    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.peek_due_time().await })
            .await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.clear().await })
            .await
    }
}

impl<T: ResultIndex + 'static> ResultIndex for ScopedIndex<T> {}

#[async_trait]
impl<T: AccumulatorIndex + 'static> AccumulatorIndex for ScopedIndex<T> {
    async fn clear(&self) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.clear().await })
            .await
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        let inner = self.inner.clone();
        let key = key.clone();
        let owner = owner.clone();
        self.work
            .run_async(async move { inner.get(&key, &owner).await })
            .await
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.set(key, owner, value).await })
            .await
    }
}

#[async_trait]
impl<T: LazySortedSetStore + 'static> LazySortedSetStore for ScopedIndex<T> {
    async fn get_next(
        &self,
        set_id: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.get_next(set_id, value).await })
            .await
    }

    async fn get_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.get_value_count(set_id, value).await })
            .await
    }

    async fn increment_value_count(
        &self,
        set_id: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.increment_value_count(set_id, value, delta).await })
            .await
    }
}

#[async_trait]
impl<T: ResultSequenceCounter + 'static> ResultSequenceCounter for ScopedIndex<T> {
    async fn apply_sequence(
        &self,
        sequence: u64,
        source_change_id: &str,
    ) -> Result<(), IndexError> {
        let inner = self.inner.clone();
        let source_change_id = source_change_id.to_owned();
        self.work
            .run_async(async move { inner.apply_sequence(sequence, &source_change_id).await })
            .await
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let inner = self.inner.clone();
        self.work
            .run_async(async move { inner.get_sequence().await })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use drasi_core::models::{ElementMetadata, ElementPropertyMap};
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_lazy_stream_pull_is_joined_by_the_resource_owner() {
        let work = Arc::new(BlockingScope::default());
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let completed = finished.clone();
        let backend = futures::stream::once(async move {
            tokio::task::spawn_blocking(move || {
                entered_tx.send(()).expect("backend stream pull entry");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(IndexError::other)?;
                completed.store(true, Ordering::Release);
                Ok(Arc::new(Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "node"),
                        labels: Arc::from([]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::new(),
                }))
            })
            .await
            .map_err(IndexError::other)?
        });
        let mut stream = scoped_stream(Box::pin(backend), work.clone());
        let mut pull = Box::pin(stream.next());
        tokio::select! {
            result = &mut pull => panic!("backend pull must still be waiting: {result:?}"),
            result = entered_rx => result.expect("backend pull entered"),
        }
        drop(pull);
        drop(stream);
        let mut cleanup = Box::pin(work.shutdown());
        assert!(futures::poll!(&mut cleanup).is_pending());
        assert!(!finished.load(Ordering::Acquire));
        release_tx.send(()).expect("release backend pull");
        cleanup.await.expect("join lazy backend pull");
        assert!(finished.load(Ordering::Acquire));
    }
}
