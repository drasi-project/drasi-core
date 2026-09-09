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

use anyhow::Context;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::activation::TerminalUpdatePolicy;

fn default_timeout_ms() -> u64 {
    5000
}

fn default_return_immediately() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct A2AReactionConfig {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_key_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_template: Option<String>,
    #[serde(default)]
    pub terminal_update_policy: TerminalUpdatePolicy,
    #[serde(default = "default_return_immediately")]
    pub return_immediately: bool,
}

impl Default for A2AReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost".to_string(),
            token: None,
            timeout_ms: default_timeout_ms(),
            result_key_fields: Vec::new(),
            instruction_template: None,
            terminal_update_policy: TerminalUpdatePolicy::default(),
            return_immediately: default_return_immediately(),
        }
    }
}

impl A2AReactionConfig {
    pub fn validate(
        &self,
        _query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        if self.endpoint.trim().is_empty() {
            anyhow::bail!("`endpoint` must not be empty");
        }

        let endpoint_url = Url::parse(&self.endpoint).context("invalid endpoint URL")?;
        match endpoint_url.scheme() {
            "http" | "https" => {}
            scheme => {
                anyhow::bail!("unsupported endpoint URL scheme '{scheme}'; expected http or https")
            }
        }
        if endpoint_url.host_str().is_none() {
            anyhow::bail!("endpoint URL must include a host");
        }

        if self.timeout_ms == 0 {
            anyhow::bail!("`timeoutMs` must be greater than 0");
        }

        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("`priorityQueueCapacity` must be greater than 0");
        }

        if self.result_key_fields.is_empty() {
            anyhow::bail!("`resultKeyFields` must contain at least one field");
        }

        for field in &self.result_key_fields {
            if field.trim().is_empty() {
                anyhow::bail!("`resultKeyFields` must not contain empty values");
            }
        }

        if let Some(template) = &self.instruction_template {
            handlebars::Template::compile(template)
                .map_err(|e| anyhow::anyhow!("invalid instructionTemplate: {e}"))?;
        }

        Ok(())
    }
}
