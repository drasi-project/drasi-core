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
//! It defines the core traits that all plugins must implement.
//!
//! # Architecture
//!
//! The plugin core separates the plugin contract from the manager implementations:
//! - **Source plugins**: Implement the `Source` trait to provide data ingestion
//! - **Reaction plugins**: Implement the `Reaction` trait to handle query results
//! - **Bootstrap plugins**: Implement the `BootstrapProvider` trait for initial data delivery
//!
//! # Plugin Architecture
//!
//! drasi-lib has **zero awareness** of what plugins exist. It only knows about traits.
//! Each plugin:
//! 1. Defines its own typed configuration struct with builder pattern
//! 2. Creates base implementations using the params structs
//! 3. Implements the appropriate trait
//! 4. Is passed to DrasiLib as `Arc<dyn Trait>`
//!
//! # Usage
//!
//! Plugin developers should implement the appropriate trait(s) and use the base
//! implementations from `sources::base` and `reactions::common::base`:
//!
//! ```ignore
//! use drasi_lib::plugin_core::Source;
//! use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
//!
//! pub struct MySource {
//!     base: SourceBase,
//!     // custom fields...
//! }
//!
//! impl MySource {
//!     pub fn new(config: MyConfig, event_tx: ComponentEventSender) -> Result<Self> {
//!         let params = SourceBaseParams::new(&config.id);
//!         Ok(Self {
//!             base: SourceBase::new(params, event_tx)?,
//!         })
//!     }
//! }
//!
//! #[async_trait]
//! impl Source for MySource {
//!     fn id(&self) -> &str { &self.base.id }
//!     fn type_name(&self) -> &str { "my-source" }
//!     // ...
//! }
//! ```

pub mod bootstrap;
pub mod reaction;
pub mod source;

// Re-export core traits for convenience
// Note: Base implementations are in sources::base and reactions::common::base

// Source trait (the only thing drasi-lib knows about sources)
pub use source::Source;

// Reaction traits
pub use reaction::{QuerySubscriber, Reaction};

// Bootstrap traits
pub use bootstrap::BootstrapProvider;
