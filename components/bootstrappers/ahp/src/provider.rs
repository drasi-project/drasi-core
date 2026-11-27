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

//! AHP bootstrap provider implementation.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use drasi_ahp_common::model::now_ms;
use drasi_ahp_common::{AhpSession, GraphProjection, ProjectedElement, ResourceToken};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use log::info;

use crate::config::AhpBootstrapConfig;

/// Bootstrap provider that reads a full snapshot from an AHP host and emits
/// it as inserts, using the same graph mapping as the AHP source.
pub struct AhpBootstrapProvider {
    config: AhpBootstrapConfig,
}

impl AhpBootstrapProvider {
    /// Create a provider from configuration.
    pub fn new(config: AhpBootstrapConfig) -> Self {
        Self { config }
    }

    /// Start building a provider.
    pub fn builder() -> AhpBootstrapProviderBuilder {
        AhpBootstrapProviderBuilder::new()
    }

    /// The provider configuration.
    pub fn config(&self) -> &AhpBootstrapConfig {
        &self.config
    }
}

/// Builder for [`AhpBootstrapProvider`].
#[derive(Default)]
pub struct AhpBootstrapProviderBuilder {
    config: AhpBootstrapConfig,
}

impl AhpBootstrapProviderBuilder {
    /// Create a builder with default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole configuration.
    pub fn with_config(mut self, config: AhpBootstrapConfig) -> Self {
        self.config = config;
        self
    }

    /// AHP host WebSocket URL.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.config.url = url.into();
        self
    }

    /// Bearer token for the WebSocket upgrade request.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.config.bearer_token = Some(token.into());
        self
    }

    /// Client ID sent in `initialize`.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.config.client_id = Some(client_id.into());
        self
    }

    /// Add a token pushed with the AHP `authenticate` command.
    pub fn with_resource_token(
        mut self,
        resource: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        self.config.resource_tokens.push(ResourceToken {
            resource: resource.into(),
            token: token.into(),
        });
        self
    }

    /// Restrict sessions to these agent providers.
    pub fn with_providers(mut self, providers: Vec<String>) -> Self {
        self.config.providers = providers;
        self
    }

    /// Include or exclude archived sessions.
    pub fn with_include_archived(mut self, include: bool) -> Self {
        self.config.include_archived = include;
        self
    }

    /// Read session channels for chats and input requests.
    pub fn with_subscribe_sessions(mut self, subscribe: bool) -> Self {
        self.config.subscribe_sessions = subscribe;
        self
    }

    /// Include the automation catalogue.
    pub fn with_include_automations(mut self, include: bool) -> Self {
        self.config.include_automations = include;
        self
    }

    /// Validate and build the provider.
    pub fn build(self) -> Result<AhpBootstrapProvider> {
        self.config.validate()?;
        Ok(AhpBootstrapProvider::new(self.config))
    }
}

#[async_trait]
impl BootstrapProvider for AhpBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let session = AhpSession::open(self.config.client_config(&context.source_id))
            .await
            .context("AHP bootstrap could not load a snapshot")?;
        let mut projection = GraphProjection::new();
        projection.refresh_all(session.mirror());
        session.close();

        let mut elements: Vec<&ProjectedElement> = projection
            .elements()
            .filter(|e| e.matches_labels(&request.node_labels, &request.relation_labels))
            .collect();
        // Nodes before relations so relation endpoints already exist.
        elements.sort_by_key(|e| e.is_relation());

        let effective_from = now_ms();
        let mut count = 0usize;
        for element in elements {
            event_tx
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change: element.to_insert(&context.source_id, effective_from),
                    timestamp: Utc::now(),
                    sequence: context.next_sequence(),
                })
                .await?;
            count += 1;
        }
        info!(
            "AHP bootstrap for query '{}' on source '{}' emitted {count} elements",
            request.query_id, context.source_id
        );
        Ok(BootstrapResult {
            event_count: count,
            source_position: None,
        })
    }
}
