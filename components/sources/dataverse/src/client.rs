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

//! Dataverse HTTP client for the OData Web API.
//!
//! Implements the REST equivalent of `RetrieveEntityChangesRequest` using
//! OData change tracking with `Prefer: odata.track-changes` headers and
//! delta links.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::types::ODataDeltaResponse;
use drasi_lib::identity::{Credentials, IdentityProvider};

/// HTTP client for the Dataverse OData Web API.
///
/// Provides methods for:
/// - Initial change tracking requests (equivalent to `RetrieveEntityChangesRequest` without `DataVersion`)
/// - Delta polling (equivalent to `RetrieveEntityChangesRequest` with `DataVersion`)
/// - Standard data retrieval for bootstrap
pub struct DataverseClient {
    base_url: String,
    api_version: String,
    identity_provider: Arc<dyn IdentityProvider>,
    http_client: reqwest::Client,
    /// Dataverse token scope derived from the environment URL (e.g., `https://myorg.crm.dynamics.com/.default`).
    scope: String,
}

impl DataverseClient {
    /// Create a new Dataverse client.
    ///
    /// # Arguments
    ///
    /// * `environment_url` - Dataverse environment URL (e.g., `https://myorg.crm.dynamics.com`)
    /// * `api_version` - Web API version (e.g., `v9.2`)
    /// * `identity_provider` - Identity provider for authentication
    pub fn new(
        environment_url: &str,
        api_version: &str,
        identity_provider: Arc<dyn IdentityProvider>,
    ) -> Self {
        let base_url = environment_url.trim_end_matches('/').to_string();
        // Derive scope from the environment URL: https://myorg.crm.dynamics.com -> https://myorg.crm.dynamics.com/.default
        let scope = crate::DataverseSource::dataverse_scope(&base_url);
        Self {
            base_url,
            api_version: api_version.to_string(),
            identity_provider,
            http_client: reqwest::Client::new(),
            scope,
        }
    }

    /// Get a bearer token from the identity provider.
    async fn get_token(&self) -> Result<String> {
        let context =
            drasi_lib::identity::CredentialContext::new().with_property("scope", &self.scope);
        let creds = self.identity_provider.get_credentials(&context).await?;
        match creds {
            Credentials::Token { token, .. } => Ok(token),
            _ => {
                anyhow::bail!("Dataverse client requires Token credentials from identity provider")
            }
        }
    }

    /// Perform an initial change tracking request for an entity.
    ///
    /// This is the Web API equivalent of executing `RetrieveEntityChangesRequest`
    /// without a `DataVersion` -- it returns all current records plus an initial
    /// delta token for subsequent change tracking.
    ///
    /// Equivalent to the platform's `GetCurrentDeltaToken()` method which pages
    /// through all initial data.
    ///
    /// # Arguments
    ///
    /// * `entity_set_name` - The entity set name (plural, e.g., `accounts`)
    /// * `select` - Optional `$select` clause for column filtering
    pub async fn initial_change_tracking(
        &self,
        entity_set_name: &str,
        select: Option<&str>,
    ) -> Result<ODataDeltaResponse> {
        let token = self.get_token().await?;

        let mut url = format!(
            "{}/api/data/{}/{}",
            self.base_url, self.api_version, entity_set_name
        );

        if let Some(select_str) = select {
            url = format!("{url}?$select={select_str}");
        }

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("OData-MaxVersion", "4.0")
            .header("OData-Version", "4.0")
            .header("Prefer", "odata.track-changes,odata.maxpagesize=1000")
            .send()
            .await
            .context("Failed to send initial change tracking request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Initial change tracking request failed with status {status}: {body}",);
        }

        let delta_response: ODataDeltaResponse = response
            .json()
            .await
            .context("Failed to parse initial change tracking response")?;

