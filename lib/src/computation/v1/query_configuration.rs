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

use crate::config::{
    QueryConfig, QueryJoinConfig, SourceSubscriptionConfig, SourceSubscriptionSettings,
};
use drasi_core::{
    middleware::MiddlewareTypeRegistry, models::SourceMiddlewareConfig, query::QueryBuilder,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};

/// Native query execution settings, independent of the legacy QueryManager.
/// The existing DTOs describe joins and middleware without owning runtime state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryExecutionSettings {
    #[serde(default)]
    pub joins: Vec<QueryJoinConfig>,
    #[serde(default)]
    pub middleware: Vec<SourceMiddlewareConfig>,
    #[serde(default)]
    pub sources: Vec<SourceSubscriptionConfig>,
}

pub struct QueryMiddlewareResource(pub Arc<MiddlewareTypeRegistry>);

impl QueryExecutionSettings {
    pub fn from_legacy_config(config: &QueryConfig) -> Self {
        Self {
            joins: config.joins.clone().unwrap_or_default(),
            middleware: config.middleware.clone(),
            sources: config.sources.clone(),
        }
    }

    /// Derive exactly the same source label interests as the legacy configuration
    /// path. The compatibility source adapter supplies its own scoped subscriber ID.
    pub fn legacy_subscriptions(
        config: &QueryConfig,
    ) -> anyhow::Result<Vec<SourceSubscriptionSettings>> {
        let labels =
            crate::queries::LabelExtractor::extract_labels(&config.query, &config.query_language)?;
        crate::queries::SubscriptionSettingsBuilder::build_subscription_settings(config, &labels)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.joins.is_empty() && self.middleware.is_empty() && self.sources.is_empty()
    }

    pub(super) fn validate(&self, registry: Option<&MiddlewareTypeRegistry>) -> anyhow::Result<()> {
        let mut joins = BTreeSet::new();
        for join in &self.joins {
            super::data::validate_identifier("join", &join.id)?;
            if !joins.insert(&join.id) || join.keys.len() < 2 {
                anyhow::bail!("synthetic joins require unique IDs and at least two keys");
            }
            for key in &join.keys {
                super::data::validate_identifier("join label", &key.label)?;
                super::data::validate_identifier("join property", &key.property)?;
            }
        }
        let mut middleware = BTreeSet::new();
        for config in &self.middleware {
            super::data::validate_identifier("middleware", &config.name)?;
            super::data::validate_identifier("middleware kind", &config.kind)?;
            if !middleware.insert(config.name.as_ref()) {
                anyhow::bail!("duplicate middleware name");
            }
            if registry.is_some_and(|registry| registry.get(&config.kind).is_none()) {
                anyhow::bail!("middleware kind {} is not registered", config.kind);
            }
        }
        let mut sources = BTreeSet::new();
        for source in &self.sources {
            super::data::validate_identifier("source", &source.source_id)?;
            if !sources.insert(&source.source_id) {
                anyhow::bail!("duplicate source subscription");
            }
            for name in &source.pipeline {
                if !middleware.contains(name.as_str()) {
                    anyhow::bail!("source pipeline references undeclared middleware {name}");
                }
            }
        }
        Ok(())
    }

    pub(super) fn configure(
        &self,
        mut builder: QueryBuilder,
        registry: Option<Arc<MiddlewareTypeRegistry>>,
    ) -> anyhow::Result<QueryBuilder> {
        self.validate(registry.as_deref())?;
        if !self.middleware.is_empty() {
            builder = builder.with_middleware_registry(registry.ok_or_else(|| {
                anyhow::anyhow!("query middleware requires a registered middleware resource")
            })?);
        }
        for config in &self.middleware {
            builder = builder.with_source_middleware(Arc::new(config.clone()));
        }
        for source in &self.sources {
            builder = builder.with_source_pipeline(&source.source_id, &source.pipeline);
        }
        Ok(builder.with_joins(self.joins.iter().cloned().map(Into::into).collect()))
    }
}
