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

//! Shared building blocks for the Drasi [Agent Host Protocol][ahp] (AHP)
//! source and bootstrap plugins.
//!
//! - [`model`] — graph labels, element-ID generation, and projected elements.
//!   The source and bootstrapper MUST use these so their IDs agree.
//! - [`mirror`] — a local mirror of host state, updated with the official
//!   `ahp` reducers, and a per-scope projection that diffs state into
//!   `SourceChange`s.
//! - [`client`] — a lossless JSON-RPC 2.0 client over WebSocket.
//! - [`sync`] — connection settings and [`sync::AhpSession`], which loads a
//!   full snapshot and keeps the mirror current.
//! - `mock` (feature `mock-host`) — an in-process AHP host for tests/demos.
//!
//! [ahp]: https://github.com/microsoft/agent-host-protocol

pub mod client;
pub mod mirror;
#[cfg(feature = "mock-host")]
pub mod mock;
pub mod model;
pub mod sync;

pub use ahp_types;
pub use mirror::{AhpMirror, ElementChange, GraphProjection, Scope};
pub use model::{labels, relations, ProjectedElement, SessionFilter};
pub use sync::{is_fatal, AhpClientConfig, AhpSession, FatalAhpError, ResourceToken};
