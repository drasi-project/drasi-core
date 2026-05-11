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

//! Redb-backed Write-Ahead Log implementation for Drasi transient sources.
//!
//! This crate implements the [`WalProvider`](drasi_lib::WalProvider) trait
//! using [redb](https://docs.rs/redb) for durable event storage. Each
//! registered source gets its own redb file at `{root_dir}/{source_id}.redb`.
//!
//! Events are serialized via explicit DTOs (not by deriving serde on core
//! domain types), keeping the wire format decoupled from the core model.

mod dto;
mod provider;

#[cfg(test)]
mod tests;

pub use provider::RedbWalProvider;
