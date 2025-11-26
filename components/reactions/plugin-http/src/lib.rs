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

//! HTTP reaction plugin for Drasi
//!
//! This plugin implements HTTP reactions for Drasi and provides extension traits
//! for configuring HTTP reactions in the Drasi plugin architecture.

pub mod config;
pub mod http;

pub use config::{CallSpec, HttpReactionConfig, QueryConfig};
pub use http::HttpReaction;
pub use drasi_lib::config::ReactionConfig;

/// Extension trait for creating HTTP reactions
///
/// This trait is implemented on `ReactionConfig` to provide a fluent builder API
/// for configuring HTTP reactions.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::config::ReactionConfig;
/// use drasi_plugin_http_reaction::ReactionConfigHttpExt;
///
/// let config = ReactionConfig::http()
///     .with_base_url("http://api.example.com")
///     .with_timeout_ms(5000)
///     .build();
/// ```
pub trait ReactionConfigHttpExt {
    /// Create a new HTTP reaction configuration builder
    fn http() -> HttpReactionBuilder;
}

/// Builder for HTTP reaction configuration
pub struct HttpReactionBuilder {
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
}

impl Default for HttpReactionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpReactionBuilder {
    /// Create a new HTTP reaction builder with default values
    pub fn new() -> Self {
        Self {
            base_url: "http://localhost".to_string(),
            token: None,
            timeout_ms: 5000,
        }
    }

    /// Set the base URL for HTTP requests
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the authentication token
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Build the HTTP reaction configuration
    pub fn build(self) -> HttpReactionConfig {
        HttpReactionConfig {
            base_url: self.base_url,
            token: self.token,
            timeout_ms: self.timeout_ms,
            routes: Default::default(),
        }
    }
}

impl ReactionConfigHttpExt for ReactionConfig {
    fn http() -> HttpReactionBuilder {
        HttpReactionBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_builder_defaults() {
        let config = HttpReactionBuilder::new().build();
        assert_eq!(config.base_url, "http://localhost");
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.token, None);
    }

    #[test]
    fn test_http_builder_custom_values() {
        let config = HttpReactionBuilder::new()
            .with_base_url("http://api.example.com")
            .with_token("secret-token")
            .with_timeout_ms(10000)
            .build();

        assert_eq!(config.base_url, "http://api.example.com");
        assert_eq!(config.token, Some("secret-token".to_string()));
        assert_eq!(config.timeout_ms, 10000);
    }

    #[test]
    fn test_http_builder_fluent_api() {
        let config = ReactionConfig::http()
            .with_base_url("http://localhost:8080")
            .with_timeout_ms(3000)
            .build();

        assert_eq!(config.base_url, "http://localhost:8080");
        assert_eq!(config.timeout_ms, 3000);
    }
}
