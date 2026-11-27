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

//! Connection settings and the synchronised AHP session used by both the
//! source (long-lived) and the bootstrap provider (one-shot snapshot).

use std::collections::HashSet;
use std::time::Duration;

use ahp_types::commands::{
    AuthenticateParams, AuthenticateResult, Implementation, InitializeParams, InitializeResult,
    ListSessionsParams, ListSessionsResult, SubscribeParams, SubscribeResult, UnsubscribeParams,
};
use ahp_types::{PROTOCOL_VERSION, ROOT_RESOURCE_URI, SUPPORTED_PROTOCOL_VERSIONS};
use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::client::{AhpConnection, RequestError, ServerEvent};
use crate::mirror::{AhpMirror, Scope};
use crate::model::{SessionFilter, AUTOMATIONS_URI};

/// Default `listSessions` page size.
pub const DEFAULT_PAGE_SIZE: u32 = 100;
/// Default connect timeout in milliseconds.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Default request timeout in milliseconds.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

/// A bearer token pushed to the host with the AHP `authenticate` command for
/// an agent's protected resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceToken {
    /// Protected resource identifier from `AgentInfo.protectedResources[].resource`.
    pub resource: String,
    /// Bearer token for the resource.
    pub token: String,
}

/// Resolved connection and selection settings shared by the AHP source and
/// bootstrap provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AhpClientConfig {
    /// WebSocket URL of the AHP host, e.g. `ws://localhost:4321`.
    pub url: String,
    /// Optional bearer token sent as an `Authorization` header on connect.
    pub bearer_token: Option<String>,
    /// Client identifier sent in `initialize`.
    pub client_id: String,
    /// Tokens pushed with `authenticate` after `initialize`.
    pub resource_tokens: Vec<ResourceToken>,
    /// Session selection.
    pub filter: SessionFilter,
    /// Subscribe to each selected session channel to mirror chats and input
    /// requests in full. When `false`, only catalogue summaries are used.
    pub subscribe_sessions: bool,
    /// Subscribe to the `ahp-automations://` catalogue when the host supports it.
    pub include_automations: bool,
    /// `listSessions` page size.
    pub page_size: u32,
    /// Connect timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Request timeout in milliseconds.
    pub request_timeout_ms: u64,
}

impl AhpClientConfig {
    /// Create a configuration with defaults for everything but the URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            bearer_token: None,
            client_id: "drasi".to_string(),
            resource_tokens: Vec::new(),
            filter: SessionFilter::default(),
            subscribe_sessions: true,
            include_automations: true,
            page_size: DEFAULT_PAGE_SIZE,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err(anyhow!("AHP url is required"));
        }
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return Err(anyhow!("AHP url must use ws:// or wss:// (got '{url}')"));
        }
        if self.client_id.trim().is_empty() {
            return Err(anyhow!("AHP clientId must not be empty"));
        }
        if self.page_size == 0 {
            return Err(anyhow!("AHP pageSize must be greater than zero"));
        }
        if self.connect_timeout_ms == 0 || self.request_timeout_ms == 0 {
            return Err(anyhow!("AHP timeouts must be greater than zero"));
        }
        Ok(())
    }
}

/// Error that retrying cannot fix (protocol mismatch, rejected credentials).
#[derive(Debug)]
pub struct FatalAhpError(pub String);

impl std::fmt::Display for FatalAhpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FatalAhpError {}

/// Whether an error returned by [`AhpSession::open`] is fatal.
pub fn is_fatal(error: &anyhow::Error) -> bool {
    error.downcast_ref::<FatalAhpError>().is_some()
}

fn protocol_versions() -> Vec<String> {
    let mut versions = vec![PROTOCOL_VERSION.to_string()];
    for v in SUPPORTED_PROTOCOL_VERSIONS {
        if !versions.iter().any(|existing| existing == v) {
            versions.push((*v).to_string());
        }
    }
    versions
}

/// A connected AHP client whose [`AhpMirror`] is kept in sync with the host.
pub struct AhpSession {
    connection: AhpConnection,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    mirror: AhpMirror,
    config: AhpClientConfig,
    subscribed: HashSet<String>,
}

