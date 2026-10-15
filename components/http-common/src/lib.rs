// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! HTTP source message definitions and conversion shared by ordinary and native
//! components. This crate contains no listener, component factory or plugin SDK.

mod models;
pub mod time;

pub use models::{convert_http_to_source_change, HttpElement, HttpSourceChange};
