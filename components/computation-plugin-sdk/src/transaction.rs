// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    abi,
    transport::{checked_table, take_status, Failure, OperationFuture},
    wire,
};
use async_trait::async_trait;
use drasi_core::models::{Element, ElementReference, ElementValue};
use drasi_lib::computation::v1::{
    ChangeEnvelope, ChangeSetRef, ComponentId, EnvelopeCodec, Schema, Transformer,
};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::Arc;

/// Native opt-in to the existing TransactionTransformer's participant semantics.
/// Only configuration/caches belong on self. State belongs in the borrowed step
/// context; no I/O workers, external effects or commit operation are permitted.
#[async_trait]
pub trait TransactionalComponent: Transformer {
    fn transaction_input_schema(&self) -> Arc<Schema>;
    fn transaction_output_schema(&self) -> Arc<Schema>;
    async fn transform_in_transaction(
        &self,
        input: ChangeEnvelope,
        context: &NativeTransactionContext<'_>,
    ) -> anyhow::Result<ChangeEnvelope>;
}

pub(crate) struct RetainedTransaction(abi::Transaction);
unsafe impl Send for RetainedTransaction {}
unsafe impl Sync for RetainedTransaction {}
impl RetainedTransaction {
    pub(crate) unsafe fn new(raw: *const abi::Transaction) -> Result<Self, Failure> {
        let raw = *unsafe { checked_table(raw)? };
        if raw.context.is_null()
            || raw.retain.is_none()
            || raw.release.is_none()
            || raw.request.is_none()
        {
            return Err(Failure::protocol("incomplete native transaction callbacks"));
        }
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self(raw))
    }
}
impl Drop for RetainedTransaction {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

/// A non-cloneable borrow, valid for one participant invocation. Host state is
/// accessed by serialized RPCs polled inside the host's original transaction
/// borrow. No Rust TransactionContext or task-local scope crosses the ABI.
pub struct NativeTransactionContext<'a> {
    step: &'a ComponentId,
    transaction: &'a RetainedTransaction,
    codec: &'a EnvelopeCodec,
}
impl<'a> NativeTransactionContext<'a> {
    pub(crate) fn new(
        step: &'a ComponentId,
        transaction: &'a RetainedTransaction,
        codec: &'a EnvelopeCodec,
    ) -> Self {
        Self {
            step,
            transaction,
            codec,
        }
    }
    pub fn step_id(&self) -> &ComponentId {
        self.step
    }
    async fn call<I: Serialize, O: DeserializeOwned>(
        &self,
        code: u32,
        input: &I,
    ) -> anyhow::Result<O> {
        let bytes = wire::encode(input)?;
        let operation = {
            let mut operation = abi::OperationHandle::null();
            let raw = &self.transaction.0;
            unsafe {
                take_status(raw.request.expect("validated")(
                    raw.context,
                    code,
                    abi::BorrowedBytes::new(&bytes),
                    &mut operation,
                ))?;
                OperationFuture::new(operation)?
            }
        };
        let bytes = operation.await?;
        wire::decode(&bytes)
    }
    pub async fn get(&self, key: &str) -> anyhow::Result<Option<ElementValue>> {
        self.call(abi::transaction::GET, &key).await
    }
    pub async fn put(&self, key: &str, value: ElementValue) -> anyhow::Result<()> {
        self.call(abi::transaction::PUT, &(key, value)).await
    }
    pub async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.call(abi::transaction::REMOVE, &key).await
    }
    pub async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> anyhow::Result<Option<Arc<Element>>> {
        let value: Option<Element> = self.call(abi::transaction::GET_ELEMENT, reference).await?;
        Ok(value.map(Arc::new))
    }
    pub async fn put_element(&self, element: &Element) -> anyhow::Result<()> {
        self.call(abi::transaction::PUT_ELEMENT, element).await
    }
    pub async fn remove_element(&self, reference: &ElementReference) -> anyhow::Result<()> {
        self.call(abi::transaction::REMOVE_ELEMENT, reference).await
    }
    /// Assign the host step's authoritative intermediate identity and preserve
    /// annotations/lineage. Final publication/commit remains container-owned.
    pub async fn derive(
        &self,
        input: &ChangeEnvelope,
        changes: ChangeSetRef,
    ) -> anyhow::Result<ChangeEnvelope> {
        let output = input.derive(input.id().clone(), changes, input.system().as_ref().clone());
        let bytes: Vec<u8> = self
            .call(
                abi::transaction::DERIVE,
                &wire::DeriveRequest {
                    input: self.codec.encode(input)?.to_vec(),
                    output: self.codec.encode(&output)?.to_vec(),
                },
            )
            .await?;
        Ok(self.codec.decode(&bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport;
    use drasi_core::models::{ElementMetadata, ElementPropertyMap};
    use drasi_lib::computation::v1::GraphChangeCodec;
    use std::{
        collections::HashMap,
        ffi::c_void,
        sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        },
    };

    #[derive(Default)]
    struct Values {
        values: HashMap<String, ElementValue>,
        elements: HashMap<ElementReference, Element>,
    }
    struct Producer {
        values: Mutex<Values>,
        active: AtomicBool,
    }
    unsafe extern "C" fn retain(context: *mut c_void) {
        transport::drop_boundary(|| unsafe {
            Arc::increment_strong_count(context.cast::<Producer>())
        });
    }
    unsafe extern "C" fn release(context: *mut c_void) {
        transport::drop_boundary(|| unsafe {
            Arc::decrement_strong_count(context.cast::<Producer>())
        });
    }
    unsafe extern "C" fn request(
        context: *mut c_void,
        code: u32,
        input: abi::BorrowedBytes,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        transport::status_boundary(|| {
            if out.is_null() {
                return Err(Failure::protocol("null output"));
            }
            let input =
                unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? }.to_vec();
            unsafe { Arc::increment_strong_count(context.cast::<Producer>()) };
            let producer = unsafe { Arc::from_raw(context.cast::<Producer>()) };
            let operation = transport::export_operation(
                async move {
                    if !producer.active.load(Ordering::Acquire) {
                        return Err(Failure::closed());
                    }
                    let execute = || -> anyhow::Result<Vec<u8>> {
                        let mut state = producer.values.lock().unwrap();
                        match code {
                            abi::transaction::GET => {
                                let key: String = wire::decode(&input)?;
                                anyhow::ensure!(key != "denied", "explicit state failure");
                                wire::encode(&state.values.get(&key))
                            }
                            abi::transaction::PUT => {
                                let (key, value): (String, ElementValue) = wire::decode(&input)?;
                                state.values.insert(key, value);
                                wire::encode(&())
                            }
                            abi::transaction::REMOVE => {
                                state.values.remove(&wire::decode::<String>(&input)?);
                                wire::encode(&())
                            }
                            abi::transaction::GET_ELEMENT => wire::encode(
                                &state
                                    .elements
                                    .get(&wire::decode::<ElementReference>(&input)?),
                            ),
                            abi::transaction::PUT_ELEMENT => {
                                let element: Element = wire::decode(&input)?;
                                let reference = match &element {
                                    Element::Node { metadata, .. }
                                    | Element::Relation { metadata, .. } => {
                                        metadata.reference.clone()
                                    }
                                };
                                state.elements.insert(reference, element);
                                wire::encode(&())
                            }
                            abi::transaction::REMOVE_ELEMENT => {
                                state
                                    .elements
                                    .remove(&wire::decode::<ElementReference>(&input)?);
                                wire::encode(&())
                            }
                            _ => anyhow::bail!("unsupported state request"),
                        }
                    };
                    execute().map_err(Failure::from)
                },
                None,
            );
            unsafe { transport::write_out(out, operation) }
        })
    }

    #[tokio::test]
    async fn transaction_values_and_elements_round_trip_through_c_operations() {
        let owner = Arc::new(Producer {
            values: Mutex::new(Values::default()),
            active: AtomicBool::new(true),
        });
        let raw = abi::Transaction {
            header: abi::Header::new::<abi::Transaction>(),
            context: Arc::as_ptr(&owner).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            request: Some(request),
        };
        let retained = unsafe { RetainedTransaction::new(&raw) }.unwrap();
        let codec = wire::codec(&[GraphChangeCodec::schema()]).unwrap();
        let step = ComponentId::try_new("step").unwrap();
        let context = NativeTransactionContext::new(&step, &retained, &codec);
        assert_eq!(context.step_id(), &step);
        assert_eq!(context.get("key").await.unwrap(), None);
        let value = ElementValue::List(vec![
            ElementValue::Integer(i64::MAX),
            ElementValue::Float(f64::NAN.into()),
            ElementValue::String(Arc::from("not JSON-coerced")),
        ]);
        context.put("key", value.clone()).await.unwrap();
        assert_eq!(context.get("key").await.unwrap(), Some(value));
        context.remove("key").await.unwrap();
        assert_eq!(context.get("key").await.unwrap(), None);
        let reference = ElementReference::new("source", "element");
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: reference.clone(),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 9,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"x":1})),
        };
        context.put_element(&element).await.unwrap();
        assert_eq!(
            context.get_element(&reference).await.unwrap().as_deref(),
            Some(&element)
        );
        context.remove_element(&reference).await.unwrap();
        assert!(context.get_element(&reference).await.unwrap().is_none());
        assert!(context.get("denied").await.is_err());
        owner.active.store(false, Ordering::Release);
        assert_eq!(
            context
                .get("key")
                .await
                .unwrap_err()
                .downcast::<Failure>()
                .unwrap()
                .code,
            abi::status::CLOSED
        );
        assert_eq!(Arc::strong_count(&owner), 2);
        drop(retained);
        assert_eq!(Arc::strong_count(&owner), 1);
    }
}
