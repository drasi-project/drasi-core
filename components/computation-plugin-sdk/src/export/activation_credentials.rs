// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::abi::activation_credentials::*;

pub(super) static EXTENSION: PluginActivationCredentialsV1 = PluginActivationCredentialsV1 {
    header: Header::new::<PluginActivationCredentialsV1>(),
    version: VERSION,
    reserved: 0,
    context: Some(context),
};

unsafe extern "C" fn context(state: *mut c_void, input: BorrowedBytes) -> Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state_ref::<FactoryState>(state)? };
        let mut request: CreateRequest =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? })
                .map_err(|_| Failure::protocol("invalid activation credential context request"))?;
        request.activation_credentials_bound = true;
        validate_create_request(state, &request).map_err(Failure::from)?;
        let endpoint = state.plugin.definition.factories[state.index]
            .activation_credentials()
            .ok_or_else(|| {
                Failure::new(
                    abi::status::UNSUPPORTED,
                    "native factory does not declare activation credentials",
                )
            })?
            .activation_credential_context(&request)
            .map_err(|_| {
                Failure::protocol("invalid activation credential endpoint configuration")
            })?;
        endpoint.validate().map_err(Failure::from)?;
        wire::encode(&endpoint).map_err(Failure::from)
    })
}
