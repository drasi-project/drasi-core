// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SSE template selection and wire rendering extracted from the SSE reaction.

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    exchange::{QueryResult, ResultDiff},
    ChangeNotification,
};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SseExtension {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

pub type TemplateSpec = crate::TemplateSpec<SseExtension>;
pub type QueryConfig = crate::QueryConfig<SseExtension>;

#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub path: String,
    pub payload: String,
}

pub struct SseRenderer {
    base_path: String,
    routes: HashMap<String, QueryConfig>,
    default_template: Option<QueryConfig>,
    handlebars: Handlebars<'static>,
}

pub fn register_json_helper(handlebars: &mut Handlebars<'_>) {
    handlebars.register_helper(
        "json",
        Box::new(
            |helper: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             output: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                let value = helper
                    .param(0)
                    .map(|parameter| parameter.value())
                    .unwrap_or(&Value::Null);
                output.write(&value.to_string())?;
                Ok(())
            },
        ),
    );
}

pub fn default_payload(result: &QueryResult, timestamp: i64) -> Value {
    json!({
        "queryId": result.query_id,
        "results": result.results,
        "timestamp": timestamp,
    })
}

/// The default browser frame for the ordered changes of one input: the
/// configured output (alias, component and port), the producing stream's sequence, the
/// sink's own wall-clock publication time (`publishedAt`, milliseconds; not an event
/// or business time) and each change's real operation, canonical identity and only the
/// images the record carries. A row consumer applies them to its own state.
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

impl SseRenderer {
    pub fn new(
        base_path: String,
        routes: HashMap<String, QueryConfig>,
        default_template: Option<QueryConfig>,
    ) -> Self {
        let mut handlebars = Handlebars::new();
        register_json_helper(&mut handlebars);
        Self {
            base_path,
            routes,
            default_template,
            handlebars,
        }
    }

    pub fn validate(
        routes: &HashMap<String, QueryConfig>,
        default_template: Option<&QueryConfig>,
        queries: &[String],
    ) -> Result<()> {
        let mut handlebars = Handlebars::new();
        register_json_helper(&mut handlebars);
        let validate = |config: &QueryConfig| -> Result<()> {
            for spec in [
                config.added.as_ref(),
                config.updated.as_ref(),
                config.deleted.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                for template in
                    std::iter::once(spec.template.as_str()).chain(spec.extension.path.as_deref())
                {
                    if !template.is_empty() {
                        handlebars
                            .render_template(template, &json!({}))
                            .context("Invalid template")?;
                    }
                }
            }
            Ok(())
        };
        for (query, config) in routes {
            validate(config).with_context(|| format!("Invalid template in route '{query}'"))?;
        }
        if let Some(config) = default_template {
            validate(config).context("Invalid default template")?;
        }
        if !queries.is_empty() {
            for route in routes.keys() {
                anyhow::ensure!(
                    queries
                        .iter()
                        .any(|query| query == route || query.ends_with(&format!(".{route}"))),
                    "Route '{route}' does not match any subscribed query. Subscribed queries: {queries:?}"
                );
            }
        }
        Ok(())
    }

