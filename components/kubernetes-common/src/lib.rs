#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Shared types for Kubernetes source and bootstrap plugins
//!
//! This crate contains configuration, mapping, property extraction, and
//! client-building types used by both `drasi-source-kubernetes` and
//! `drasi-bootstrap-kubernetes`.

pub mod client;
pub mod config;
pub mod mapping;
pub mod properties;

// Re-export main types
pub use client::{build_client, parse_api_version};
pub use config::{
    default_annotation_excludes, is_cluster_scoped_kind, is_supported_kind, AuthMode,
    KubernetesSourceConfig, ResourceSpec, StartFrom,
};
