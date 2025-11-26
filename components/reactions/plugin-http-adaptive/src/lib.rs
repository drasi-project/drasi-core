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

//! HTTP Adaptive reaction plugin for Drasi
//!
//! This plugin implements HTTP Adaptive reactions for Drasi and provides extension traits
//! for configuring HTTP Adaptive reactions in the Drasi plugin architecture.

pub mod config;
pub mod http_adaptive;

pub use config::HttpAdaptiveReactionConfig;
pub use http_adaptive::AdaptiveHttpReaction;
pub use drasi_lib::config::ReactionConfig;

/// Extension trait for creating HTTP Adaptive reactions
///
/// This trait is implemented on `ReactionConfig` to provide a fluent builder API
/// for configuring HTTP Adaptive reactions.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::config::ReactionConfig;
/// use drasi_plugin_http_adaptive_reaction::ReactionConfigHttpAdaptiveExt;
///
/// let config = ReactionConfig::http_adaptive()
///     .with_base_url("http://api.example.com")
///     .with_max_batch_size(100)
///     .with_timeout_ms(5000)
///     .build();
/// ```
pub trait ReactionConfigHttpAdaptiveExt {
    /// Create a new HTTP Adaptive reaction configuration builder
    fn http_adaptive() -> HttpAdaptiveReactionBuilder;
}

/// Builder for HTTP Adaptive reaction configuration
pub struct HttpAdaptiveReactionBuilder {
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    adaptive_min_batch_size: usize,
    adaptive_max_batch_size: usize,
    adaptive_batch_timeout_ms: u64,
    adaptive_window_size: usize,
}

impl Default for HttpAdaptiveReactionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpAdaptiveReactionBuilder {
    /// Create a new HTTP Adaptive reaction builder with default values
    pub fn new() -> Self {
        Self {
            base_url: "http://localhost".to_string(),
            token: None,
            timeout_ms: 5000,
            adaptive_min_batch_size: 1,
            adaptive_max_batch_size: 100,
            adaptive_batch_timeout_ms: 1000,
            adaptive_window_size: 10,
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

    /// Set the maximum batch size for adaptive batching
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive_max_batch_size = size;
        self
    }

    /// Set the minimum batch size for adaptive batching
    pub fn with_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive_min_batch_size = size;
        self
    }

    /// Set the batch timeout in milliseconds for adaptive batching
    pub fn with_batch_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.adaptive_batch_timeout_ms = timeout_ms;
        self
    }

    /// Set the adaptive window size
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.adaptive_window_size = size;
        self
    }

    /// Build the HTTP Adaptive reaction configuration
    pub fn build(self) -> HttpAdaptiveReactionConfig {
        use drasi_lib::reactions::common::AdaptiveBatchConfig;
        HttpAdaptiveReactionConfig {
            base_url: self.base_url,
            token: self.token,
            timeout_ms: self.timeout_ms,
            routes: Default::default(),
            adaptive: AdaptiveBatchConfig {
                adaptive_min_batch_size: self.adaptive_min_batch_size,
                adaptive_max_batch_size: self.adaptive_max_batch_size,
                adaptive_window_size: self.adaptive_window_size,
                adaptive_batch_timeout_ms: self.adaptive_batch_timeout_ms,
            },
        }
    }
}

impl ReactionConfigHttpAdaptiveExt for ReactionConfig {
    fn http_adaptive() -> HttpAdaptiveReactionBuilder {
        HttpAdaptiveReactionBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_adaptive_builder_defaults() {
        let config = HttpAdaptiveReactionBuilder::new().build();
        assert_eq!(config.base_url, "http://localhost");
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.token, None);
        assert_eq!(config.adaptive.adaptive_max_batch_size, 100);
    }

    #[test]
    fn test_http_adaptive_builder_custom_values() {
        let config = HttpAdaptiveReactionBuilder::new()
            .with_base_url("http://api.example.com")
            .with_token("secret-token")
            .with_timeout_ms(10000)
            .with_max_batch_size(100)
            .with_min_batch_size(10)
            .with_batch_timeout_ms(500)
            .build();

        assert_eq!(config.base_url, "http://api.example.com");
        assert_eq!(config.token, Some("secret-token".to_string()));
        assert_eq!(config.timeout_ms, 10000);
        assert_eq!(config.adaptive.adaptive_max_batch_size, 100);
        assert_eq!(config.adaptive.adaptive_min_batch_size, 10);
        assert_eq!(config.adaptive.adaptive_batch_timeout_ms, 500);
    }

    #[test]
    fn test_http_adaptive_builder_fluent_api() {
        let config = ReactionConfig::http_adaptive()
            .with_base_url("http://localhost:8080")
            .with_max_batch_size(50)
            .build();

        assert_eq!(config.base_url, "http://localhost:8080");
        assert_eq!(config.adaptive.adaptive_max_batch_size, 50);
    }
}