    pub fn static_paths(&self) -> BTreeSet<String> {
        let mut paths = BTreeSet::from([self.base_path.clone()]);
        for config in self.routes.values().chain(self.default_template.iter()) {
            for spec in [
                config.added.as_ref(),
                config.updated.as_ref(),
                config.deleted.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(path) = spec
                    .extension
                    .path
                    .as_deref()
                    .filter(|path| !path.contains("{{"))
                {
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

    pub fn render(
        &self,
        result: &QueryResult,
        timestamp: i64,
        mut report_error: impl FnMut(String),
    ) -> Vec<SseEvent> {
        let config = self
            .routes
            .get(&result.query_id)
            .or_else(|| {
                result
                    .query_id
                    .rsplit_once('.')
                    .and_then(|(_, last)| self.routes.get(last))
            })
            .or(self.default_template.as_ref());
        let Some(config) = config else {
            return vec![SseEvent {
                path: self.base_path.clone(),
                payload: default_payload(result, timestamp).to_string(),
            }];
        };
        let mut events = Vec::new();
        for change in &result.results {
            let (spec, operation) = match change {
                ResultDiff::Add { .. } => (config.added.as_ref(), "ADD"),
                ResultDiff::Update { .. } => (config.updated.as_ref(), "UPDATE"),
                ResultDiff::Delete { .. } => (config.deleted.as_ref(), "DELETE"),
                ResultDiff::Aggregation { .. } => (config.updated.as_ref(), "AGGREGATION"),
                ResultDiff::Noop => continue,
            };
            let Some(spec) = spec else {
                continue;
            };
            let mut context = Map::new();
            match change {
                ResultDiff::Add { data, .. } => {
                    context.insert("after".into(), data.clone());
                }
                ResultDiff::Update {
                    data,
                    before,
                    after,
                    ..
                } => {
                    context.insert("data".into(), data.clone());
                    context.insert("before".into(), before.clone());
                    context.insert("after".into(), after.clone());
                }
                ResultDiff::Delete { data, .. } => {
                    context.insert("before".into(), data.clone());
                }
                ResultDiff::Aggregation { before, after, .. } => {
                    if let Some(before) = before {
                        context.insert("before".into(), before.clone());
                    }
                    context.insert("after".into(), after.clone());
                }
                ResultDiff::Noop => unreachable!("no-op filtered above"),
            }
            context.insert("query_id".into(), json!(result.query_id));
            context.insert("query_name".into(), json!(result.query_id));
            context.insert("operation".into(), json!(operation));
            context.insert("timestamp".into(), json!(timestamp));
            context.insert("metadata".into(), json!(result.metadata));

            let path = match spec.extension.path.as_deref() {
                Some(path) => {
                    let rendered = if path.contains("{{") {
                        self.handlebars
                            .render_template(path, &context)
                            .unwrap_or_else(|error| {
                                report_error(format!(
                                    "Failed to render SSE path for query '{}': {error}; using configured path",
                                    result.query_id
                                ));
                                path.to_owned()
                            })
                    } else {
                        path.to_owned()
                    };
                    self.resolve_path(&rendered)
                }
                None => self.base_path.clone(),
            };
            let default = || {
                json!({
                    "queryId": result.query_id,
                    "result": change,
                    "timestamp": timestamp,
                })
                .to_string()
            };
            let payload = if spec.template.is_empty() {
                default()
            } else {
                self.handlebars
                    .render_template(&spec.template, &context)
                    .unwrap_or_else(|error| {
                        report_error(format!(
                            "Failed to render SSE body for query '{}': {error}; using default result payload",
                            result.query_id
                        ));
                        default()
                    })
            };
            events.push(SseEvent { path, payload });
        }
        events
    }

    /// Render the ordered changes of one input from one configured output.
    ///
    /// Without a matching route the whole input is one default frame. With one,
    /// each change selects its `added`/`updated`/`deleted` template (an update
    /// covers a replacement and a patch; the context's `operation` tells them
    /// apart). The context is the truthful change notification: `output`,
    /// `sequence`, `operation`, `identity`, and the `element`, `before`, `after`
    /// and `removedProperties` that the record carries.
    pub fn render_changes(
        &self,
        notifications: &[ChangeNotification],
        timestamp: i64,
        mut report_error: impl FnMut(String),
    ) -> Result<Vec<SseEvent>> {
        let Some(first) = notifications.first() else {
            return Ok(Vec::new());
        };
        let alias = first.alias();
        let config = self
            .routes
            .get(alias)
            .or_else(|| {
                alias
                    .rsplit_once('.')
                    .and_then(|(_, last)| self.routes.get(last))
            })
            .or(self.default_template.as_ref());
        let Some(config) = config else {
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
                drasi_computation_contracts::ChangeRoute::Added => config.added.as_ref(),
                drasi_computation_contracts::ChangeRoute::Updated => config.updated.as_ref(),
                drasi_computation_contracts::ChangeRoute::Deleted => config.deleted.as_ref(),
            };
            let Some(spec) = spec else {
                continue;
            };
            let body = notification.to_json()?;
            let context = body.as_object().context("change notification object")?;
            let operation = change.operation();
            let path = match spec.extension.path.as_deref() {
                Some(path) => {
                    let rendered = if path.contains("{{") {
                        self.handlebars
                            .render_template(path, context)
                            .unwrap_or_else(|error| {
                                report_error(format!(
                                    "Failed to render SSE path for output '{alias}': {error}; using configured path"
                                ));
                                path.to_owned()
                            })
                    } else {
                        path.to_owned()
                    };
                    self.resolve_path(&rendered)
                }
                None => self.base_path.clone(),
            };
            let change_json = change.to_json()?;
            let default = || {
                json!({
                    "output": notification.output_json(),
                    "sequence": notification.sequence(),
                    "publishedAt": timestamp,
                    "change": change_json,
                })
                .to_string()
            };
            let payload = if spec.template.is_empty() {
                default()
            } else {
                self.handlebars
                    .render_template(&spec.template, context)
                    .unwrap_or_else(|error| {
                        report_error(format!(
                            "Failed to render SSE body for output '{alias}' ({operation}): {error}; using default change payload"
                        ));
                        default()
                    })
            };
            events.push(SseEvent { path, payload });
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests;
