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

//! Source payload mapping engine for Drasi.
//!
//! Transforms arbitrary JSON payloads into graph change events (`SourceChange`)
//! using Handlebars templates. This crate provides a shared mapping mechanism
//! used by sources that receive arbitrary payloads (HTTP webhooks, Kafka messages, etc.).
//!
//! # Overview
//!
//! The mapping engine takes:
//! - A `SourceMapping` configuration defining how to extract graph elements from payloads
//! - A `serde_json::Value` context containing the payload and any source-specific metadata
//! - A source ID string
//!
//! And produces `SourceChange` events (Insert, Update, Delete) that can be dispatched
//! to the Drasi query engine.

mod config;
mod engine;

pub use config::{
    EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
    SourceMapping, TimestampFormat,
};
pub use engine::{json_to_element_value, SourceMappingEngine};
