// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! An extension symbol alone is a malformed native family, never a legacy plugin.

use drasi_computation_plugin_abi::{self as abi, bootstrap_coordination::*};

#[no_mangle]
pub extern "C" fn drasi_computation_plugin_bootstrap_coordination_v1(
) -> *const PluginBootstrapCoordinationV1 {
    static TABLE: PluginBootstrapCoordinationV1 = PluginBootstrapCoordinationV1 {
        header: abi::Header::new::<PluginBootstrapCoordinationV1>(),
        version: VERSION,
        reserved: 0,
        source_factory: None,
        bootstrap_factory: None,
        create: None,
        create_source: None,
        create_bootstrap: None,
    };
    &TABLE
}
