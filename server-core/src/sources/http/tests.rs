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

//! Integration tests for HTTP source
//!
//! Note: Unit tests for data schema conversion are in direct_format.rs

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::config::SourceConfig;
    use crate::sources::Source;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_http_source_start_stop() {
        use crate::config::SourceSpecificConfig;
        use crate::sources::http::HttpSourceConfig;

        let config = SourceConfig {
            id: "test-http-source".to_string(),
            auto_start: false,
            config: SourceSpecificConfig::Http(HttpSourceConfig {
                host: "127.0.0.1".to_string(),
                port: 9999,
                endpoint: None,
                timeout_ms: 30000,
                adaptive_enabled: None,
                adaptive_max_batch_size: None,
                adaptive_min_batch_size: None,
                adaptive_max_wait_ms: None,
                adaptive_min_wait_ms: None,
                adaptive_window_secs: None,
            }),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        };

        let (event_tx, _event_rx) = mpsc::channel(100);

        let source = HttpSource::new(config, event_tx).unwrap();

        // Test initial status
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Stopped
        );

        // Test start
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Running
        );

        // Test stop
        source.stop().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Stopped
        );
    }
}
