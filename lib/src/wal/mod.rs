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

//! Write-Ahead Log plugin contract.
//!
//! Defines the [`WalProvider`] trait for durable event logging used by
//! transient sources (HTTP, gRPC, etc.) for crash recovery. External crates
//! (e.g., `drasi-wal-redb`) provide concrete implementations.

mod config;
mod error;
mod traits;

pub use config::{CapacityPolicy, WriteAheadLogConfig, MIN_MAX_EVENTS};
pub use error::WalError;
pub use traits::WalProvider;
