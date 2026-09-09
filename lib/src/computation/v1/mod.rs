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

//! Version 1 of the experimental, default-off computation contracts.
//!
//! Enable with `drasi-lib = { version = "0.9", features = ["computation"] }`.
//! These are in-process contracts for a future acyclic graph host, not a runnable
//! graph API. Legacy source/query/reaction execution remains unchanged.
//!
//! Records contain immutable, schema-defined bytes. A [`RecordValidator`] must
//! check their encoding, domain constraints, and embedded identity where applicable.
//! [`RecordReference`] validates schema-specific keys even for image-less deletes.
//! The library additionally checks complete schema descriptors, operation ordering,
//! image roles, and cross-image identity. Providers are responsible for the truth
//! of their validators and capability declarations.
//!
//! None of these types implements automatic serialization. Record encoding is
//! schema-specific, not a Drasi wire standard. Lossless legacy codecs/adapters,
//! graph execution, durable delivery, replay, and SDK/ABI integration are separate
//! work. Internal canonical identity bytes are not a proven reversible codec.
//!
//! ```
//! use drasi_lib::computation::v1::{
//!     PipeCapabilities, PipeCapability, PipeRequirements, SchemaVersion,
//! };
//! use std::num::NonZeroUsize;
//!
//! assert!(SchemaVersion::try_new(0).is_err());
//! let pipe = PipeCapabilities::volatile_bounded(NonZeroUsize::new(16).unwrap());
//! let durable = PipeRequirements::new([PipeCapability::DurableAcceptance]);
//! assert!(pipe.validate(&durable).is_err());
//! ```

mod component;
mod data;
mod envelope;
mod error;
mod pipe;
mod ports;

pub use component::*;
pub use data::*;
pub use envelope::*;
pub use error::{ContractError, Result};
pub use pipe::*;
pub use ports::*;
