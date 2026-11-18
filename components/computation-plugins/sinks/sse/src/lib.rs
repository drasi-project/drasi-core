// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! `drasi.network/sse-sink`: a native ComputationGraph sink that serves
//! volatile graph-change notifications to browsers over Server-Sent Events,
//! with optional output-snapshot reads. See DESIGN.md and CONFIGURATION.md.

mod component;
mod config;
mod render;

use drasi_computation_contracts::GraphChangeCodec;
use drasi_computation_plugin_sdk::PluginDefinition;
use std::sync::Arc;

pub use component::{sse_payload, SseFactory};
pub use config::SseSinkConfig;

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

#[cfg(test)]
mod tests;
