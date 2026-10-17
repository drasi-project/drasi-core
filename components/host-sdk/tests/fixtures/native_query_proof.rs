// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only entry point for the resource-boundary proof, not a production
//! Query factory or a transformer pretending to support the Query role.

#[path = "query_proof/client.rs"]
mod client;
#[path = "query_proof/engine.rs"]
mod engine;
#[path = "query_proof/wire.rs"]
mod wire;

use drasi_computation_plugin_sdk::{
    abi,
    transport::{self, Failure},
};
use drasi_core::query::QueryEvaluator;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};

struct State {
    text: String,
    indexes: Arc<client::Indexes>,
    evaluator: OnceLock<QueryEvaluator>,
    busy: AtomicBool,
}

struct Invocation(Arc<State>);
impl Drop for Invocation {
    fn drop(&mut self) {
        self.0.indexes.unbind();
        self.0.busy.store(false, Ordering::Release);
    }
}

#[no_mangle]
/// # Safety
/// Input bytes and the output pointer must obey the native ABI borrowing rules.
pub unsafe extern "C" fn drasi_native_query_proof(
    input: abi::BorrowedBytes,
    out: *mut abi::ComponentHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol("null proof component output"));
        }
        let config: wire::Create =
            wire::decode(unsafe { transport::borrowed_bytes(input, wire::LIMIT)? })?;
        let state = Arc::new(State {
            text: config.query,
            indexes: Arc::new(client::Indexes::default()),
            evaluator: OnceLock::new(),
            busy: AtomicBool::new(false),
        });
        unsafe {
            transport::write_out(
                out,
                abi::ComponentHandle {
                    state: Arc::into_raw(state).cast_mut().cast(),
                    vtable: &TABLE,
                },
            )
        }
    })
}

static TABLE: abi::ComponentVTable = abi::ComponentVTable {
    header: abi::Header::new::<abi::ComponentVTable>(),
    release: Some(release),
    inspect: None,
    configuration: None,
    bind_control: None,
    begin: Some(begin),
};

unsafe extern "C" fn release(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Arc::from_raw(state.cast::<State>())) });
}

unsafe extern "C" fn begin(
    state: *mut c_void,
    operation: u32,
    input: abi::BorrowedBytes,
    scope: *const abi::Transaction,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if state.is_null() || out.is_null() {
            return Err(Failure::protocol("null query proof operation"));
        }
        if !matches!(operation, abi::operation::START | abi::operation::TRANSFORM) {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "unsupported proof operation",
            ));
        }
        let input = unsafe { transport::borrowed_bytes(input, wire::LIMIT)? }.to_vec();
        unsafe { Arc::increment_strong_count(state.cast::<State>()) };
        let state = unsafe { Arc::from_raw(state.cast::<State>()) };
        if state.busy.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                abi::status::BUSY,
                "query proof operation in progress",
            ));
        }
        let invocation = Invocation(state);
        unsafe { invocation.0.indexes.bind(scope)? };
        let operation = transport::export_operation(
            async move {
                let state = &invocation.0;
                let result: anyhow::Result<Vec<u8>> = async {
                    if operation == abi::operation::START {
                        anyhow::ensure!(
                            state.evaluator.get().is_none(),
                            "query proof already initialized"
                        );
                        let query = engine::build(
                            &state.text,
                            state.indexes.clone(),
                            state.indexes.clone(),
                            state.indexes.clone(),
                        )
                        .await?;
                        state.indexes.check_setup()?;
                        state
                            .evaluator
                            .set(query)
                            .map_err(|_| anyhow::anyhow!("query initialized twice"))?;
                        Ok(Vec::new())
                    } else {
                        let query = state
                            .evaluator
                            .get()
                            .ok_or_else(|| anyhow::anyhow!("query proof not initialized"))?;
                        engine::evaluate(query, &input).await
                    }
                }
                .await;
                drop(invocation);
                result.map_err(Failure::from)
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}
