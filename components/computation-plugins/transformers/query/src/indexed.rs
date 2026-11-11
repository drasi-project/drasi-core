// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

use async_trait::async_trait;
use drasi_computation_plugin_sdk::NativeProcessingContext;
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, ElementArchiveIndex, ElementIndex, ElementStream, FutureElementRef,
        FutureQueue, IndexError, IndexJoin, LazySortedSetStore, PushType, ResultIndex, ResultKey,
        ResultOwner, ResultSequence, ResultSequenceCounter,
    },
    models::{Element, ElementReference, ElementTimestamp, TimestampRange},
};
use ordered_float::OrderedFloat;

fn binding_error(message: &'static str) -> IndexError {
    IndexError::other(std::io::Error::other(message))
}

struct BoundContext<T> {
    value: Arc<T>,
    active: AtomicBool,
}

impl<T> BoundContext<T> {
    fn get(&self) -> Result<Arc<T>, IndexError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(binding_error("query index operation is no longer active"));
        }
        Ok(self.value.clone())
    }
}

#[must_use = "retain the guard until the processing operation finishes"]
pub(crate) struct ContextGuard<T>(Arc<BoundContext<T>>);

impl<T> Drop for ContextGuard<T> {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}

struct ContextBinding<T>(Mutex<Weak<BoundContext<T>>>);

impl<T> Default for ContextBinding<T> {
    fn default() -> Self {
        Self(Mutex::new(Weak::new()))
    }
}

impl<T> ContextBinding<T> {
    fn bind(&self, value: Arc<T>) -> Result<ContextGuard<T>, IndexError> {
        let mut current = self
            .0
            .lock()
            .map_err(|_| binding_error("query index binding poisoned"))?;
        if current
            .upgrade()
            .is_some_and(|context| context.active.load(Ordering::Acquire))
        {
            return Err(binding_error(
                "query indexes already have an active operation",
            ));
        }
        let context = Arc::new(BoundContext {
            value,
            active: AtomicBool::new(true),
        });
        *current = Arc::downgrade(&context);
        Ok(ContextGuard(context))
    }

    fn current(&self) -> Result<Arc<BoundContext<T>>, IndexError> {
        self.0
            .lock()
            .map_err(|_| binding_error("query index binding poisoned"))?
            .upgrade()
            .ok_or_else(|| binding_error("query indexes have no active operation"))
    }

    fn get(&self) -> Result<Arc<T>, IndexError> {
        self.current()?.get()
    }
}

#[derive(Default)]
pub(crate) struct Indexes(ContextBinding<NativeProcessingContext>, AtomicBool);

impl Indexes {
    pub(crate) fn take_future_changes(&self) -> bool {
        self.1.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn bind(
        &self,
        context: Arc<NativeProcessingContext>,
    ) -> Result<ContextGuard<NativeProcessingContext>, IndexError> {
        self.0.bind(context)
    }
}

fn slot_stream(context: Weak<BoundContext<NativeProcessingContext>>, stream: u64) -> ElementStream {
    Box::pin(async_stream::try_stream! {
        loop {
            // A lazy stream belongs to the operation that opened it, never a later binding.
            let element = {
                let context = context
                    .upgrade()
                    .ok_or_else(|| binding_error("query slot operation has ended"))?
                    .get()?;
                context.next_slot(stream).await?
            };
            let Some(element) = element else {
                break;
            };
            yield Arc::new(element);
        }
    })
}

#[async_trait]
impl ElementIndex for Indexes {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Ok(self.0.get()?.get_element(reference).await?.map(Arc::new))
    }

    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.0.get()?.set_element(element, slots).await
    }

    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.0.get()?.delete_element(reference).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Ok(self
            .0
            .get()?
            .get_slot_element(slot, reference)
            .await?
            .map(Arc::new))
    }

    async fn get_slot_elements(&self, slot: usize) -> Result<ElementStream, IndexError> {
        let context = self.0.current()?;
        let stream = context.get()?.open_slot(slot).await?;
        Ok(slot_stream(Arc::downgrade(&context), stream))
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let context = self.0.current()?;
        let stream = context.get()?.open_slot_inbound(slot, reference).await?;
        Ok(slot_stream(Arc::downgrade(&context), stream))
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        let context = self.0.current()?;
        let stream = context.get()?.open_slot_outbound(slot, reference).await?;
        Ok(slot_stream(Arc::downgrade(&context), stream))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.0.get()?.clear_elements().await
    }

    async fn set_joins(&self, joins: &[IndexJoin]) -> Result<(), IndexError> {
        self.0.get()?.set_joins(joins).await
    }
}

#[async_trait]
impl ElementArchiveIndex for Indexes {
    async fn get_element_as_at(
        &self,
        reference: &ElementReference,
        time: ElementTimestamp,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        Ok(self
            .0
            .get()?
            .get_element_as_at(reference, time)
            .await?
            .map(Arc::new))
    }