        Ok(delta_response)
    }

    /// Follow a delta link to poll for changes.
    ///
    /// This is the Web API equivalent of executing `RetrieveEntityChangesRequest`
    /// with `DataVersion` set to a previously obtained delta token.
    ///
    /// Maps directly to the platform's `GetChanges(deltaToken)` method.
    ///
    /// # Arguments
    ///
    /// * `delta_link` - The delta link URL from a previous response
    pub async fn follow_delta_link(&self, delta_link: &str) -> Result<ODataDeltaResponse> {
        // Reject cross-origin delta links to prevent SSRF via attacker-controlled
        // OData responses (see `ensure_same_origin` for rationale).
        self.ensure_same_origin(delta_link, "delta_link")?;

        let token = self.get_token().await?;

        let response = self
            .http_client
            .get(delta_link)
            .header("Authorization", format!("Bearer {token}"))
            .header("OData-MaxVersion", "4.0")
            .header("OData-Version", "4.0")
            .header("Prefer", "odata.maxpagesize=1000")
            .send()
            .await
            .context("Failed to send delta link request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Delta link request failed with status {status}: {body}",);
        }

        let delta_response: ODataDeltaResponse = response
            .json()
            .await
            .context("Failed to parse delta response")?;

        Ok(delta_response)
    }

    /// Follow a next link for pagination within a response.
    ///
    /// Equivalent to following the platform's `PagingCookie` for additional pages.
    ///
    /// # Arguments
    ///
    /// * `next_link` - The next link URL from the current page
    pub async fn follow_next_link(&self, next_link: &str) -> Result<ODataDeltaResponse> {
        // Reject cross-origin next links to prevent SSRF via attacker-controlled
        // OData responses (see `ensure_same_origin` for rationale).
        self.ensure_same_origin(next_link, "next_link")?;

        let token = self.get_token().await?;

        let response = self
            .http_client
            .get(next_link)
            .header("Authorization", format!("Bearer {token}"))
            .header("OData-MaxVersion", "4.0")
            .header("OData-Version", "4.0")
            .send()
            .await
            .context("Failed to send next link request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Next link request failed with status {status}: {body}",);
        }

        let delta_response: ODataDeltaResponse = response
            .json()
            .await
            .context("Failed to parse next link response")?;

        Ok(delta_response)
    }

    /// Retrieve all records from an entity (for bootstrap).
    ///
    /// Sends a standard GET request without change tracking headers.
    /// Used by the bootstrap provider to load initial data.
    ///
    /// # Arguments
    ///
    /// * `entity_set_name` - The entity set name (plural, e.g., `accounts`)
    /// * `select` - Optional `$select` clause for column filtering
    pub async fn get_entity_data(
        &self,
        entity_set_name: &str,
        select: Option<&str>,
    ) -> Result<ODataDeltaResponse> {
        let token = self.get_token().await?;

        let mut url = format!(
            "{}/api/data/{}/{}",
            self.base_url, self.api_version, entity_set_name
        );

        if let Some(select_str) = select {
            url = format!("{url}?$select={select_str}");
        }

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("OData-MaxVersion", "4.0")
            .header("OData-Version", "4.0")
            .header("Prefer", "odata.maxpagesize=5000")
            .send()
            .await
            .context("Failed to send entity data request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Entity data request failed with status {status}: {body}",);
        }

        let data_response: ODataDeltaResponse = response
            .json()
            .await
            .context("Failed to parse entity data response")?;

        Ok(data_response)
    }

    /// Get the base URL for this client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Get the API version.
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Reject delta/next link URLs that point outside the configured Dataverse
    /// environment. The OData server controls the contents of `@odata.deltaLink`
    /// and `@odata.nextLink`; without this check, a compromised, misconfigured,
    /// or proxied response could redirect the client (with its bearer token)
    /// to an arbitrary host — e.g., the cloud metadata service at
    /// `169.254.169.254` (SSRF).
    fn ensure_same_origin(&self, link: &str, kind: &str) -> Result<()> {
        let parsed =
            url::Url::parse(link).with_context(|| format!("{kind} is not a valid URL: {link}"))?;
        let base = url::Url::parse(&self.base_url)
            .with_context(|| format!("client base_url is not a valid URL: {}", self.base_url))?;

        // Compare scheme + host + port. We accept any path/query as long as the
        // origin matches the configured environment.
        if parsed.scheme() != base.scheme()
            || parsed.host_str() != base.host_str()
            || parsed.port_or_known_default() != base.port_or_known_default()
        {
            anyhow::bail!(
                "{kind} origin '{}://{}{}' does not match configured Dataverse environment '{}'",
                parsed.scheme(),
                parsed.host_str().unwrap_or(""),
                parsed.port().map(|p| format!(":{p}")).unwrap_or_default(),
                self.base_url,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! HTTP error-path tests for `DataverseClient`.
    //!
    //! These tests stand the client up against `wiremock` and verify the
    //! observable behaviour of failure modes that the production code is
    //! expected to handle:
    //! - Non-2xx HTTP responses (`401`, `429`, `503`) surface as errors with
    //!   diagnostic context rather than panicking.
    //! - Malformed OData responses (missing `@odata.deltaLink`) deserialize
    //!   without crashing — `delta_link` is `None`.

    use super::*;
    use async_trait::async_trait;
    use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Static-token identity provider used to bypass real auth in unit tests.
    struct StaticToken(&'static str);

    #[async_trait]
    impl IdentityProvider for StaticToken {
        async fn get_credentials(&self, _ctx: &CredentialContext) -> anyhow::Result<Credentials> {
            Ok(Credentials::Token {
                username: "test".to_string(),
                token: self.0.to_string(),
            })
        }

        fn clone_box(&self) -> Box<dyn IdentityProvider> {
            Box::new(StaticToken(self.0))
        }
    }

    fn client_for(server: &MockServer) -> DataverseClient {
        DataverseClient::new(&server.uri(), "v9.2", Arc::new(StaticToken("test-token")))
    }

    #[tokio::test]
    async fn initial_change_tracking_propagates_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .initial_change_tracking("accounts", None)
            .await
            .expect_err("401 must surface as an error");
        let msg = format!("{err:?}");
        assert!(msg.contains("401"), "error should mention status: {msg}");
    }

    #[tokio::test]
    async fn delta_link_propagates_429() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "5")
                    .set_body_string("too many requests"),
            )
            .mount(&server)
            .await;

        let client = client_for(&server);
        let delta_url = format!("{}/api/data/v9.2/accounts?$deltatoken=abc", server.uri());
        let err = client
            .follow_delta_link(&delta_url)
            .await
            .expect_err("429 must surface as an error");
        assert!(format!("{err:?}").contains("429"));
    }

    #[tokio::test]
    async fn delta_link_propagates_503() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let delta_url = format!("{}/api/data/v9.2/accounts?$deltatoken=abc", server.uri());
        let err = client
            .follow_delta_link(&delta_url)
            .await
            .expect_err("503 must surface as an error");
        assert!(format!("{err:?}").contains("503"));
    }

    #[tokio::test]
    async fn delta_response_without_delta_link_parses_with_none() {
        // A delta response that only contains `value` (no `@odata.deltaLink` and
        // no `@odata.nextLink`) must still deserialize. Callers can then decide
        // how to handle the missing token.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "@odata.context": "context",
                "value": []
            })))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let url = format!("{}/api/data/v9.2/accounts?$deltatoken=abc", server.uri());
        let resp = client
            .follow_delta_link(&url)
            .await
            .expect("malformed-but-valid OData response should deserialize");
        assert!(resp.delta_link.is_none(), "delta_link should be absent");
        assert!(resp.next_link.is_none(), "next_link should be absent");
        assert!(resp.value.is_empty());
    }

    #[tokio::test]
    async fn malformed_json_body_surfaces_as_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let err = client
            .initial_change_tracking("accounts", None)
            .await
            .expect_err("non-JSON body must surface as a parse error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("parse") || msg.contains("decoding"),
            "error should reference parsing: {msg}"
        );
    }

    #[tokio::test]
    async fn next_link_propagates_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
            .mount(&server)
            .await;

        let client = client_for(&server);
        let next_url = format!("{}/api/data/v9.2/accounts?$skiptoken=xyz", server.uri());
        let err = client
            .follow_next_link(&next_url)
            .await
            .expect_err("500 must surface as an error");
        assert!(format!("{err:?}").contains("500"));
    }

    /// SSRF guard: `follow_delta_link` must reject a URL that points outside
    /// the configured Dataverse environment. Without this, an attacker who
    /// influences the OData response body can redirect the bearer-token
    /// request to e.g. the cloud metadata service.
    #[tokio::test]
    async fn follow_delta_link_rejects_cross_origin() {
        let server = MockServer::start().await;
        // No mocks are mounted: the request must be blocked before it leaves.
        let client = client_for(&server);

        let err = client
            .follow_delta_link("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("cross-origin delta_link must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("origin") && msg.contains("does not match"),
            "error should explain origin mismatch: {msg}"
        );
    }

    #[tokio::test]
    async fn follow_next_link_rejects_cross_origin() {
        let server = MockServer::start().await;
        let client = client_for(&server);

        let err = client
            .follow_next_link("https://attacker.example.com/api/data/v9.2/accounts")
            .await
            .expect_err("cross-origin next_link must be rejected");
        assert!(format!("{err:?}").contains("does not match"));
    }

    #[tokio::test]
    async fn follow_delta_link_rejects_different_scheme() {
        let server = MockServer::start().await;
        let client = client_for(&server);

        // Same host, different scheme — still a different origin.
        let server_host = url::Url::parse(&server.uri())
            .unwrap()
            .host_str()
            .unwrap()
            .to_string();
        let err = client
            .follow_delta_link(&format!("ftp://{server_host}/api/data/v9.2/accounts"))
            .await
            .expect_err("scheme mismatch must be rejected");
        assert!(format!("{err:?}").contains("does not match"));
    }

    #[tokio::test]
    async fn follow_delta_link_rejects_malformed_url() {
        let server = MockServer::start().await;
        let client = client_for(&server);

        let err = client
            .follow_delta_link("not a url")
            .await
            .expect_err("malformed URL must be rejected");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not a valid URL") || msg.contains("delta_link"),
            "error should explain parse failure: {msg}"
        );
    }
}
