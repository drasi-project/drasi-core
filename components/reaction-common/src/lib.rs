// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Reaction configuration and rendering shared by legacy and native components.

pub mod config;
pub mod templates;

#[cfg(feature = "sse")]
pub mod sse;

pub use config::AdaptiveBatchConfig;
pub use templates::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
