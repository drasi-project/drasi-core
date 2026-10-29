// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared graph elements, result values and failure-survival descriptions.
//! There is no query evaluator, language parser, storage backend or runtime here.

extern crate alloc;

pub mod data_value;
pub mod evaluation;
pub mod interface;
pub mod models;
pub mod profiling;
pub mod query;
pub mod schema;
