// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::wire::{self, Accumulator, Key, Owner, Request};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{
    abi,
    transport::{checked_table, take_status, OperationFuture},
};
use drasi_core::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        AccumulatorIndex, ElementIndex, ElementStream, FutureElementRef, FutureQueue, IndexError,
        LazySortedSetStore, PushType, ResultIndex, ResultKey, ResultOwner, ResultSequence,
        ResultSequenceCounter,
    },
    models::{Element, ElementReference, QueryJoin},
    path_solver::match_path::MatchPath,
};
use ordered_float::OrderedFloat;
use serde::de::DeserializeOwned;
use std::sync::{Arc, Mutex};

struct Scope(abi::Transaction);
// The producer owns this retained opaque handle and provides thread-safe callbacks.
unsafe impl Send for Scope {}
unsafe impl Sync for Scope {}

impl Scope {
    unsafe fn retain(raw: *const abi::Transaction) -> Result<Self> {
        let table = *unsafe { checked_table(raw)? };
        ensure!(
            !table.context.is_null()
                && table.retain.is_some()
                && table.release.is_some()
                && table.request.is_some(),
            "incomplete query proof resource scope"
        );
        unsafe { table.retain.expect("validated")(table.context) };
        Ok(Self(table))
    }

    async fn call<T: DeserializeOwned>(&self, request: &Request) -> Result<T> {
        let input = wire::encode(request)?;
        let operation = unsafe {
            let mut operation = abi::OperationHandle::null();
            take_status(self.0.request.expect("validated")(
                self.0.context,
                wire::RESOURCE_REQUEST,
                abi::BorrowedBytes::new(&input),
                &mut operation,
            ))?;
            OperationFuture::new(operation)?
        };
        wire::decode(&operation.await?)
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

#[derive(Default)]
pub struct Indexes {
    scope: Mutex<Option<Arc<Scope>>>,
    setup_failure: Mutex<Option<String>>,
}

impl Indexes {
    /// # Safety
    /// `raw` must point to a live native transaction table for this invocation.
    pub unsafe fn bind(&self, raw: *const abi::Transaction) -> Result<()> {
        let scope = Arc::new(unsafe { Scope::retain(raw)? });
        let mut current = self
            .scope
            .lock()
            .map_err(|_| anyhow::anyhow!("query scope poisoned"))?;
        ensure!(current.is_none(), "query scope is already bound");
        *current = Some(scope);
        Ok(())
    }

    pub fn unbind(&self) {
        self.scope
            .lock()
            .unwrap_or_else(|error| {
                log::error!("Releasing poisoned query proof scope");
                error.into_inner()
            })
            .take();
    }

    pub fn check_setup(&self) -> Result<()> {
        let failure = self
            .setup_failure
            .lock()
            .map_err(|_| anyhow::anyhow!("query setup poisoned"))?;
        if let Some(failure) = failure.as_ref() {
            anyhow::bail!("query index setup failed: {failure}");
        }
        Ok(())
    }

    async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T, IndexError> {
        let scope = self
            .scope
            .lock()
            .map_err(|_| wire::index_error(anyhow::anyhow!("query scope poisoned")))?
            .clone()
            .context("query index access outside its host-owned operation")
            .map_err(wire::index_error)?;
        scope.call(&request).await.map_err(wire::index_error)
    }

    async fn scan(&self, request: Request) -> Result<ElementStream, IndexError> {
        let elements: Vec<Element> = self.call(request).await?;
        if elements.len() > wire::SCAN_LIMIT {
            return Err(wire::index_error(anyhow::anyhow!(
                "query proof scan exceeded its bound"
            )));
        }
        Ok(Box::pin(futures_util::stream::iter(
            elements.into_iter().map(|element| Ok(Arc::new(element))),
        )))
    }
}

#[async_trait]
impl ElementIndex for Indexes {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.call::<Option<Element>>(Request::GetElement(reference.clone()))
            .await
            .map(|value| value.map(Arc::new))
    }

    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.call(Request::SetElement(element.clone(), slots.clone()))
            .await
    }

    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.call(Request::DeleteElement(reference.clone())).await
    }

    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.call::<Option<Element>>(Request::GetSlot(slot, reference.clone()))
            .await
            .map(|value| value.map(Arc::new))
    }

    async fn get_slot_elements(&self, slot: usize) -> Result<ElementStream, IndexError> {
        self.scan(Request::ScanSlot(slot)).await
    }

    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.scan(Request::ScanInbound(slot, reference.clone()))
            .await
    }

    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.scan(Request::ScanOutbound(slot, reference.clone()))
            .await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.call(Request::Clear(0)).await
    }

    async fn set_joins(&self, _: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        if let Err(error) = self
            .call::<()>(Request::Configure {
                virtual_joins: !joins.is_empty(),
            })
            .await
        {
            *self.setup_failure.lock().unwrap_or_else(|error| {
                log::error!("Recording poisoned query proof setup");
                error.into_inner()
            }) = Some(error.to_string());
        }
    }
}

