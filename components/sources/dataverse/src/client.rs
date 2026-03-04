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

//! Dataverse HTTP client for the OData Web API.
//!
//! Implements the REST equivalent of `RetrieveEntityChangesRequest` using
//! OData change tracking with `Prefer: odata.track-changes` headers and
//! delta links.

use anyhow::{Context, Result};
use std::sync::Arc;

use drasi_lib::identity::{Credentials, IdentityProvider};
use crate::types::ODataDeltaResponse;

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
        Self {
            base_url,
            api_version: api_version.to_string(),
            identity_provider,
            http_client: reqwest::Client::new(),
        }
    }

    /// Get a bearer token from the identity provider.
    async fn get_token(&self) -> Result<String> {
        let creds = self.identity_provider.get_credentials().await?;
        match creds {
            Credentials::Token { token, .. } => Ok(token),
            _ => anyhow::bail!("Dataverse client requires Token credentials from identity provider"),
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
}
