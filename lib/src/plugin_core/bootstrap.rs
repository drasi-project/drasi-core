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
//! This module re-exports the bootstrap provider trait and factory
//! for use in the plugin architecture.

use anyhow::Result;

// Re-export the existing BootstrapProvider trait from the bootstrap module
pub use crate::bootstrap::BootstrapProvider;
use crate::bootstrap::BootstrapProviderConfig;

/// Factory for creating bootstrap provider instances from configuration
pub struct BootstrapFactory;

impl BootstrapFactory {
    /// Create a bootstrap provider instance from configuration
    ///
    /// This delegates to the existing BootstrapProviderFactory for now.
    /// In the future, this will support plugins.
    pub fn create(config: &BootstrapProviderConfig) -> Result<Box<dyn BootstrapProvider>> {
        use crate::bootstrap::BootstrapProviderFactory;
        BootstrapProviderFactory::create_provider(config)
    }
}
