// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::abi::activation_credentials::*;

pub(super) static EXTENSION: PluginActivationCredentialsV1 = PluginActivationCredentialsV1 {
    header: Header::new::<PluginActivationCredentialsV1>(),
    version: VERSION,
    reserved: 0,
    factory: Some(factory),
    context: Some(context),
};

unsafe extern "C" fn factory(state: *mut c_void) -> Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state_ref::<FactoryState>(state)? };
        wire::encode(
            &state.plugin.definition.factories[state.index].supports_activation_credentials(),
        )
        .map_err(Failure::from)
    })
}

unsafe extern "C" fn context(state: *mut c_void, input: BorrowedBytes) -> Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state_ref::<FactoryState>(state)? };
        let mut request: CreateRequest =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? })
                .map_err(|_| Failure::protocol("invalid activation credential context request"))?;
        request.activation_credentials_bound = true;
        validate_create_request(state, &request).map_err(Failure::from)?;
        let endpoint = state.plugin.definition.factories[state.index]
            .activation_credential_context(&request)
            .map_err(|_| {
                Failure::protocol("invalid activation credential endpoint configuration")
            })?;
        endpoint.validate().map_err(Failure::from)?;
        wire::encode(&endpoint).map_err(Failure::from)
    })
}