impl AhpSession {
    /// Connect, initialize, and load a complete snapshot: root state, the
    /// paginated session catalogue, selected session channels, and (when
    /// supported) the automation catalogue. Live events received while
    /// loading are queued and applied by [`AhpSession::next_change`].
    pub async fn open(config: AhpClientConfig) -> Result<Self> {
        config.validate()?;
        let (connection, events) = AhpConnection::connect(
            config.url.trim(),
            config.bearer_token.as_deref(),
            Duration::from_millis(config.connect_timeout_ms),
            Duration::from_millis(config.request_timeout_ms),
        )
        .await?;
        let mut session = Self {
            connection,
            events,
            mirror: AhpMirror::new(config.filter.clone()),
            config,
            subscribed: HashSet::new(),
        };
        session.load().await?;
        Ok(session)
    }

    async fn load(&mut self) -> Result<()> {
        let init: InitializeResult = self
            .connection
            .request(
                "initialize",
                InitializeParams {
                    channel: ROOT_RESOURCE_URI.to_string(),
                    meta: None,
                    protocol_versions: protocol_versions(),
                    client_id: self.config.client_id.clone(),
                    initial_subscriptions: Some(vec![ROOT_RESOURCE_URI.to_string()]),
                    locale: None,
                    capabilities: None,
                    client_info: Some(Implementation {
                        name: "drasi".to_string(),
                        version: Some(env!("CARGO_PKG_VERSION").to_string()),
                        title: Some("Drasi AHP source".to_string()),
                    }),
                },
            )
            .await
            .map_err(|e| match e {
                RequestError::Rpc(rpc) => anyhow::Error::new(FatalAhpError(format!(
                    "AHP host rejected initialize ({}): {}",
                    rpc.code, rpc.message
                ))),
                other => anyhow!("AHP initialize failed: {other}"),
            })?;
        info!(
            "Connected to AHP host '{}' (protocol {})",
            init.server_info
                .as_ref()
                .map_or("unknown", |i| i.name.as_str()),
            init.protocol_version
        );
        for snapshot in &init.snapshots {
            self.mirror.apply_snapshot(snapshot);
        }

        for token in &self.config.resource_tokens {
            let _: AuthenticateResult = self
                .connection
                .request(
                    "authenticate",
                    AuthenticateParams {
                        channel: ROOT_RESOURCE_URI.to_string(),
                        meta: None,
                        resource: token.resource.clone(),
                        token: token.token.clone(),
                        expires_in: None,
                        scopes: None,
                        target: None,
                        account_id: None,
                    },
                )
                .await
                .map_err(|e| match e {
                    RequestError::Rpc(rpc) => anyhow::Error::new(FatalAhpError(format!(
                        "AHP authenticate for '{}' rejected: {}",
                        token.resource, rpc.message
                    ))),
                    other => anyhow!("AHP authenticate failed: {other}"),
                })?;
        }

        if self.config.include_automations && init.automations.is_some() {
            match self.subscribe(AUTOMATIONS_URI).await {
                Ok(()) => {}
                Err(e) => warn!("AHP automations catalogue unavailable: {e}"),
            }
        }

        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let page: ListSessionsResult = self
                .connection
                .request(
                    "listSessions",
                    ListSessionsParams {
                        channel: ROOT_RESOURCE_URI.to_string(),
                        meta: None,
                        limit: Some(i64::from(self.config.page_size)),
                        cursor: cursor.clone(),
                    },
                )
                .await
                .map_err(|e| anyhow!("AHP listSessions failed: {e}"))?;
            for summary in page.items {
                self.mirror.upsert_summary(summary);
            }
            match page.next_cursor {
                Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
                _ => break,
            }
        }

