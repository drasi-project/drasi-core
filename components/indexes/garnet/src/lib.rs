// Copyright 2024 The Drasi Authors.
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

#![allow(unexpected_cfgs)]

//! Garnet/Redis Index Backend for Drasi
//!
//! This crate provides a distributed storage backend for Drasi queries using Redis/Garnet.
//!
//! # Usage
//!
//! ```ignore
//! use drasi_index_garnet::GarnetIndexProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let provider = GarnetIndexProvider::new("redis://localhost:6379", None, true);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider("redis", Arc::new(provider))
//!     .build()?;
//! ```

pub mod checkpoint;
#[cfg(feature = "plugin-descriptor")]
mod descriptor;
pub mod element_index;
pub mod future_queue;
pub mod live_results;
pub mod outbox;
mod plugin;
pub mod result_index;
pub(crate) mod session_state;
mod storage_models;

// Re-export the plugin provider for easy access
pub use checkpoint::GarnetCheckpointStore;
pub use live_results::GarnetLiveResultsWriter;
pub use outbox::GarnetOutboxWriter;
pub use plugin::GarnetIndexProvider;
pub use session_state::{GarnetSessionControl, GarnetSessionState};

#[cfg(feature = "plugin-descriptor")]
pub use descriptor::{GarnetIndexConfigDto, GarnetIndexDescriptor};
