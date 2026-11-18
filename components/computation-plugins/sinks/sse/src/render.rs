// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Route selection and frame rendering.
//!
//! Body and path templates are compiled once, when the renderer is built
//! (construction or an in-place update), by the shared native template engine
//! (`drasi-computation-template-common`): strict, unescaped and checked
//! statically against the shared helper set in every branch. A render failure
//! is a permanent delivery failure of the input, so the declared
//! `onPermanentFailure` applies; nothing falls back to another body or to the
//! unrendered path (D-5).

use anyhow::{Context, Result};
use drasi_computation_contracts::{ChangeNotification, ChangeRoute, DeliveryFailure};
use drasi_computation_template_common::{change_context, Templates};
use drasi_reaction_common::sse::QueryConfig;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

/// One frame to broadcast on one path.
#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub path: String,
    pub payload: String,
}

/// The compiled routes of one configuration.
pub(crate) struct SseRenderer {
    base_path: String,
    routes: HashMap<String, QueryConfig>,
    default_template: Option<QueryConfig>,
    templates: Templates,
}

fn specs(config: &QueryConfig) -> impl Iterator<Item = &drasi_reaction_common::sse::TemplateSpec> {
    [
        config.added.as_ref(),
        config.updated.as_ref(),
        config.deleted.as_ref(),
    ]
    .into_iter()
    .flatten()
}

/// Whether a configured path is a template rather than a literal path.
fn is_template(path: &str) -> bool {
    path.contains("{{")
}

impl SseRenderer {
    /// Compiles every body and path template.
    pub fn new(
        base_path: String,
        routes: HashMap<String, QueryConfig>,
        default_template: Option<QueryConfig>,
    ) -> Result<Self> {
        let mut templates = Templates::new();
        let named = routes
            .iter()
            .map(|(route, config)| (format!("route '{route}'"), config))
            .chain(
                default_template
                    .iter()
                    .map(|config| ("defaultTemplate".to_owned(), config)),
            );
        for (label, config) in named {
            for spec in specs(config) {
                templates
                    .compile(&spec.template)
                    .with_context(|| format!("invalid SSE body template in {label}"))?;
                if let Some(path) = spec.extension.path.as_deref().filter(|p| is_template(p)) {
                    templates
                        .compile(path)
                        .with_context(|| format!("invalid SSE path template in {label}"))?;
                }
            }
        }
        Ok(Self {
            base_path,
            routes,
            default_template,
            templates,
        })
    }

    /// Every route must select at least one output alias, by full alias or by
    /// its last dotted segment.
    pub fn validate_routes<'a>(
        routes: impl Iterator<Item = &'a String>,
        aliases: &[String],
    ) -> Result<()> {
        for route in routes {
            anyhow::ensure!(
                aliases
                    .iter()
                    .any(|alias| alias == route || alias.ends_with(&format!(".{route}"))),
                "Route '{route}' does not match any output. Outputs: {aliases:?}"
            );
        }
        Ok(())
    }

    /// The base path and every literal (untemplated) configured path.
    pub fn static_paths(&self) -> BTreeSet<String> {
        let mut paths = BTreeSet::from([self.base_path.clone()]);
        for config in self.routes.values().chain(self.default_template.iter()) {
            for spec in specs(config) {
                if let Some(path) = spec.extension.path.as_deref().filter(|p| !is_template(p)) {
                    paths.insert(self.resolve_path(path));
                }
            }
        }
        paths
    }

    fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("{}/{path}", self.base_path)
        }
    }

    fn route(&self, alias: &str) -> Option<&QueryConfig> {
        self.routes
            .get(alias)
            .or_else(|| {
                alias
                    .rsplit_once('.')
                    .and_then(|(_, last)| self.routes.get(last))
            })
            .or(self.default_template.as_ref())
    }

    /// Renders the ordered changes of one input from one configured output.
    ///
    /// Without a matching route the whole input is one default frame. With
    /// one, each change selects its `added`/`updated`/`deleted` template (an
    /// update covers a replacement and a patch; the context's `operation`
    /// tells them apart); a change whose operation has no template is not
    /// broadcast. A render failure fails the whole input permanently.
    pub fn render_changes(
        &self,
        notifications: &[ChangeNotification],
        timestamp: i64,
    ) -> Result<Vec<SseEvent>> {
        let Some(first) = notifications.first() else {
            return Ok(Vec::new());
        };
        let alias = first.alias();
        let Some(config) = self.route(alias) else {
            let changes = notifications
                .iter()
                .map(|notification| notification.change().to_json())
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(vec![SseEvent {
                path: self.base_path.clone(),
                payload: default_changes_payload(
                    first.output_json(),
                    first.sequence(),
                    timestamp,
                    changes,
                )
                .to_string(),
            }]);
        };
        let mut events = Vec::new();
        for notification in notifications {
            let change = notification.change();
            let spec = match change.route() {
                ChangeRoute::Added => config.added.as_ref(),
                ChangeRoute::Updated => config.updated.as_ref(),
                ChangeRoute::Deleted => config.deleted.as_ref(),
            };
            let Some(spec) = spec else {
                continue;
            };
            let context = change_context(notification)?;
            let failed = |part: &str, error: drasi_computation_template_common::TemplateError| {
                anyhow::Error::from(DeliveryFailure::permanent(
                    anyhow::Error::new(error).context(format!(
                        "SSE {part} template for output '{alias}' ({}) failed to render",
                        change.operation()
                    )),
                ))
            };
            let path = match spec.extension.path.as_deref() {
                Some(path) if is_template(path) => self.resolve_path(
                    &self
                        .templates
                        .render(path, &context)
                        .map_err(|error| failed("path", error))?,
                ),
                Some(path) => self.resolve_path(path),
                None => self.base_path.clone(),
            };
            let payload = if spec.template.is_empty() {
                json!({
                    "output": notification.output_json(),
                    "sequence": notification.sequence(),
                    "publishedAt": timestamp,
                    "change": change.to_json()?,
                })
                .to_string()
            } else {
                self.templates
                    .render(&spec.template, &context)
                    .map_err(|error| failed("body", error))?
            };
            events.push(SseEvent { path, payload });
        }
        Ok(events)
    }
}

/// The default browser frame for the ordered changes of one input: the
/// configured output (alias, component and port), the producing stream's
/// sequence, the sink's own wall-clock publication time (`publishedAt`,
/// milliseconds; not an event or business time) and each change's real
/// operation, canonical identity and only the images the record carries. A
/// row consumer applies them to its own state.
pub fn default_changes_payload(
    output: Value,
    sequence: u64,
    published_at: i64,
    changes: Vec<Value>,
) -> Value {
    json!({
        "output": output,
        "sequence": sequence,
        "publishedAt": published_at,
        "changes": changes,
    })
}