        let uris: Vec<String> = self.mirror.sessions().keys().cloned().collect();
        for uri in uris {
            self.reconcile_subscription(&uri).await;
        }
        debug!(
            "AHP snapshot loaded: {} sessions, {} subscribed",
            self.mirror.sessions().len(),
            self.subscribed.len()
        );
        Ok(())
    }

    async fn subscribe(&mut self, uri: &str) -> std::result::Result<(), RequestError> {
        let result: SubscribeResult = self
            .connection
            .request(
                "subscribe",
                SubscribeParams {
                    channel: uri.to_string(),
                    meta: None,
                    delivery: None,
                    view: None,
                    flow_control: None,
                },
            )
            .await?;
        self.subscribed.insert(uri.to_string());
        match result.snapshot {
            Some(snapshot) => {
                self.mirror.apply_snapshot(&snapshot);
            }
            None => warn!("AHP subscribe to '{uri}' returned no snapshot"),
        }
        Ok(())
    }

    fn unsubscribe(&mut self, uri: &str) {
        if self.subscribed.remove(uri) {
            if let Err(e) = self.connection.notify(
                "unsubscribe",
                UnsubscribeParams {
                    channel: uri.to_string(),
                },
            ) {
                debug!("AHP unsubscribe from '{uri}' failed: {e}");
            }
        }
    }

    /// Subscribe to or unsubscribe from a session channel so the set of
    /// subscriptions tracks the filter.
    async fn reconcile_subscription(&mut self, uri: &str) {
        let want = self.config.subscribe_sessions && self.mirror.is_session_included(uri);
        let have = self.subscribed.contains(uri);
        if want && !have {
            if let Err(e) = self.subscribe(uri).await {
                warn!("AHP subscribe to session '{uri}' failed: {e}");
            }
        } else if !want && have {
            self.unsubscribe(uri);
            self.mirror.clear_session_state(uri);
        }
    }

    /// The mirrored state.
    pub fn mirror(&self) -> &AhpMirror {
        &self.mirror
    }

    /// Channels currently subscribed (excluding the root channel).
    pub fn subscriptions(&self) -> &HashSet<String> {
        &self.subscribed
    }

    /// Wait for the next server event, apply it to the mirror, and return
    /// the scopes whose projection may have changed. Returns an error when
    /// the connection closes.
    pub async fn next_change(&mut self) -> Result<Vec<Scope>> {
        let event = self
            .events
            .recv()
            .await
            .ok_or_else(|| anyhow!("AHP connection closed"))?;
        self.apply_event(event).await
    }

    async fn apply_event(&mut self, event: ServerEvent) -> Result<Vec<Scope>> {
        let mut scopes = Vec::new();
        match event {
            ServerEvent::Action(envelope) => {
                if let Some(scope) = self.mirror.apply_action(&envelope) {
                    if let Scope::Session(uri) = &scope {
                        let uri = uri.clone();
                        self.reconcile_subscription(&uri).await;
                    }
                    scopes.push(scope);
                }
            }
            ServerEvent::SessionAdded(params) => {
                let scope = self.mirror.upsert_summary(params.summary);
                if let Scope::Session(uri) = &scope {
                    let uri = uri.clone();
                    self.reconcile_subscription(&uri).await;
                }
                scopes.push(scope);
            }
            ServerEvent::SessionRemoved(params) => {
                self.unsubscribe(&params.session);
                scopes.push(self.mirror.remove_session(&params.session));
            }
            ServerEvent::SessionSummaryChanged(params) => {
                if let Some(scope) = self.mirror.merge_summary(&params.session, &params.changes) {
                    self.reconcile_subscription(&params.session).await;
                    scopes.push(scope);
                }
            }
            ServerEvent::AuthRequired(params) => {
                warn!(
                    "AHP host requires authentication on '{}'; configure resourceTokens",
                    params.channel
                );
            }
            ServerEvent::Closed(reason) => return Err(anyhow!("AHP connection lost: {reason}")),
        }
        Ok(scopes)
    }

    /// Liveness check using the protocol-level `ping`.
    pub async fn ping(&self) -> Result<()> {
        #[derive(Serialize)]
        struct PingParams {
            channel: &'static str,
        }
        self.connection
            .request::<_, serde_json::Value>(
                "ping",
                PingParams {
                    channel: ROOT_RESOURCE_URI,
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow!("AHP ping failed: {e}"))
    }

    /// Close the connection gracefully.
    pub fn close(&self) {
        self.connection.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_bad_urls() {
        assert!(AhpClientConfig::new("ws://localhost:1").validate().is_ok());
        assert!(AhpClientConfig::new("wss://host/ahp").validate().is_ok());
        assert!(AhpClientConfig::new("").validate().is_err());
        assert!(AhpClientConfig::new("http://host").validate().is_err());
        let mut config = AhpClientConfig::new("ws://h");
        config.page_size = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn offers_current_and_supported_versions() {
        let versions = protocol_versions();
        assert_eq!(versions[0], PROTOCOL_VERSION);
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            assert!(versions.iter().any(|x| x == v));
        }
    }
}
