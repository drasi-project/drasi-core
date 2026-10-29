// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! HTTP source message definitions and conversion shared by ordinary and native
//! components. This crate contains no listener, component factory or plugin SDK.

#[cfg(feature = "adaptive")]
pub mod adaptive_batcher;
#[cfg(feature = "bootstrap")]
pub mod bootstrap;
mod models;
#[cfg(feature = "reaction")]
pub mod reaction;
pub mod time;
#[cfg(feature = "webhook")]
pub mod webhook;

pub use models::{convert_http_to_source_change, HttpElement, HttpSourceChange};
