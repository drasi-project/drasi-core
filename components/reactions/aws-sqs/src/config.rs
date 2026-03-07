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

//! Configuration types for SQS reactions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_fifo_queue() -> bool {
    false
}

/// Template specification for an SQS send operation.
///
/// `body` and all `message_attributes` values support Handlebars syntax.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TemplateSpec {
    /// SQS message body template.
    ///
    /// If empty, the reaction sends the raw JSON result payload.
    #[serde(default)]
    pub body: String,

    /// Additional message attributes where values are Handlebars templates.
    #[serde(default)]
    pub message_attributes: HashMap<String, String>,
}

impl TemplateSpec {
    /// Construct a new template specification.
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            message_attributes: HashMap::new(),
        }
    }

    /// Add a templated SQS message attribute.
    pub fn with_message_attribute(
        mut self,
        key: impl Into<String>,
        value_template: impl Into<String>,
    ) -> Self {
        self.message_attributes
            .insert(key.into(), value_template.into());
        self
    }
}

/// Per-query template configuration for each operation type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct QueryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpec>,
}

/// SQS reaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SqsReactionConfig {
    /// SQS queue URL.
    pub queue_url: String,

    /// Optional AWS region override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Optional endpoint override (e.g. ElasticMQ/LocalStack).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_url: Option<String>,

    /// Whether this reaction targets a FIFO queue.
    #[serde(default = "default_fifo_queue")]
    pub fifo_queue: bool,

    /// Optional Handlebars template for FIFO MessageGroupId.
    ///
    /// Defaults to the query id when not set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_group_id_template: Option<String>,

    /// Optional static AWS access key ID (useful for non-IAM environments and testing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,

    /// Optional static AWS secret access key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,

    /// Default template configuration used when query-specific route is not present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,

    /// Query-specific template routes.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
}
