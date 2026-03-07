#![allow(unexpected_cfgs)]
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

//! AWS SQS reaction plugin for Drasi.

use std::collections::HashMap;

pub mod config;
pub mod descriptor;
pub mod reaction;

pub use config::{QueryConfig, SqsReactionConfig, TemplateSpec};
pub use reaction::SqsReaction;

/// Builder for SQS reactions.
pub struct SqsReactionBuilder {
    id: String,
    queries: Vec<String>,
    queue_url: String,
    region: Option<String>,
    endpoint_url: Option<String>,
    fifo_queue: bool,
    message_group_id_template: Option<String>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    routes: HashMap<String, QueryConfig>,
    default_template: Option<QueryConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl SqsReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            queue_url: String::new(),
            region: None,
            endpoint_url: None,
            fifo_queue: false,
            message_group_id_template: None,
            access_key_id: None,
            secret_access_key: None,
            routes: HashMap::new(),
            default_template: None,
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_queue_url(mut self, queue_url: impl Into<String>) -> Self {
        self.queue_url = queue_url.into();
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url = Some(endpoint_url.into());
        self
    }

    pub fn with_fifo_queue(mut self, fifo_queue: bool) -> Self {
        self.fifo_queue = fifo_queue;
        self
    }

    pub fn with_message_group_id_template(mut self, template: impl Into<String>) -> Self {
        self.message_group_id_template = Some(template.into());
        self
    }

    pub fn with_credentials(
        mut self,
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
    ) -> Self {
        self.access_key_id = Some(access_key_id.into());
        self.secret_access_key = Some(secret_access_key.into());
        self
    }

    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
        self
    }

    pub fn with_default_template(mut self, config: QueryConfig) -> Self {
        self.default_template = Some(config);
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_config(mut self, config: SqsReactionConfig) -> Self {
        self.queue_url = config.queue_url;
        self.region = config.region;
        self.endpoint_url = config.endpoint_url;
        self.fifo_queue = config.fifo_queue;
        self.message_group_id_template = config.message_group_id_template;
        self.access_key_id = config.access_key_id;
        self.secret_access_key = config.secret_access_key;
        self.routes = config.routes;
        self.default_template = config.default_template;
        self
    }

    fn validate_templates(&self) -> anyhow::Result<()> {
        let mut handlebars = handlebars::Handlebars::new();
        reaction::SqsReaction::register_json_helper(&mut handlebars);

        let validate_template = |template: &str| -> anyhow::Result<()> {
            if template.is_empty() {
                return Ok(());
            }
            handlebars
                .render_template(template, &serde_json::json!({}))
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("invalid template '{template}': {e}"))
        };

        let validate_spec = |spec: &TemplateSpec| -> anyhow::Result<()> {
            validate_template(&spec.body)?;
            for template in spec.message_attributes.values() {
                validate_template(template)?;
            }
            Ok(())
        };

        for (query_id, config) in &self.routes {
            if let Some(spec) = &config.added {
                validate_spec(spec)
                    .map_err(|e| anyhow::anyhow!("invalid added template for '{query_id}': {e}"))?;
            }
            if let Some(spec) = &config.updated {
                validate_spec(spec).map_err(|e| {
                    anyhow::anyhow!("invalid updated template for '{query_id}': {e}")
                })?;
            }
            if let Some(spec) = &config.deleted {
                validate_spec(spec).map_err(|e| {
                    anyhow::anyhow!("invalid deleted template for '{query_id}': {e}")
                })?;
            }
        }

        if let Some(config) = &self.default_template {
            if let Some(spec) = &config.added {
                validate_spec(spec)?;
            }
            if let Some(spec) = &config.updated {
                validate_spec(spec)?;
            }
            if let Some(spec) = &config.deleted {
                validate_spec(spec)?;
            }
        }

        if let Some(template) = &self.message_group_id_template {
            validate_template(template)?;
        }

        Ok(())
    }

    pub fn build(self) -> anyhow::Result<SqsReaction> {
        if self.queue_url.trim().is_empty() {
            return Err(anyhow::anyhow!("queue_url must be provided"));
        }
        self.validate_templates()?;

        let config = SqsReactionConfig {
            queue_url: self.queue_url,
            region: self.region,
            endpoint_url: self.endpoint_url,
            fifo_queue: self.fifo_queue,
            message_group_id_template: self.message_group_id_template,
            access_key_id: self.access_key_id,
            secret_access_key: self.secret_access_key,
            routes: self.routes,
            default_template: self.default_template,
        };

        Ok(SqsReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_sqs_builder_defaults() {
        let reaction = SqsReaction::builder("test")
            .with_queue_url("https://example.com/queue")
            .build()
            .expect("builder should succeed");

        assert_eq!(reaction.id(), "test");
        assert!(reaction.auto_start());
        let props = reaction.properties();
        assert_eq!(
            props.get("queue_url"),
            Some(&serde_json::Value::String(
                "https://example.com/queue".to_string()
            ))
        );
        assert_eq!(
            props.get("fifo_queue"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn test_sqs_builder_custom_values() {
        let reaction = SqsReaction::builder("test")
            .with_queue_url("https://example.com/queue.fifo")
            .with_region("us-east-1")
            .with_endpoint_url("http://localhost:9324")
            .with_fifo_queue(true)
            .with_message_group_id_template("{{query_id}}")
            .with_query("query-a")
            .with_auto_start(false)
            .build()
            .expect("builder should succeed");

        assert_eq!(reaction.query_ids(), vec!["query-a"]);
        assert!(!reaction.auto_start());
        let props = reaction.properties();
        assert_eq!(
            props.get("region"),
            Some(&serde_json::Value::String("us-east-1".to_string()))
        );
        assert_eq!(
            props.get("endpoint_url"),
            Some(&serde_json::Value::String(
                "http://localhost:9324".to_string()
            ))
        );
        assert_eq!(
            props.get("fifo_queue"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_builder_requires_queue_url() {
        let err = match SqsReaction::builder("test").build() {
            Ok(_) => panic!("queue_url is required"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("queue_url must be provided"));
    }

    #[test]
    fn test_template_spec_builder() {
        let spec = TemplateSpec::new("{\"value\":{{after.value}}}")
            .with_message_attribute("x-test", "{{operation}}");
        assert_eq!(spec.body, "{\"value\":{{after.value}}}");
        assert_eq!(
            spec.message_attributes.get("x-test"),
            Some(&"{{operation}}".to_string())
        );
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "aws-sqs-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::SqsReactionDescriptor],
    bootstrap_descriptors = [],
);
