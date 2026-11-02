// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Azure public-cloud token audience used by Durable Task Scheduler.
pub const DTS_SCOPE: &str = "https://durabletask.io/.default";

/// Authentication for the scheduler connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = reaction::durable_task::Authentication)]
#[serde(rename_all = "camelCase")]
pub enum Authentication {
    /// Acquire a fresh bearer token through Drasi's identity provider for each RPC.
    #[default]
    Identity,
    /// Unauthenticated HTTP to a loopback emulator only.
    None,
}

/// Behavior when an operation cannot target an existing workflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = reaction::durable_task::TargetPolicy)]
#[serde(rename_all = "camelCase")]
pub enum TargetPolicy {
    /// Fail delivery without advancing the checkpoint under Strict recovery.
    #[default]
    Error,
    /// Log the condition and deliberately acknowledge without sending.
    Ignore,
}

/// Operator-selected action for one query diff.
///
/// All string fields except policies are Handlebars templates. A missing
/// `instanceId` on `Start` generates a stable ID per emission and diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[schema(as = reaction::durable_task::Action)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    /// Do not send this operation to DTS.
    #[default]
    Ignore,
    /// Schedule a workflow. Existing instances are never replaced.
    #[serde(rename_all = "camelCase")]
    Start {
        orchestration: String,
        #[serde(default)]
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
        /// Applies to a conflicting start, not an identical command retry.
        #[serde(default)]
        on_conflict: TargetPolicy,
    },
    /// Deliver an external event to a configured instance.
    #[serde(rename_all = "camelCase")]
    RaiseEvent {
        instance_id: String,
        event_name: String,
        #[serde(default)]
        on_missing: TargetPolicy,
        #[serde(default)]
        on_terminal: TargetPolicy,
    },
    /// Force termination; this does not compensate completed activities.
    #[serde(rename_all = "camelCase")]
    Terminate {
        instance_id: String,
        #[serde(default)]
        recursive: bool,
        #[serde(default)]
        on_missing: TargetPolicy,
    },
}

/// Per-operation payload template and dispatch action.
pub type DurableTaskTemplate = TemplateSpec<Action>;
/// Independently configured ADD, UPDATE and DELETE actions.
pub type DurableTaskQueryConfig = QueryConfig<Action>;

/// Resolved configuration for the Durable Task Scheduler reaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableTaskReactionConfig {
    pub endpoint: String,
    pub task_hub: String,
    /// Stable producer/epoch namespace. Change it when resetting query sequences.
    pub producer_id: String,
    #[serde(default)]
    pub authentication: Authentication,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<DurableTaskQueryConfig>,
    #[serde(default)]
    pub routes: HashMap<String, DurableTaskQueryConfig>,
}

pub(crate) fn default_scope() -> String {
    DTS_SCOPE.into()
}
pub(crate) fn default_timeout() -> u64 {
    10_000
}
pub(crate) fn default_retries() -> u32 {
    3
}
pub(crate) fn default_retry_delay() -> u64 {
    250
}

impl Default for DurableTaskReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            task_hub: String::new(),
            producer_id: String::new(),
            authentication: Authentication::default(),
            scope: default_scope(),
            timeout_ms: default_timeout(),
            max_retries: default_retries(),
            retry_delay_ms: default_retry_delay(),
            default_template: None,
            routes: HashMap::new(),
        }
    }
}

impl TemplateRouting<Action> for DurableTaskReactionConfig {
    fn routes(&self) -> &HashMap<String, DurableTaskQueryConfig> {
        &self.routes
    }

    fn default_template(&self) -> Option<&DurableTaskQueryConfig> {
        self.default_template.as_ref()
    }

    fn get_template_spec(&self, query_id: &str, op: OperationType) -> Option<&DurableTaskTemplate> {
        self.routes
            .get(query_id)
            .and_then(|route| Self::get_spec_from_config(route, op))
            .or_else(|| {
                query_id.rsplit_once('.').and_then(|(_, short)| {
                    self.routes
                        .get(short)
                        .and_then(|route| Self::get_spec_from_config(route, op))
                })
            })
            .or_else(|| {
                self.default_template
                    .as_ref()
                    .and_then(|route| Self::get_spec_from_config(route, op))
            })
    }
}

impl DurableTaskReactionConfig {
    pub(crate) fn validate(&self, queries: &[String], capacity: Option<usize>) -> Result<()> {
        ensure!(
            !queries.is_empty() && queries.iter().all(|query| !query.trim().is_empty()),
            "at least one nonempty query ID is required"
        );
        ensure!(
            capacity != Some(0),
            "priorityQueueCapacity must be positive"
        );
        ensure!(
            !self.producer_id.trim().is_empty(),
            "producerId is required"
        );
        ensure!(!self.task_hub.trim().is_empty(), "taskHub is required");
        self.task_hub
            .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            .context("taskHub must be valid gRPC ASCII metadata")?;
        ensure!(!self.scope.trim().is_empty(), "scope must not be empty");
        ensure!(self.timeout_ms > 0, "timeoutMs must be positive");
        ensure!(self.max_retries <= 10, "maxRetries must be at most 10");
        ensure!(
            (1..=30_000).contains(&self.retry_delay_ms),
            "retryDelayMs must be between 1 and 30000"
        );
        let url = url::Url::parse(&self.endpoint).context("invalid DTS endpoint")?;
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "endpoint must be an origin URL without credentials, path, query or fragment"
        );
        match self.authentication {
            Authentication::Identity => ensure!(
                url.scheme() == "https",
                "identity authentication requires HTTPS"
            ),
            Authentication::None => ensure!(
                url.scheme() == "http"
                    && (url.host_str() == Some("localhost")
                        || url.host_str().is_some_and(|host| {
                            host.trim_matches(['[', ']'])
                                .parse::<std::net::IpAddr>()
                                .is_ok_and(|ip| ip.is_loopback())
                        })),
                "authentication:none is restricted to a loopback HTTP emulator"
            ),
        }
        for key in self.routes.keys() {
            ensure!(
                queries
                    .iter()
                    .any(|query| query == key || query.rsplit('.').next() == Some(key)),
                "route '{key}' is not a subscribed query"
            );
        }
        for route in self.routes.values().chain(self.default_template.iter()) {
            for spec in [&route.added, &route.updated, &route.deleted]
                .into_iter()
                .flatten()
            {
                handlebars::Template::compile(&spec.template).context("invalid body template")?;
                match &spec.extension {
                    Action::Ignore => {}
                    Action::Start {
                        orchestration,
                        version,
                        instance_id,
                        ..
                    } => {
                        validate_template(orchestration, "orchestration")?;
                        handlebars::Template::compile(version)
                            .context("invalid version template")?;
                        if let Some(id) = instance_id {
                            validate_template(id, "instanceId")?;
                        }
                    }
                    Action::RaiseEvent {
                        instance_id,
                        event_name,
                        ..
                    } => {
                        validate_template(instance_id, "instanceId")?;
                        validate_template(event_name, "eventName")?;
                    }
                    Action::Terminate { instance_id, .. } => {
                        validate_template(instance_id, "instanceId")?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_template(value: &str, field: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{field} must not be empty");
    handlebars::Template::compile(value).with_context(|| format!("invalid {field} template"))?;
    Ok(())
}
