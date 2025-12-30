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

//! Redb-Based State Store Provider for Drasi
//!
//! This crate provides a persistent state store provider using [redb](https://docs.rs/redb),
//! an embedded key-value database written in pure Rust.
//!
//! # Features
//!
//! - **ACID Transactions**: All operations are atomic and durable
//! - **Persistent Storage**: Data survives restarts
//! - **Partitioned by Store ID**: Each plugin gets its own table
//! - **Thread-Safe**: Safe for concurrent access from multiple plugins
//! - **No External Dependencies**: Pure Rust implementation
//!
//! # Usage
//!
//! ```ignore
//! use drasi_state_store_redb::RedbStateStoreProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let state_store = RedbStateStoreProvider::new("/data/state.redb")?;
//! let drasi = DrasiLib::builder()
//!     .with_state_store_provider(Arc::new(state_store))
//!     .build()
//!     .await?;
//! ```
//!
//! # Database Structure
//!
//! The provider creates a single redb database file with a separate table for each
//! `store_id`. Tables are created dynamically when first accessed and remain in the
//! database for subsequent operations.
//!
//! Keys and values are stored as byte arrays (`&[u8]` / `Vec<u8>`).

mod provider;

pub use provider::RedbStateStoreProvider;
