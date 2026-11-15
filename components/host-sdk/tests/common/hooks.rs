// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Lifecycle hook contexts for tests that drive components directly.
#![allow(dead_code)]

use drasi_lib::computation::v1::{
    ApplyContext, CancellationToken, DisposeContext, DisposeReason, InitializeContext,
    OperationScope, PauseContext, ResumeContext, StartContext, StateIntent, StopContext, StopMode,
    UpdateBoundary,
};

/// Each call gets a fresh operation id, as the graph would issue.
pub fn scope(operation: &str) -> OperationScope {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    OperationScope::new(
        format!("{operation}-{id}"),
        "component",
        1,
        None,
        CancellationToken::new(),
    )
}

pub fn initialize_ctx() -> InitializeContext {
    InitializeContext::new(scope("test-initialize"))
}

pub fn start_ctx() -> StartContext {
    StartContext::new(scope("test-start"))
}

pub fn pause_ctx() -> PauseContext {
    PauseContext::new(scope("test-pause"))
}

pub fn resume_ctx() -> ResumeContext {
    ResumeContext::new(scope("test-resume"))
}

pub fn stop_ctx() -> StopContext {
    StopContext::new(scope("test-stop"), StopMode::Drain)
}

/// The former `dispose()`: release in-memory state, preserving owned state.
pub fn dispose_ctx() -> DisposeContext {
    DisposeContext::new(
        scope("test-dispose"),
        DisposeReason::Shutdown,
        StateIntent::Preserve,
        1,
    )
    .expect("valid dispose context")
}

/// The former `delete_owned_resources()`: removal that erases owned state.
pub fn erase_ctx() -> DisposeContext {
    DisposeContext::new(
        scope("test-erase"),
        DisposeReason::Removal,
        StateIntent::Erase,
        1,
    )
    .expect("valid erase context")
}

pub fn apply_ctx(boundary: UpdateBoundary) -> ApplyContext {
    ApplyContext::new(scope("test-apply"), boundary)
}

/// Contexts for driving SDK components directly.
pub mod sdk {
    use drasi_computation_plugin_sdk::lifecycle::{StartContext, StopContext, StopMode};

    pub fn start_ctx() -> StartContext {
        StartContext::new(super::scope("test-start"), None)
    }

    pub fn stop_ctx() -> StopContext {
        StopContext::new(super::scope("test-stop"), StopMode::Drain)
    }
}
