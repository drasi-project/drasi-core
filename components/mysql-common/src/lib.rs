#![allow(unexpected_cfgs)]
// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Shared types for MySQL source and bootstrap plugins
//!
//! This crate contains configuration, key formatting, and identifier
//! validation types used by both `drasi-source-mysql` and `drasi-bootstrap-mysql`.

pub mod config;
pub mod keys;

// Re-export main types
pub use config::{is_valid_identifier, TableKeyConfig};
pub use keys::{escape_identifier, format_value_for_key, quote_identifier};