    async fn get_element_versions(
        &self,
        reference: &ElementReference,
        range: TimestampRange<ElementTimestamp>,
    ) -> Result<ElementStream, IndexError> {
        let context = self.0.current()?;
        let stream = context
            .get()?
            .open_archive_versions(reference, range)
            .await?;
        Ok(slot_stream(Arc::downgrade(&context), stream))
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.0.get()?.clear_archive().await
    }
}

#[async_trait]
impl AccumulatorIndex for Indexes {
    async fn clear(&self) -> Result<(), IndexError> {
        Err(IndexError::NotSupported)
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        self.0.get()?.get_result(key, owner).await
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        self.0.get()?.set_result(key, owner, value).await
    }
}

#[async_trait]
impl LazySortedSetStore for Indexes {
    async fn get_next(
        &self,
        set: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        Ok(self
            .0
            .get()?
            .sorted_next(set, value.map(|value| value.0.to_bits()))
            .await?
            .map(|(bits, count)| (OrderedFloat(f64::from_bits(bits)), count)))
    }

    async fn get_value_count(
        &self,
        set: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        self.0.get()?.sorted_count(set, value.0.to_bits()).await
    }

    async fn increment_value_count(
        &self,
        set: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        self.0
            .get()?
            .sorted_increment(set, value.0.to_bits(), delta)
            .await
    }
}

#[async_trait]
impl ResultSequenceCounter for Indexes {
    async fn apply_sequence(&self, _: u64, _: &str) -> Result<(), IndexError> {
        Err(IndexError::NotSupported)
    }
    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        Err(IndexError::NotSupported)
    }
}

impl ResultIndex for Indexes {}

#[async_trait]
impl FutureQueue for Indexes {
    async fn push(
        &self,
        push_type: PushType,
        position: usize,
        signature: u64,
        reference: &ElementReference,
        original_time: ElementTimestamp,
        due_time: ElementTimestamp,
    ) -> Result<bool, IndexError> {
        let changed = self
            .0
            .get()?
            .future_push(
                push_type,
                position,
                signature,
                reference,
                original_time,
                due_time,
            )
            .await?;
        if changed || push_type == PushType::Overwrite {
            self.1.store(true, Ordering::Release);
        }
        Ok(changed)
    }
    async fn remove(&self, position: usize, signature: u64) -> Result<(), IndexError> {
        self.0.get()?.future_remove(position, signature).await?;
        self.1.store(true, Ordering::Release);
        Ok(())
    }
    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        let future = self.0.get()?.future_pop().await?;
        self.1.store(true, Ordering::Release);
        Ok(future)
    }
    async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
        self.0.get()?.future_peek().await
    }
    async fn clear(&self) -> Result<(), IndexError> {
        self.0.get()?.future_clear().await?;
        self.1.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::{pending, Future},
        task::{Context, Waker},
    };

    #[test]
    fn operation_binding_is_exclusive_and_revoked_before_reuse() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        assert!(binding.get().is_err());
        let first = binding.bind(Arc::new(1))?;
        let retired = binding.current()?;
        assert_eq!(*binding.get()?, 1);
        assert!(binding.bind(Arc::new(2)).is_err());
        drop(first);
        assert!(binding.get().is_err());
        assert!(retired.get().is_err());
        let second = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        assert!(retired.get().is_err());
        drop(retired);
        assert_eq!(*binding.get()?, 2);
        drop(second);
        assert!(binding.get().is_err());
        Ok(())
    }

    #[test]
    fn cancelled_operation_releases_its_context() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let value = Arc::new(1);
        let weak = Arc::downgrade(&value);
        let mut operation = Box::pin(async {
            let _guard = binding.bind(value)?;
            pending::<()>().await;
            Ok::<_, IndexError>(())
        });
        assert!(operation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        assert_eq!(*binding.get()?, 1);
        drop(operation);
        assert!(binding.get().is_err());
        assert!(weak.upgrade().is_none());
        let _guard = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        Ok(())
    }

    #[test]
    fn completed_context_is_not_retained_or_rebound_by_lazy_readers() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let value = Arc::new(1);
        let weak = Arc::downgrade(&value);
        let guard = binding.bind(value)?;
        let reader = Arc::downgrade(&binding.current()?);
        drop(guard);
        assert!(weak.upgrade().is_none());
        assert!(reader.upgrade().is_none());
        let _guard = binding.bind(Arc::new(2))?;
        assert_eq!(*binding.get()?, 2);
        assert!(reader.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn failed_operation_and_other_instances_cannot_reuse_a_binding() -> Result<(), IndexError> {
        let binding = ContextBinding::default();
        let other = ContextBinding::default();
        let _other = other.bind(Arc::new(2))?;
        let failure = (|| {
            let _guard = binding.bind(Arc::new(1))?;
            assert_eq!(*binding.get()?, 1);
            assert_eq!(*other.get()?, 2);
            Err::<(), _>(binding_error("injected operation failure"))
        })();
        assert!(failure.is_err());
        assert!(binding.get().is_err());
        assert_eq!(*other.get()?, 2);
        let _guard = binding.bind(Arc::new(3))?;
        assert_eq!(*binding.get()?, 3);
        Ok(())
    }
}
