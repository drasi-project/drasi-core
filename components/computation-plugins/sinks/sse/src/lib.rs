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

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod lifecycle_ctx {
    use drasi_computation_plugin_sdk::lifecycle::{
        ApplyContext, CancellationToken, DisposeContext, DisposeReason, InitializeContext,
        OperationScope, PauseContext, ResumeContext, StartContext, StateIntent, StopContext,
        StopMode, UpdateBoundary,
    };

    pub(crate) fn scope() -> OperationScope {
        OperationScope::new("test", "component", 1, None, CancellationToken::new())
    }
    pub(crate) fn initialize() -> InitializeContext {
        InitializeContext::new(scope())
    }
    pub(crate) fn start() -> StartContext {
        StartContext::new(scope(), None)
    }
    pub(crate) fn start_with(
        credentials: drasi_computation_plugin_sdk::ActivationCredentials,
    ) -> StartContext {
        StartContext::new(scope(), Some(credentials))
    }
    pub(crate) fn pause() -> PauseContext {
        PauseContext::new(scope())
    }
    pub(crate) fn resume() -> ResumeContext {
        ResumeContext::new(scope())
    }
    pub(crate) fn stop() -> StopContext {
        StopContext::new(scope(), StopMode::Drain)
    }
    pub(crate) fn dispose() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Shutdown, StateIntent::Preserve, 1)
            .expect("valid dispose context")
    }
    pub(crate) fn erase() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
            .expect("valid dispose context")
    }
    pub(crate) fn apply(boundary: UpdateBoundary) -> ApplyContext {
        ApplyContext::new(scope(), boundary)
    }
}
