// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_contracts::GraphChangeCodec;
use drasi_computation_network_common::config;
use drasi_computation_plugin_sdk::PluginDefinition;
use std::sync::Arc;

pub use component::{sse_payload, SseFactory, SseSinkConfig};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const SSE_SINK: &str = "drasi.network/sse-sink";

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(SseFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());
