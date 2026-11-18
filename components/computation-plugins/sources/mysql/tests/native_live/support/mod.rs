// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only in-process adapters over the shared MySQL protocol engine.
//!
//! These drive the engine (`drasi_mysql_common::native`) directly as DrasiLib
//! components, so the live tests can exercise engine behaviour without a
//! graph. They are not product API: graphs build MySQL sources from the
//! plugin's declared factories (`drasi_computation_source_mysql::plugin`).

mod snapshot;
mod source;
pub use snapshot::MySqlSnapshot;
pub use source::MySqlSource;

#[allow(dead_code)]
pub mod lifecycle_ctx {
    use drasi_lib::computation::v1::{
        CancellationToken, DisposeContext, DisposeReason, OperationScope, PauseContext,
        ResumeContext, StartContext, StateIntent, StopContext, StopMode,
    };

    /// Keeps a context alive for futures that outlive the calling statement.
    pub fn held<T>(value: T) -> &'static T {
        Box::leak(Box::new(value))
    }
    pub fn scope() -> OperationScope {
        OperationScope::new("test", "component", 1, None, CancellationToken::new())
    }
    pub fn start() -> StartContext {
        StartContext::new(scope())
    }
    pub fn pause() -> PauseContext {
        PauseContext::new(scope())
    }
    pub fn resume() -> ResumeContext {
        ResumeContext::new(scope())
    }
    pub fn stop() -> StopContext {
        StopContext::new(scope(), StopMode::Drain)
    }
    pub fn erase() -> DisposeContext {
        DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
            .expect("valid dispose context")
    }
}
