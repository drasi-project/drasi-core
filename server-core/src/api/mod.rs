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

//! # Drasi Server Core API
//!
//! This module provides the public API for configuring and running Drasi Server Core.
//!
//! ## Quick Start
//!
//! ```no_run
//! use drasi_server_core::DrasiServerCore;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Build and start server from config file
//!     let core = DrasiServerCore::from_config_file("config.yaml").await?;
//!     core.start().await?;
//!
//!     // Get handles to interact with components
//!     let source_handle = core.source_handle("my-source")?;
//!     let reaction_handle = core.reaction_handle("my-reaction")?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Programmatic Configuration
//!
//! ```no_run
//! use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let core = DrasiServerCore::builder()
//!         .with_id("my-server")
//!         .add_source(
//!             Source::application("orders")
//!                 .auto_start(true)
//!                 .build()
//!         )
//!         .add_query(
//!             Query::cypher("active-orders")
//!                 .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
//!                 .from_source("orders")
//!                 .build()
//!         )
//!         .add_reaction(
//!             Reaction::application("notifications")
//!                 .subscribe_to("active-orders")
//!                 .build()
//!         )
//!         .build()
//!         .await?;
//!
//!     core.start().await?;
//!     Ok(())
//! }
//! ```

mod builder;
mod error;
mod handles;
mod properties;
mod query;
mod reaction;
mod source;

pub use builder::DrasiServerCoreBuilder;
pub use error::{DrasiError, Result};
pub use handles::HandleRegistry;
pub use properties::Properties;
pub use query::{Query, QueryBuilder};
pub use reaction::{Reaction, ReactionBuilder};
pub use source::{Source, SourceBuilder};
