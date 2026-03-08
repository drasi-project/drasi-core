#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Sui DeepBook bootstrap provider for Drasi.
//!
//! This provider loads historical DeepBook events from Sui JSON-RPC using pagination
//! and emits them as bootstrap events.

pub mod config;
pub mod descriptor;

pub use config::{
    StartPosition, SuiDeepBookBootstrapConfig, DEFAULT_DEEPBOOK_PACKAGE_ID, DEFAULT_SUI_MAINNET_RPC,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_sui_deepbook::mapping::{map_event_to_insert_change, should_include_event};
use drasi_source_sui_deepbook::rpc::SuiRpcClient;
use log::info;
use std::sync::Arc;

pub struct SuiDeepBookBootstrapProvider {
    config: SuiDeepBookBootstrapConfig,
    source_id: String,
}

impl SuiDeepBookBootstrapProvider {
    pub fn new(source_id: impl Into<String>, config: SuiDeepBookBootstrapConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            source_id: source_id.into(),
        })
    }

    pub fn builder() -> SuiDeepBookBootstrapProviderBuilder {
        SuiDeepBookBootstrapProviderBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for SuiDeepBookBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let source_id = if context.source_id.is_empty() {
            self.source_id.as_str()
        } else {
            context.source_id.as_str()
        };
        info!(
            "Starting Sui DeepBook bootstrap for source '{}' and query '{}'",
            source_id, request.query_id
        );

        if matches!(self.config.start_position, StartPosition::Now) {
            info!("Bootstrap start_position=Now, skipping historical load");
            return Ok(0);
        }

        let rpc_client = SuiRpcClient::new(self.config.rpc_endpoint.clone())?;
        let query_filter = serde_json::json!({ "All": [] });

        let timestamp_floor = match self.config.start_position {
            StartPosition::Timestamp(value) => Some(value.max(0) as u64),
            _ => None,
        };

        let mut cursor = None;
        let mut page_count = 0u32;
        let mut sent_events = 0usize;

        while page_count < self.config.max_pages {
            let page = rpc_client
                .query_events(
                    query_filter.clone(),
                    cursor.as_ref(),
                    self.config.request_limit,
                    false,
                )
                .await?;
            page_count += 1;

            for event in page.data {
                cursor = Some(event.id.clone());

                if event.package_id != self.config.deepbook_package_id {
                    continue;
                }

                if let Some(ts_floor) = timestamp_floor {
                    if event
                        .timestamp_ms
                        .is_some_and(|timestamp| timestamp < ts_floor)
                    {
                        continue;
                    }
                }
                if !should_include_event(&event, &self.config.event_filters, &self.config.pools) {
                    continue;
                }

                let change = map_event_to_insert_change(source_id, &event);
                event_tx
                    .send(BootstrapEvent {
                        source_id: source_id.to_string(),
                        change,
                        timestamp: chrono::Utc::now(),
                        sequence: context.next_sequence(),
                    })
                    .await
                    .map_err(|e| anyhow!("Failed to send Sui DeepBook bootstrap event: {e}"))?;
                sent_events += 1;
            }

            // Always advance cursor from next_cursor when present, even if all
            // events on this page were filtered out.
            if let Some(next_cursor) = page.next_cursor {
                cursor = Some(next_cursor);
            }

            if !page.has_next_page {
                break;
            }
        }

        info!(
            "Completed Sui DeepBook bootstrap for source '{source_id}': sent {sent_events} records in {page_count} pages"
        );
        Ok(sent_events)
    }
}

pub struct SuiDeepBookBootstrapProviderBuilder {
    source_id: String,
    config: SuiDeepBookBootstrapConfig,
}

impl SuiDeepBookBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            source_id: "sui-deepbook-bootstrap".to_string(),
            config: SuiDeepBookBootstrapConfig::default(),
        }
    }

    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = source_id.into();
        self
    }

    pub fn with_rpc_endpoint(mut self, rpc_endpoint: impl Into<String>) -> Self {
        self.config.rpc_endpoint = rpc_endpoint.into();
        self
    }

    pub fn with_deepbook_package_id(mut self, package_id: impl Into<String>) -> Self {
        self.config.deepbook_package_id = package_id.into();
        self
    }

    pub fn with_request_limit(mut self, request_limit: u16) -> Self {
        self.config.request_limit = request_limit;
        self
    }

    pub fn with_max_pages(mut self, max_pages: u32) -> Self {
        self.config.max_pages = max_pages;
        self
    }

    pub fn with_event_filters(mut self, event_filters: Vec<String>) -> Self {
        self.config.event_filters = event_filters;
        self
    }

    pub fn with_pools(mut self, pools: Vec<String>) -> Self {
        self.config.pools = pools;
        self
    }

    pub fn with_start_position(mut self, start_position: StartPosition) -> Self {
        self.config.start_position = start_position;
        self
    }

    pub fn with_start_from_beginning(mut self) -> Self {
        self.config.start_position = StartPosition::Beginning;
        self
    }

    pub fn with_start_from_now(mut self) -> Self {
        self.config.start_position = StartPosition::Now;
        self
    }

    pub fn with_start_from_timestamp(mut self, timestamp_ms: i64) -> Self {
        self.config.start_position = StartPosition::Timestamp(timestamp_ms);
        self
    }

    pub fn with_config(mut self, config: SuiDeepBookBootstrapConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> Result<SuiDeepBookBootstrapProvider> {
        SuiDeepBookBootstrapProvider::new(self.source_id, self.config)
    }
}

impl Default for SuiDeepBookBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let provider = SuiDeepBookBootstrapProvider::builder().build().unwrap();
        assert_eq!(provider.source_id, "sui-deepbook-bootstrap");
        assert_eq!(provider.config.request_limit, 100);
        assert!(matches!(
            provider.config.start_position,
            StartPosition::Beginning
        ));
    }

    #[test]
    fn test_builder_custom_values() {
        let provider = SuiDeepBookBootstrapProvider::builder()
            .with_source_id("source-a")
            .with_rpc_endpoint("https://fullnode.testnet.sui.io:443")
            .with_deepbook_package_id("0xabc")
            .with_request_limit(25)
            .with_max_pages(5)
            .with_start_from_now()
            .build()
            .unwrap();

        assert_eq!(provider.source_id, "source-a");
        assert_eq!(
            provider.config.rpc_endpoint,
            "https://fullnode.testnet.sui.io:443"
        );
        assert_eq!(provider.config.deepbook_package_id, "0xabc");
        assert_eq!(provider.config.request_limit, 25);
        assert_eq!(provider.config.max_pages, 5);
        assert!(matches!(provider.config.start_position, StartPosition::Now));
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "sui-deepbook-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::SuiDeepBookBootstrapDescriptor],
);
