// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

//! Workflow actions and template routing. No action is inferred from a query operation.

use anyhow::{ensure, Context, Result};
use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Ordinary Start behavior when the workflow ID is already running.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::ConflictPolicy)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Fail,
    UseExisting,
}

/// Ordinary Start behavior when the workflow ID has a closed execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::ReusePolicy)]
#[serde(rename_all = "snake_case")]
pub enum ReusePolicy {
    #[default]
    RejectDuplicate,
    AllowDuplicate,
    AllowDuplicateFailedOnly,
}

/// Stage acknowledged before an Update delivery is checkpointed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::UpdateWait)]
#[serde(rename_all = "snake_case")]
pub enum UpdateWait {
    Accepted,
    #[default]
    Completed,
}

/// Action-specific fields, flattened alongside the shared payload `template`.
///
/// String fields are Handlebars templates. Policy and action enums are not templated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::Action)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Signal {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        signal_name: String,
    },
    Start {
        workflow_id: String,
        workflow_type: String,
        task_queue: String,
        #[serde(default)]
        conflict_policy: ConflictPolicy,
        #[serde(default)]
        reuse_policy: ReusePolicy,
    },
    Update {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        update_name: String,
        #[serde(default)]
        wait_for: UpdateWait,
    },
    Cancel {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        #[serde(default)]
        reason: String,
    },
    Terminate {
        workflow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
        reason: String,
    },
}

impl Default for Action {
    fn default() -> Self {
        // Shared template types require Default; validation rejects an unconfigured action.
        Self::Signal {
            workflow_id: String::new(),
            run_id: None,
            signal_name: String::new(),
        }
    }
}

impl Action {
    pub(crate) fn workflow_id(&self) -> &str {
        match self {
            Self::Signal { workflow_id, .. }
            | Self::Start { workflow_id, .. }
            | Self::Update { workflow_id, .. }
            | Self::Cancel { workflow_id, .. }
            | Self::Terminate { workflow_id, .. } => workflow_id,
        }
    }

    pub(crate) fn run_id(&self) -> Option<&str> {
        match self {
            Self::Start { .. } => None,
            Self::Signal { run_id, .. }
            | Self::Update { run_id, .. }
            | Self::Cancel { run_id, .. }
            | Self::Terminate { run_id, .. } => run_id.as_deref(),
        }
    }

    pub(crate) fn strings(&self) -> Vec<(&str, &str, bool)> {
        let mut fields = vec![("workflowId", self.workflow_id(), true)];
        if let Some(run) = self.run_id() {
            fields.push(("runId", run, true));
        }
        match self {
            Self::Signal { signal_name, .. } => fields.push(("signalName", signal_name, true)),
            Self::Start {
                workflow_type,
                task_queue,
                ..
            } => {
                fields.push(("workflowType", workflow_type, true));
                fields.push(("taskQueue", task_queue, true));
            }
            Self::Update { update_name, .. } => fields.push(("updateName", update_name, true)),
            Self::Cancel { reason, .. } => fields.push(("reason", reason, false)),
            Self::Terminate { reason, .. } => fields.push(("reason", reason, true)),
        }
        fields
    }
}

/// One Temporal action and its JSON payload template.
pub type ActionSpec = TemplateSpec<Action>;
/// Actions for additions, updates, and removals from one query result.
pub type QueryActions = QueryConfig<Action>;

/// Exact-query, query-suffix, and default mappings, in that precedence order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "OutputTemplatesDto", into = "OutputTemplatesDto")]
pub struct OutputTemplates {
    pub default_template: Option<QueryActions>,
    pub routes: HashMap<String, QueryActions>,
}

/// Strict wire representation of one action specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::ActionSpec)]
pub struct ActionSpecDto {
    #[serde(default)]
    pub template: String,
    #[serde(flatten)]
    pub action: Action,
}

/// Strict wire representation of the three query operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::QueryActions)]
#[serde(deny_unknown_fields)]
pub struct QueryActionsDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<ActionSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<ActionSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<ActionSpecDto>,
}

/// Schema for declarative query routing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::temporal::OutputTemplates)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OutputTemplatesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryActionsDto>,
    #[serde(default)]
    pub routes: HashMap<String, QueryActionsDto>,
}

impl From<QueryActionsDto> for QueryActions {
    fn from(dto: QueryActionsDto) -> Self {
        fn spec(dto: ActionSpecDto) -> ActionSpec {
            TemplateSpec::with_extension(dto.template, dto.action)
        }
        Self {
            added: dto.added.map(spec),
            updated: dto.updated.map(spec),
            deleted: dto.deleted.map(spec),
        }
    }
}

impl From<QueryActions> for QueryActionsDto {
    fn from(config: QueryActions) -> Self {
        fn spec(config: ActionSpec) -> ActionSpecDto {
            ActionSpecDto {
                template: config.template,
                action: config.extension,
            }
        }
        Self {
            added: config.added.map(spec),
            updated: config.updated.map(spec),
            deleted: config.deleted.map(spec),
        }
    }
}

