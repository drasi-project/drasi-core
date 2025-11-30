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

//! Plugin core module for bootstrap provider abstractions
//!
//! This module re-exports the bootstrap provider trait for use in the plugin architecture.
//!
//! # Plugin Architecture
//!
//! Bootstrap providers are created by source plugins using their own typed configurations.
//! drasi-lib only knows about the `BootstrapProvider` trait - it has no knowledge of
//! which bootstrap provider implementations exist.
//!
//! # Example
//!
//! ```ignore
//! use drasi_lib::plugin_core::BootstrapProvider;
//!
//! pub struct MyBootstrapProvider {
//!     config: MyBootstrapConfig,
//! }
//!
//! impl MyBootstrapProvider {
//!     pub fn new(config: MyBootstrapConfig) -> Self {
//!         Self { config }
//!     }
//! }
//!
//! #[async_trait]
//! impl BootstrapProvider for MyBootstrapProvider {
//!     async fn bootstrap(
//!         &self,
//!         request: BootstrapRequest,
//!         context: &BootstrapContext,
//!         event_tx: BootstrapEventSender,
//!     ) -> Result<usize> {
//!         // Implementation
//!     }
//! }
//!
//! // Source plugin creates the provider and transfers ownership:
//! let provider = MyBootstrapProvider::new(config);
//! source_base.set_bootstrap_provider(provider).await;  // Ownership transferred
//! ```

// Re-export the existing BootstrapProvider trait from the bootstrap module
pub use crate::bootstrap::BootstrapProvider;
