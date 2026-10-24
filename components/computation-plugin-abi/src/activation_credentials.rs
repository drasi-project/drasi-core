// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional activation-credentials protocol. Base tables and wire version 2 are
//! unchanged. A supporting factory accepts the versioned creation envelope on
//! its existing create callbacks, and bound instances accept credential payloads
//! only on their normal, owned START operation. No credential-fetch RPC exists.
//!
//! All payloads are bounded named MessagePack. A bound creation is
//! `{activation_credentials_version: 1, request: <base CreateRequest>}`; the
//! ordinary request and immutable configuration are not modified. This envelope
//! also applies to source/bootstrap coordinator creation when identity-bound.
//! Bound START accepts `{kind: "usernamePassword", username, password}` or
//! `{kind: "token", username, token}`. Certificates and unknown fields/kinds are
//! rejected. Unbound START continues to require empty input.

use crate::{BorrowedBytes, Header, Reply};
use std::ffi::c_void;

pub const VERSION: u32 = 1;
pub const MAX_CREDENTIAL_FIELD_BYTES: usize = 64 * 1024;
pub const MAX_CREDENTIAL_PAYLOAD_BYTES: usize = 2 * MAX_CREDENTIAL_FIELD_BYTES + 1024;
pub const SYMBOL: &[u8] = b"drasi_computation_plugin_activation_credentials_v1\0";
pub type Entry = unsafe extern "C" fn() -> *const PluginActivationCredentialsV1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PluginActivationCredentialsV1 {
    pub header: Header,
    pub version: u32,
    pub reserved: u32,
    /// Given FactoryHandle.state, returns MessagePack bool indicating support.
    pub factory: Option<unsafe extern "C" fn(*mut c_void) -> Reply>,
    /// Given FactoryHandle.state and the base CreateRequest, purely inspects the
    /// configured endpoint, returning `{hostname: string, port: u16}`. Inspection
    /// must permit omitted literal auth because the Host binds an actual provider.
    /// Neither callback may acquire credentials.
    pub context: Option<unsafe extern "C" fn(*mut c_void, BorrowedBytes) -> Reply>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn activation_credentials_preserves_frozen_base_layouts_and_wire() {
        use std::mem::size_of;
        assert_eq!(
            size_of::<super::PluginActivationCredentialsV1>(),
            24 + 2 * size_of::<usize>()
        );
        assert_eq!(
            size_of::<crate::ComponentVTable>(),
            16 + 5 * size_of::<usize>()
        );
        assert_eq!(
            size_of::<crate::FactoryVTable>(),
            16 + 2 * size_of::<usize>()
        );
        assert_eq!(crate::ABI_VERSION, "1.0.0");
        assert_eq!(crate::WIRE_VERSION, 2);
    }
}