impl From<OutputTemplatesDto> for OutputTemplates {
    fn from(dto: OutputTemplatesDto) -> Self {
        Self {
            default_template: dto.default_template.map(Into::into),
            routes: dto.routes.into_iter().map(|(k, v)| (k, v.into())).collect(),
        }
    }
}

impl From<OutputTemplates> for OutputTemplatesDto {
    fn from(config: OutputTemplates) -> Self {
        Self {
            default_template: config.default_template.map(Into::into),
            routes: config
                .routes
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

impl TemplateRouting<Action> for OutputTemplates {
    fn routes(&self) -> &HashMap<String, QueryActions> {
        &self.routes
    }
    fn default_template(&self) -> Option<&QueryActions> {
        self.default_template.as_ref()
    }
    fn get_template_spec(&self, query: &str, op: OperationType) -> Option<&ActionSpec> {
        self.routes
            .get(query)
            .and_then(|q| Self::get_spec_from_config(q, op))
            .or_else(|| {
                self.routes
                    .get(query.rsplit('.').next().unwrap_or(query))
                    .and_then(|q| Self::get_spec_from_config(q, op))
            })
            .or_else(|| {
                self.default_template
                    .as_ref()
                    .and_then(|q| Self::get_spec_from_config(q, op))
            })
    }
}

/// PEM contents, not file paths. Neither field is printed through Debug.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ClientIdentity {
    pub certificate: String,
    pub private_key: String,
}

/// Certificate verification and optional mutual TLS.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TlsConfig {
    pub server_root_ca_cert: Option<String>,
    pub domain: Option<String>,
    pub client_identity: Option<ClientIdentity>,
}

/// Resolved configuration for an embedded reaction.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TemporalReactionConfig {
    pub endpoint: String,
    pub namespace: String,
    pub timeout_ms: u64,
    pub priority_queue_capacity: usize,
    pub output_templates: OutputTemplates,
    pub api_key: Option<String>,
    pub tls: Option<TlsConfig>,
}

impl Default for TemporalReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:7233".into(),
            namespace: "default".into(),
            timeout_ms: 10_000,
            priority_queue_capacity: 10_000,
            output_templates: OutputTemplates::default(),
            api_key: None,
            tls: None,
        }
    }
}

impl TemporalReactionConfig {
    pub fn validate(&self, queries: &[String]) -> Result<()> {
        let url = temporalio_client::Url::parse(&self.endpoint).context("invalid endpoint")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "endpoint must be an http(s) URL"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && matches!(url.path(), "" | "/"),
            "endpoint must not contain user info, a path, query, or fragment"
        );
        ensure!(
            !self.namespace.trim().is_empty(),
            "namespace must not be empty"
        );
        ensure!(
            self.timeout_ms > 0 && self.priority_queue_capacity > 0,
            "timeoutMs and priorityQueueCapacity must be positive"
        );
        if self.api_key.is_some() || self.tls.is_some() {
            ensure!(
                url.scheme() == "https",
                "API keys and TLS settings require HTTPS"
            );
        }
        if let Some(key) = &self.api_key {
            ensure!(!key.trim().is_empty(), "apiKey must not be empty");
        }
        if let Some(tls) = &self.tls {
            for field in [&tls.server_root_ca_cert, &tls.domain]
                .into_iter()
                .flatten()
            {
                ensure!(!field.trim().is_empty(), "TLS values must not be empty");
            }
            if let Some(identity) = &tls.client_identity {
                ensure!(
                    !identity.certificate.trim().is_empty()
                        && !identity.private_key.trim().is_empty(),
                    "mTLS requires both certificate and privateKey"
                );
            }
        }
        for key in self.output_templates.routes.keys() {
            ensure!(
                queries
                    .iter()
                    .any(|q| q == key || q.rsplit('.').next() == Some(key)),
                "route '{key}' does not match a subscribed query"
            );
        }
        for query in self
            .output_templates
            .routes
            .values()
            .chain(self.output_templates.default_template.iter())
        {
            for spec in [&query.added, &query.updated, &query.deleted]
                .into_iter()
                .flatten()
            {
                ensure!(
                    !matches!(spec.extension, Action::Cancel { .. }) || spec.template.is_empty(),
                    "cancel does not accept a payload template"
                );
                handlebars::Template::compile(&spec.template)
                    .context("invalid payload template")?;
                for (name, value, required) in spec.extension.strings() {
                    ensure!(
                        !required || !value.trim().is_empty(),
                        "{name} must not be empty"
                    );
                    handlebars::Template::compile(value)
                        .with_context(|| format!("invalid {name} template"))?;
                }
            }
        }
        Ok(())
    }
}
