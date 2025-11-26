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

//! Plugin Core Module
//!
//! This module provides the foundational abstractions for the Drasi plugin architecture.
//! It defines the core traits and base implementations that all plugins must implement.
//!
//! # Architecture
//!
//! The plugin core separates the plugin contract from the manager implementations:
//! - **Source plugins**: Implement the `Source` trait to provide data ingestion
//! - **Reaction plugins**: Implement the `Reaction` trait to handle query results
//! - **Bootstrap plugins**: Implement the `BootstrapProvider` trait for initial data delivery
//!
//! # Usage
//!
//! Plugin developers should implement the appropriate trait(s) and use the base
//! implementations for common functionality:
//!
//! ```ignore
//! use drasi_lib::plugin_core::{Source, SourceBase};
//!
//! pub struct MySource {
//!     base: SourceBase,
//!     // custom fields...
//! }
//!
//! #[async_trait]
//! impl Source for MySource {
//!     async fn start(&self) -> Result<()> {
//!         // implementation...
//!     }
//!     // ...
//! }
//! ```

pub mod bootstrap;
pub mod reaction;
pub mod source;
pub mod registry;

// Re-export all public types for convenience

// Source abstractions
pub use source::{Source, SourceBase, SourceFactory};

// Reaction abstractions
pub use reaction::{QuerySubscriber, Reaction, ReactionBase, ReactionFactory};

// Bootstrap abstractions
pub use bootstrap::{BootstrapFactory, BootstrapProvider};

// Plugin registry
pub use registry::{ReactionRegistry, SourceRegistry};
