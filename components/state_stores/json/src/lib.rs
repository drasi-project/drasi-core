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

//! JSON File-Based State Store Provider for Drasi
//!
//! This crate provides a persistent state store provider that stores data in JSON files.
//! Each store partition (identified by `store_id`) is stored in a separate JSON file.
//!
//! # Features
//!
//! - **Persistent Storage**: Data survives restarts
//! - **Partitioned by Store ID**: Each plugin gets its own JSON file
//! - **Thread-Safe**: Safe for concurrent access from multiple plugins
//! - **Automatic Cleanup**: Empty files are removed when all data is deleted
//!
//! # Usage
//!
//! ```ignore
//! use drasi_state_store_json::JsonStateStoreProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let state_store = JsonStateStoreProvider::new("/data/state")?;
//! let drasi = DrasiLib::builder()
//!     .with_state_store_provider(Arc::new(state_store))
//!     .build()
//!     .await?;
//! ```
//!
//! # File Structure
//!
//! Files are stored in the configured directory with the following naming convention:
//! ```text
//! {directory}/
//!   {store_id}.json
//! ```
//!
//! Each JSON file contains a map of keys to base64-encoded values:
//! ```json
//! {
//!   "key1": "YmFzZTY0IGVuY29kZWQgdmFsdWU=",
//!   "key2": "YW5vdGhlciB2YWx1ZQ=="
//! }
//! ```

mod provider;

pub use provider::JsonStateStoreProvider;