impl ResultIndex for Indexes {}

#[async_trait]
impl AccumulatorIndex for Indexes {
    async fn clear(&self) -> Result<(), IndexError> {
        self.call(Request::Clear(1)).await
    }

    async fn get(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<ValueAccumulator>, IndexError> {
        self.call::<Option<Accumulator>>(Request::GetResult(
            Key::encode(key).map_err(wire::index_error)?,
            Owner::from(owner),
        ))
        .await
        .map(|value| value.map(Into::into))
    }

    async fn set(
        &self,
        key: ResultKey,
        owner: ResultOwner,
        value: Option<ValueAccumulator>,
    ) -> Result<(), IndexError> {
        self.call(Request::SetResult(
            Key::encode(&key).map_err(wire::index_error)?,
            Owner::from(&owner),
            value.map(Into::into),
        ))
        .await
    }
}

#[async_trait]
impl LazySortedSetStore for Indexes {
    async fn get_next(
        &self,
        set: u64,
        value: Option<OrderedFloat<f64>>,
    ) -> Result<Option<(OrderedFloat<f64>, isize)>, IndexError> {
        let next: Option<(u64, i64)> = self
            .call(Request::NextValue(set, value.map(|v| v.0.to_bits())))
            .await?;
        next.map(|(bits, count)| {
            Ok((
                OrderedFloat(f64::from_bits(bits)),
                isize::try_from(count).map_err(IndexError::other)?,
            ))
        })
        .transpose()
    }

    async fn get_value_count(
        &self,
        set: u64,
        value: OrderedFloat<f64>,
    ) -> Result<isize, IndexError> {
        let count: i64 = self
            .call(Request::ValueCount(set, value.0.to_bits()))
            .await?;
        isize::try_from(count).map_err(IndexError::other)
    }

    async fn increment_value_count(
        &self,
        set: u64,
        value: OrderedFloat<f64>,
        delta: isize,
    ) -> Result<(), IndexError> {
        self.call(Request::IncrementCount(
            set,
            value.0.to_bits(),
            i64::try_from(delta).map_err(IndexError::other)?,
        ))
        .await
    }
}

#[async_trait]
impl ResultSequenceCounter for Indexes {
    async fn apply_sequence(&self, sequence: u64, source: &str) -> Result<(), IndexError> {
        self.call(Request::ApplySequence(sequence, source.into()))
            .await
    }

    async fn get_sequence(&self) -> Result<ResultSequence, IndexError> {
        let (sequence, source): (u64, String) = self.call(Request::GetSequence).await?;
        Ok(ResultSequence {
            sequence,
            source_change_id: source.into(),
        })
    }
}

#[async_trait]
impl FutureQueue for Indexes {
    async fn push(
        &self,
        kind: PushType,
        position: usize,
        group: u64,
        reference: &ElementReference,
        original: u64,
        due: u64,
    ) -> Result<bool, IndexError> {
        let kind = match kind {
            PushType::Always => 0,
            PushType::IfNotExists => 1,
            PushType::Overwrite => 2,
        };
        self.call(Request::PushFuture(
            kind,
            position,
            group,
            reference.clone(),
            original,
            due,
        ))
        .await
    }

    async fn remove(&self, position: usize, group: u64) -> Result<(), IndexError> {
        self.call(Request::RemoveFuture(position, group)).await
    }

    async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
        self.call(Request::PopFuture).await
    }

    async fn peek_due_time(&self) -> Result<Option<u64>, IndexError> {
        self.call(Request::PeekFuture).await
    }

    async fn clear(&self) -> Result<(), IndexError> {
        self.call(Request::Clear(2)).await
    }
}
