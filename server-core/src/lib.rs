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

pub mod application;
pub mod bootstrap;
pub mod channels;
pub mod config;
pub mod error;
pub mod queries;
pub mod reactions;
pub mod routers;
pub mod server_core;
pub mod sources;
pub mod utils;

#[cfg(test)]
mod test_support;

// Main exports for library users
pub use server_core::DrasiServerCore;
// DrasiServerCore now supports start()/stop() for library mode control

// Re-export commonly used types
pub use application::ApplicationHandle;
pub use channels::{ComponentEvent, ComponentStatus, QueryResult};
pub use config::{DrasiServerCoreConfig, QueryConfig, ReactionConfig, RuntimeConfig, SourceConfig};
pub use error::DrasiError;
pub use queries::{Query, QueryManager};
pub use reactions::application::SubscriptionOptions;
pub use reactions::ApplicationReactionHandle;
pub use reactions::{Reaction, ReactionManager};
pub use sources::application::PropertyMapBuilder;
pub use sources::{ApplicationSourceHandle, Source, SourceManager};
