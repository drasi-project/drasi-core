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

//! Read-only preparation for temporal dispatch and recovery.
//! Query-part reconciliation, queue mutation, and root completion are not implemented here.

pub mod deadlines;
pub mod due;
pub mod engine;
pub mod epochs;
pub mod frame;
pub mod functions;
pub mod program;
pub mod startup;

pub use engine::{TemporalInputChange, TemporalRuntime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalStartup {
    /// The caller created fresh query indexes, or explicitly reset and will rebuild history.
    Create,
    /// Existing state must have a readable catalog and a matching executable plan.
    Reopen,
}

use thiserror::Error;

use crate::interface::IndexError;

use super::{codec::TemporalCodecError, TemporalNamespace, TemporalStateError};

#[derive(Debug, Error)]
pub enum TemporalRuntimeError {
    #[error("the persisted temporal query differs from the requested plan; explicit history replay or snapshot-reset rebuild is required")]
    QueryPlanChanged,
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error(transparent)]
    Codec(#[from] TemporalCodecError),
    #[error(transparent)]
    State(#[from] TemporalStateError),
    #[error(transparent)]
    QueueHead(#[from] due::HeadChanged),
    #[error("temporal epoch already exists: {0:?}")]
    EpochAlreadyExists(TemporalNamespace),
    #[error("an explicit temporal reset must use a new query epoch")]
    EpochReuse,
    #[error("temporal index returned a different record than {0}")]
    UnexpectedRecord(&'static str),
    #[error("live temporal ticket has a missing or inconsistent {0}")]
    MissingDependency(&'static str),
}
