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

//! Application bootstrap plugin for Drasi
//!
//! This plugin provides the Application bootstrap provider implementation for replaying
//! stored insert events during query subscription.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_application::ApplicationBootstrapProvider;
//!
//! // Using the builder - creates with isolated storage
//! let provider = ApplicationBootstrapProvider::builder().build();
//!
//! // Using the builder with shared storage
//! use std::sync::Arc;
//! use tokio::sync::RwLock;
//! use drasi_core::models::SourceChange;
//!
//! let shared_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
//! let provider = ApplicationBootstrapProvider::builder()
//!     .with_shared_data(shared_data)
//!     .build();
//!
//! // Or using the constructor directly
//! let provider = ApplicationBootstrapProvider::new();
//! ```

pub mod application;

pub use application::{ApplicationBootstrapProvider, ApplicationBootstrapProviderBuilder};

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::SourceChange;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[test]
    fn test_application_bootstrap_builder_isolated() {
        // Builder without shared data creates isolated provider
        let provider = ApplicationBootstrapProviderBuilder::new().build();
        // Provider should exist (we can't easily test internal state)
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_builder_with_shared_data() {
        // Builder with shared data creates connected provider
        let shared_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
        let provider = ApplicationBootstrapProviderBuilder::new()
            .with_shared_data(shared_data.clone())
            .build();
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_from_provider_method() {
        // Test using ApplicationBootstrapProvider::builder()
        let provider = ApplicationBootstrapProvider::builder().build();
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_new() {
        // Test using ApplicationBootstrapProvider::new()
        let provider = ApplicationBootstrapProvider::new();
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_with_shared_data() {
        // Test using ApplicationBootstrapProvider::with_shared_data()
        let shared_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
        let provider = ApplicationBootstrapProvider::with_shared_data(shared_data);
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_builder_default() {
        // Default builder should work
        let provider = ApplicationBootstrapProviderBuilder::default().build();
        let _ = provider;
    }

    #[test]
    fn test_application_bootstrap_provider_default() {
        // ApplicationBootstrapProvider::default() should work
        let provider = ApplicationBootstrapProvider::default();
        let _ = provider;
    }
}
