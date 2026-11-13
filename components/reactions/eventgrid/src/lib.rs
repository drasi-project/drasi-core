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

#![allow(unexpected_cfgs)]

//! Azure Event Grid reaction plugin for Drasi.
//!
//! Publishes continuous-query result changes to an **Azure Event Grid custom
//! topic** as events. Supports two wire schemas (**CloudEvents 1.0** and native
//! **EventGrid**) and three output formats (**packed**, **unpacked**,
//! **template**). Authentication is via the topic **access key**
//! (`aeg-sas-key`) or **Microsoft Entra Workload Identity** (AAD bearer token).
//!
//! See [`config::EventGridReactionConfig`] for the full configuration shape and
//! the crate `README.md` for examples.
//!
//! ## Quick start (access key, CloudEvents, unpacked)
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_lib::{ComponentId, ComponentName};
//! use drasi_reaction_eventgrid::{EventGridReaction, EventGridSchema, OutputFormat};
//!
//! let reaction_id = ComponentId::try_new("00000000-0000-0000-0000-000000000051")?;
//! let reaction = EventGridReaction::builder(reaction_id)
//!     .with_query_by_name(ComponentName::try_new("orders")?)
//!     .with_endpoint("https://my-topic.eastus-1.eventgrid.azure.net/api/events") // DevSkim: ignore DS137138
//!     .with_access_key("secret")
//!     .with_schema(EventGridSchema::CloudEvents)
//!     .with_format(OutputFormat::Unpacked)
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod descriptor;
pub mod event;
pub mod publish;
pub mod reaction;

#[cfg(test)]
mod tests;

pub use config::{
    EventGridOutputTemplates, EventGridQueryConfig, EventGridReactionConfig, EventGridSchema,
    EventGridTemplateExt, EventGridTemplateSpec, OutputFormat,
};
pub use reaction::EventGridReaction;

use drasi_lib::recovery::ReactionRecoveryPolicy;

/// Builder for the Event Grid reaction.
pub struct EventGridReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    config: EventGridReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl EventGridReactionBuilder {
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            config: EventGridReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
        }
    }

    /// Set explicit name or ID references for every query input.
    pub fn with_query_inputs(mut self, inputs: Vec<drasi_lib::config::ComponentReference>) -> Self {
        self.queries = inputs;
        self
    }

    /// Set all query inputs by their current names.
    pub fn with_queries_by_name(self, names: Vec<drasi_lib::ComponentName>) -> Self {
        self.with_query_inputs(
            names
                .into_iter()
                .map(drasi_lib::config::ComponentReference::by_name)
                .collect(),
        )
    }

    /// Set all query inputs by their immutable IDs.
    pub fn with_queries_by_id(self, ids: Vec<drasi_lib::ComponentId>) -> Self {
        self.with_query_inputs(
            ids.into_iter()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        )
    }

    /// Add a query input by its current name.
    pub fn with_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn with_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Add a query input by its current name.
    pub fn from_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn from_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Set the full Event Grid custom-topic events URL.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    /// Set the topic access key (sent as the `aeg-sas-key` header).
    pub fn with_access_key(mut self, access_key: impl Into<String>) -> Self {
        self.config.access_key = Some(access_key.into());
        self
    }

    /// Set the wire schema (CloudEvents or EventGrid).
    pub fn with_schema(mut self, schema: EventGridSchema) -> Self {
        self.config.schema = schema;
        self
    }

    /// Set the output format (packed/unpacked/template).
    pub fn with_format(mut self, format: OutputFormat) -> Self {
        self.config.format = format;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    /// Permit a plaintext `http` endpoint. Disabled by default; opt in only for
    /// local test servers, since an `http` endpoint transmits the access key in
    /// cleartext.
    pub fn with_allow_http(mut self, allow_http: bool) -> Self {
        self.config.allow_http = allow_http;
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

    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }

    /// Replace the default fallback template applied to every query that has no
    /// explicit route.
    pub fn with_default_template(mut self, template: EventGridQueryConfig) -> Self {
        let templates = self
            .config
            .output_templates
            .get_or_insert_with(Default::default);
        templates.default_template = Some(template);
        self
    }

    /// Add or replace the per-query template for `query_id`.
    pub fn with_query_template(
        mut self,
        query_id: impl Into<String>,
        template: EventGridQueryConfig,
    ) -> Self {
        let templates = self
            .config
            .output_templates
            .get_or_insert_with(Default::default);
        templates.routes.insert(query_id.into(), template);
        self
    }

    /// Replace the whole [`EventGridOutputTemplates`] block at once.
    pub fn with_output_templates(mut self, templates: EventGridOutputTemplates) -> Self {
        self.config.output_templates = Some(templates);
        self
    }

    /// Replace the full configuration.
    pub fn with_config(mut self, config: EventGridReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Validate the configuration and construct the [`EventGridReaction`].
    ///
    /// Returns an error if the configuration is invalid — for example a missing,
    /// malformed, or non-`https` endpoint (see `allowHttp`), a zero `timeoutMs`
    /// or `priorityQueueCapacity`, the `template` format selected without any
    /// templates, an invalid Handlebars template, or a template route that
    /// matches no subscribed query.
    pub fn build(self) -> anyhow::Result<EventGridReaction> {
        self.config.validate(
            &drasi_lib::reactions::common::input_aliases(&self.queries),
            self.priority_queue_capacity,
        )?;
        Ok(EventGridReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
            self.recovery_policy,
        ))
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "eventgrid-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::EventGridReactionDescriptor],
    bootstrap_descriptors = [],
);
