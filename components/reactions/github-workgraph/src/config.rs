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

use anyhow::{ensure, Context, Result};
use serde::Serialize;

pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Runtime configuration for the GitHub WorkGraph dispatcher.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubWorkGraphReactionConfig {
    #[serde(skip_serializing)]
    pub token: String,
    pub api_base_url: String,
}

impl GitHubWorkGraphReactionConfig {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
        }
    }

    pub fn with_api_base_url(mut self, api_base_url: impl Into<String>) -> Self {
        self.api_base_url = api_base_url.into();
        self
    }

    pub(crate) fn validate(&self, query_ids: &[String]) -> Result<()> {
        ensure!(
            query_ids.len() == 1,
            "GitHub WorkGraph reaction requires exactly one capacity query"
        );
        ensure!(!self.token.trim().is_empty(), "token cannot be empty");

        let url = reqwest::Url::parse(self.api_base_url.trim())
            .context("apiBaseUrl must be a valid URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https"),
            "apiBaseUrl scheme must be http or https"
        );
        Ok(())
    }
}
