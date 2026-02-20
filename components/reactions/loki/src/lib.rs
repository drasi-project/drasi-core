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

//! Grafana Loki reaction plugin for Drasi.

pub mod config;
pub mod loki;

pub use config::{BasicAuth, LokiReactionConfig, QueryConfig, TemplateSpec};
pub use loki::LokiReaction;

use std::collections::HashMap;

pub struct LokiReactionBuilder {
    id: String,
    queries: Vec<String>,
    endpoint: String,
    labels: HashMap<String, String>,
    tenant_id: Option<String>,
    token: Option<String>,
    basic_auth: Option<BasicAuth>,
    timeout_ms: u64,
    routes: HashMap<String, QueryConfig>,
    default_template: Option<QueryConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl LokiReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            endpoint: "http://localhost:3100".to_string(),
            labels: HashMap::new(),
            tenant_id: None,
            token: None,
            basic_auth: None,
            timeout_ms: 5000,
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

    pub fn from_query(self, query_id: impl Into<String>) -> Self {
        self.with_query(query_id)
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn with_tenant_id(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.basic_auth = Some(BasicAuth {
            username: username.into(),
            password: password.into(),
        });
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
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

    pub fn with_config(mut self, config: LokiReactionConfig) -> Self {
        self.endpoint = config.endpoint;
        self.labels = config.labels;
        self.tenant_id = config.tenant_id;
        self.token = config.token;
        self.basic_auth = config.basic_auth;
        self.timeout_ms = config.timeout_ms;
        self.routes = config.routes;
        self.default_template = config.default_template;
        self
    }

    pub fn build(self) -> anyhow::Result<LokiReaction> {
        let config = LokiReactionConfig {
            endpoint: self.endpoint,
            labels: self.labels,
            tenant_id: self.tenant_id,
            token: self.token,
            basic_auth: self.basic_auth,
            timeout_ms: self.timeout_ms,
            routes: self.routes,
            default_template: self.default_template,
        };

        LokiReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_loki_builder_defaults() {
        let reaction = LokiReaction::builder("test-reaction")
            .build()
            .expect("builder should succeed");
        assert_eq!(reaction.id(), "test-reaction");
        assert!(reaction.query_ids().is_empty());
        assert_eq!(reaction.type_name(), "loki");

        let props = reaction.properties();
        assert_eq!(
            props.get("endpoint"),
            Some(&serde_json::Value::String(
                "http://localhost:3100".to_string()
            ))
        );
        assert_eq!(
            props.get("timeout_ms"),
            Some(&serde_json::Value::Number(5000_u64.into()))
        );
    }

    #[test]
    fn test_loki_builder_custom_values() {
        let reaction = LokiReaction::builder("test-reaction")
            .with_endpoint("http://example:3100")
            .with_label("job", "drasi")
            .with_timeout_ms(10000)
            .with_query("query1")
            .build()
            .expect("builder should succeed");

        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        let props = reaction.properties();
        assert_eq!(
            props.get("endpoint"),
            Some(&serde_json::Value::String(
                "http://example:3100".to_string()
            ))
        );
        assert_eq!(
            props.get("timeout_ms"),
            Some(&serde_json::Value::Number(10000_u64.into()))
        );
    }

    #[test]
    fn test_loki_builder_with_query() {
        let reaction = LokiReaction::builder("test-reaction")
            .with_query("query1")
            .with_query("query2")
            .build()
            .expect("builder should succeed");

        assert_eq!(reaction.query_ids(), vec!["query1", "query2"]);
    }
}
