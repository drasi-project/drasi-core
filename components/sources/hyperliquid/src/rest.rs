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

//! REST client for Hyperliquid /info endpoints.

use crate::types::{AssetCtx, InfoRequest, L2Book, MetaResponse, SpotMetaResponse};
use anyhow::{anyhow, Result};
use reqwest::Client;

#[derive(Clone)]
pub struct HyperliquidRestClient {
    base_url: String,
    client: Client,
}

impl HyperliquidRestClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
        }
    }

    async fn post_info<T: serde::de::DeserializeOwned>(
        &self,
        request: serde_json::Value,
    ) -> Result<T> {
        let response = self
            .client
            .post(&self.base_url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json::<T>().await?)
    }

    pub async fn fetch_meta(&self) -> Result<MetaResponse> {
        self.post_info(serde_json::json!({ "type": "meta" })).await
    }

    pub async fn fetch_spot_meta(&self) -> Result<SpotMetaResponse> {
        self.post_info(serde_json::json!({ "type": "spotMeta" }))
            .await
    }

    pub async fn fetch_all_mids(&self) -> Result<std::collections::HashMap<String, String>> {
        self.post_info(serde_json::json!({ "type": "allMids" }))
            .await
    }

    pub async fn fetch_meta_and_asset_ctxs(&self) -> Result<(MetaResponse, Vec<AssetCtx>)> {
        let value: serde_json::Value = self
            .post_info(serde_json::json!({ "type": "metaAndAssetCtxs" }))
            .await?;

        let array = value
            .as_array()
            .ok_or_else(|| anyhow!("metaAndAssetCtxs response is not an array"))?;
        let meta_value = array
            .first()
            .ok_or_else(|| anyhow!("metaAndAssetCtxs missing meta response"))?
            .clone();
        let ctxs_value = array
            .get(1)
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or(serde_json::Value::Array(vec![]));

        let meta: MetaResponse = serde_json::from_value(meta_value)
            .map_err(|e| anyhow!("Failed to parse meta response: {e}"))?;
        let ctxs: Vec<AssetCtx> = serde_json::from_value(ctxs_value).map_err(|e| {
            anyhow!("Failed to parse asset contexts from metaAndAssetCtxs response: {e}")
        })?;

        Ok((meta, ctxs))
    }

    pub async fn fetch_l2_book(&self, coin: &str) -> Result<L2Book> {
        self.post_info(serde_json::json!({ "type": "l2Book", "coin": coin }))
            .await
    }

    pub async fn resolve_all_coins(&self) -> Result<Vec<String>> {
        let meta = self.fetch_meta().await?;
        Ok(meta.universe.into_iter().map(|asset| asset.name).collect())
    }

    pub async fn post_custom(&self, request: InfoRequest) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(&self.base_url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;

        Ok(response.json::<serde_json::Value>().await?)
    }
}
