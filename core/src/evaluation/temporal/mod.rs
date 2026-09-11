// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Persistable temporal state. Evaluation and queue dispatch are separate from this module.

pub mod codec;
pub(crate) mod queue;
pub(crate) mod result_index;
pub mod runtime;
mod state;
mod value;

#[cfg(test)]
pub(crate) mod fixtures;

pub use state::*;
pub use result_index::initialize_empty_query_store;

use serde::{Deserialize, Serialize};

/// A persisted query lifetime. Rebuilding temporal history requires a new epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QueryEpoch(pub [u8; 16]);

/// The executable plan format, independent of the storage codec version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlanVersion(pub u32);
