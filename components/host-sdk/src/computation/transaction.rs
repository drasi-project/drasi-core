// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A revocable RPC mailbox, not an owned/erased TransactionContext. The only
//! transaction borrow lives in serve() and is dropped with the host step future.

use super::proxy::NativeComponentProxy;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    transport::{self, Failure},
    wire,
};
use drasi_core::models::{Element, ElementReference, ElementValue};
use drasi_lib::computation::v1::{ChangeEnvelope, EnvelopeCodec, TransactionContext};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot};

const REQUEST_CAPACITY: usize = 16;
struct Request {
    code: u32,
    input: Vec<u8>,
    response: oneshot::Sender<Result<Vec<u8>, Failure>>,
}
struct State {
    admission: Mutex<()>,
    active: AtomicBool,
    failed: AtomicBool,
    outstanding: AtomicUsize,
    requests: mpsc::Sender<Request>,
}
struct StepScope {
    state: Arc<State>,
    requests: mpsc::Receiver<Request>,
}
impl StepScope {
    fn new() -> Self {
        let (send, receive) = mpsc::channel(REQUEST_CAPACITY);
        Self {
            state: Arc::new(State {
                admission: Mutex::new(()),
                active: AtomicBool::new(true),
                failed: AtomicBool::new(false),
                outstanding: AtomicUsize::new(0),
                requests: send,
            }),
            requests: receive,
        }
    }
    fn table(&self) -> abi::Transaction {
        abi::Transaction {
            header: abi::Header::new::<abi::Transaction>(),
            context: Arc::as_ptr(&self.state).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            request: Some(request),
        }
    }
    fn revoke(&mut self) {
        let admission = self
            .state
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.state.active.store(false, Ordering::Release);
        self.requests.close();
        drop(admission);
        while let Ok(request) = self.requests.try_recv() {
            let _ = request.response.send(Err(Failure::closed()));
        }
    }
    async fn serve(
        &mut self,
        context: &TransactionContext<'_>,
        codec: &EnvelopeCodec,
    ) -> anyhow::Result<()> {
        while let Some(request) = self.requests.recv().await {
            if request.response.is_closed() || !self.state.active.load(Ordering::Acquire) {
                self.state.failed.store(true, Ordering::Release);
                let _ = request.response.send(Err(Failure::cancelled()));
                continue;
            }
            let result = apply(context, codec, request.code, &request.input)
                .await
                .map_err(Failure::from);
            if result.is_err() {
                self.state.failed.store(true, Ordering::Release);
            }
            if request.response.send(result).is_err() {
                self.state.failed.store(true, Ordering::Release);
            }
        }
        anyhow::bail!("native transaction request scope closed while the participant was running")
    }
}
impl Drop for StepScope {
    fn drop(&mut self) {
        self.revoke();
    }
}

struct RequestGuard {
    state: Arc<State>,
    completed: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.state.failed.store(true, Ordering::Release);
        }
        self.state.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}
unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn request(
    context: *mut c_void,
    code: u32,
    input: abi::BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol(
                "null native transaction operation output",
            ));
        }
        let state = unsafe { &*context.cast::<State>() };
        let admission = state.admission.lock().map_err(|_| {
            state.failed.store(true, Ordering::Release);
            Failure::failed("native transaction admission poisoned")
        })?;
        if !state.active.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        if !(abi::transaction::GET..=abi::transaction::DERIVE).contains(&code) {
            state.failed.store(true, Ordering::Release);
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "unsupported transaction state request",
            ));
        }
        let input = unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES) }
            .inspect_err(|_| state.failed.store(true, Ordering::Release))?
            .to_vec();
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let owner = unsafe { Arc::from_raw(context.cast::<State>()) };
        state.outstanding.fetch_add(1, Ordering::AcqRel);
        let mut guard = RequestGuard {
            state: owner,
            completed: false,
        };
        drop(admission);
        let (response, receive) = oneshot::channel();
        state
            .requests
            .try_send(Request {
                code,
                input,
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Failure::new(
                    abi::status::BUSY,
                    "native transaction request capacity exceeded",
                ),
                mpsc::error::TrySendError::Closed(_) => Failure::closed(),
            })?;
        let operation = transport::export_operation(
            async move {
                let result = receive.await.unwrap_or_else(|_| Err(Failure::closed()));
                if result.is_err() {
                    guard.state.failed.store(true, Ordering::Release);
                }
                guard.completed = true;
                result
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

pub(super) async fn run(
    component: &NativeComponentProxy,
    input: ChangeEnvelope,
    context: &TransactionContext<'_>,
) -> anyhow::Result<ChangeEnvelope> {
    let mut scope = StepScope::new();
    let operation = {
        let table = scope.table();
        component.inner.begin(
            abi::operation::TRANSACT,
            &component.codec.encode(&input)?,
            Some(&table),
        )?
    };
    tokio::pin!(operation);
    let result = tokio::select! {
        biased;
        result = &mut operation => result.map_err(anyhow::Error::from),
        result = scope.serve(context, &component.codec) => {
            result?;
            unreachable!("request service never completes successfully")
        },
    };
    scope.revoke();
    let output = result?;
    anyhow::ensure!(
        !scope.state.failed.load(Ordering::Acquire)
            && scope.state.outstanding.load(Ordering::Acquire) == 0,
        "native participant completed with failed, cancelled or unfinished state requests"
    );
    Ok(component.codec.decode(&output)?)
}

async fn apply(
    context: &TransactionContext<'_>,
    codec: &EnvelopeCodec,
    code: u32,
    input: &[u8],
) -> anyhow::Result<Vec<u8>> {
    use abi::transaction::*;
    match code {
        GET => wire::encode(&context.get(&wire::decode::<String>(input)?).await?),
        PUT => {
            let (key, value): (String, ElementValue) = wire::decode(input)?;
            context.put(&key, value).await?;
            wire::encode(&())
        }
        REMOVE => {
            context.remove(&wire::decode::<String>(input)?).await?;
            wire::encode(&())
        }
        GET_ELEMENT => {
            let reference: ElementReference = wire::decode(input)?;
            wire::encode(&context.get_element(&reference).await?.as_deref())
        }
        PUT_ELEMENT => {
            context
                .put_element(&wire::decode::<Element>(input)?)
                .await?;
            wire::encode(&())
        }
        REMOVE_ELEMENT => {
            context
                .remove_element(&wire::decode::<ElementReference>(input)?)
                .await?;
            wire::encode(&())
        }
        DERIVE => {
            let request: wire::DeriveRequest = wire::decode(input)?;
            let input = codec.decode(&request.input)?;
            let output = codec.decode(&request.output)?;
            wire::encode(
                &codec
                    .encode(&context.derive(&input, output.changes().clone())?)?
                    .to_vec(),
            )
        }
        _ => anyhow::bail!("unsupported native transaction state operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::transport::{take_status, OperationFuture};

    #[tokio::test]
    async fn retained_context_survives_revocation_and_pending_requests_fail_closed() {
        let mut scope = StepScope::new();
        let raw = scope.table();
        let weak = Arc::downgrade(&scope.state);
        unsafe { raw.retain.unwrap()(raw.context) };
        let bytes = wire::encode(&"key").unwrap();
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(raw.request.unwrap()(
                raw.context,
                abi::transaction::GET,
                abi::BorrowedBytes::new(&bytes),
                &mut operation,
            ))
            .unwrap()
        };
        scope.revoke();
        drop(scope);
        assert!(weak.upgrade().is_some());
        let failure = unsafe { OperationFuture::new(operation) }
            .unwrap()
            .await
            .unwrap_err();
        assert_eq!(failure.code, abi::status::CLOSED);
        let mut out = abi::OperationHandle::null();
        let failure = unsafe {
            take_status(raw.request.unwrap()(
                raw.context,
                abi::transaction::PUT,
                abi::BorrowedBytes::empty(),
                &mut out,
            ))
        }
        .unwrap_err();
        assert_eq!(failure.code, abi::status::CLOSED);
        assert!(out.state.is_null());
        unsafe { raw.release.unwrap()(raw.context) };
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn cancelled_suboperation_poisoning_cannot_be_ignored() {
        let scope = StepScope::new();
        let raw = scope.table();
        let bytes = wire::encode(&"key").unwrap();
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(raw.request.unwrap()(
                raw.context,
                abi::transaction::GET,
                abi::BorrowedBytes::new(&bytes),
                &mut operation,
            ))
            .unwrap()
        };
        assert_eq!(scope.state.outstanding.load(Ordering::Acquire), 1);
        drop(unsafe { OperationFuture::new(operation) }.unwrap());
        assert!(scope.state.failed.load(Ordering::Acquire));
        assert_eq!(scope.state.outstanding.load(Ordering::Acquire), 0);
    }
}
