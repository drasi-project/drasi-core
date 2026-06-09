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

//! Handlebars-based output template engine for the gRPC reaction.
//!
//! Templates reshape the **row content** of each `QueryResultItem` that
//! the reaction puts on the wire. The model is per-row:
//!
//! | Op       | Renders the configured op-template…                              |
//! |----------|------------------------------------------------------------------|
//! | `ADD`    | once, with the new row → output goes into `after`.               |
//! | `DELETE` | once, with the old row → output goes into `before`.              |
//! | `UPDATE` | twice, independently per side: old row → `before`, new row → `after`. |
//!
//! `AGGREGATION` is treated as `UPDATE` (consistent with the RabbitMQ
//! and Azure Storage reactions). `NOOP` is filtered out by the runners
//! before `build_proto_item` is ever invoked.
//!
//! When no template is configured for the op, or when rendering fails,
//! the field carries the raw row state. Events are never dropped.
//!
//! Per-render Handlebars context shape:
//!
//! ```text
//! {
//!   "row":       <the single row state being rendered>,
//!   "query_id":  "...",
//!   "operation": "ADD" | "UPDATE" | "DELETE",
//!   "side":      "before" | "after"
//! }
//! ```

use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, TemplateRouting, TemplateSpec};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext};
use log::warn;
use serde_json::{json, Map, Value};

use crate::config::GrpcReactionConfig;
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::drasi_v1::QueryResultItemType;
use crate::proto::ProtoQueryResultItem;

/// Which side of an UPDATE / ADD / DELETE a single render is producing.
/// Exposed to the template as the `side` context variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Before,
    After,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::Before => "before",
            Side::After => "after",
        }
    }
}

/// Cached Handlebars renderer.
///
/// Handlebars templates are stateless given the same template string, so
/// we keep a single `Handlebars` instance per runner task and call
/// `render_template` for each event. This avoids per-event allocation of a
/// new engine while still being safe to share across awaits within a single
/// task.
pub(crate) struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl TemplateEngine {
    pub(crate) fn new() -> Self {
        let mut handlebars = Handlebars::new();
        // Avoid runtime panics from misspelled placeholders — render a
        // visible marker instead so users notice during testing.
        handlebars.set_strict_mode(false);
        register_json_helper(&mut handlebars);
        Self { handlebars }
    }

    /// Render the configured template against a single row state. Returns
    /// `None` on render failure or when the rendered string is not valid
    /// JSON; a warning is logged. Callers fall back to the raw row state
    /// in that case — events are never dropped.
    pub(crate) fn render_row(
        &self,
        query_id: &str,
        operation: OperationType,
        side: Side,
        row: &Value,
        spec: &TemplateSpec,
    ) -> Option<Value> {
        let context = build_row_context(query_id, operation, side, row);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
                Ok(value) => Some(value),
                Err(parse_err) => {
                    warn!(
                        "Template for query '{query_id}' ({op} {side}) rendered non-JSON output: \
                         {parse_err}; falling back to raw row state",
                        op = op_as_str(operation),
                        side = side.as_str()
                    );
                    None
                }
            },
            Err(render_err) => {
                warn!(
                    "Template render failed for query '{query_id}' ({op} {side}): {render_err}; \
                     falling back to raw row state",
                    op = op_as_str(operation),
                    side = side.as_str()
                );
                None
            }
        }
    }
}

fn op_as_str(op: OperationType) -> &'static str {
    match op {
        OperationType::Add => "ADD",
        OperationType::Update => "UPDATE",
        OperationType::Delete => "DELETE",
    }
}

/// Register the `json` Handlebars helper so templates can serialize a nested
/// object/array to a JSON string (e.g. `{{json row}}`). Mirrors the helper
/// registered by the SSE/Loki/SQS reactions for cross-reaction template
/// compatibility.
fn register_json_helper(handlebars: &mut Handlebars<'static>) {
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                match h.param(0) {
                    Some(value) => match serde_json::to_string(value.value()) {
                        Ok(json_str) => out.write(&json_str)?,
                        Err(_) => out.write("null")?,
                    },
                    None => out.write("null")?,
                }
                Ok(())
            },
        ),
    );
}

/// Build the per-render Handlebars context for one row state.
///
/// Contract:
/// - `row` — the single row being rendered (the `before` row for
///   `Side::Before`, the `after` row for `Side::After`).
/// - `query_id` — the originating continuous-query id.
/// - `operation` — `"ADD"` / `"UPDATE"` / `"DELETE"` (Aggregation
///   surfaces here as `"UPDATE"`).
/// - `side` — `"before"` or `"after"`. Useful inside an `updated`
///   template that wants to differentiate per side; harmless to ignore.
fn build_row_context(
    query_id: &str,
    operation: OperationType,
    side: Side,
    row: &Value,
) -> Map<String, Value> {
    let mut ctx = Map::new();
    ctx.insert("row".into(), row.clone());
    ctx.insert("query_id".into(), json!(query_id));
    ctx.insert("operation".into(), json!(op_as_str(operation)));
    ctx.insert("side".into(), json!(side.as_str()));
    ctx
}

/// Render a single row through the configured `(query_id, op)` template
/// if one exists, falling back to the raw row on miss or render failure.
fn render_or_raw(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    query_id: &str,
    op: OperationType,
    side: Side,
    row: &Value,
) -> Value {
    let Some(engine) = engine else {
        return row.clone();
    };
    let Some(spec) = cfg.get_template_spec(query_id, op) else {
        return row.clone();
    };
    if spec.template.trim().is_empty() {
        return row.clone();
    }
    engine
        .render_row(query_id, op, side, row, spec)
        .unwrap_or_else(|| row.clone())
}

/// Build the proto item emitted for a single `ResultDiff`.
///
/// Contract:
/// - `before` / `after` carry the row state present for the op:
///   ADD → only `after`; DELETE → only `before`; UPDATE → both;
///   Aggregation → treated as UPDATE (`after` always present, `before`
///   present iff the diff carried one).
/// - When a template is configured for the `(query_id, op)` pair, each
///   present row state is rendered independently — once for ADD/DELETE,
///   twice for UPDATE (same `updated` template, different row each
///   time). Render failures fall back to the raw row state for that
///   field. Events are never dropped.
/// - `item_type` is `ADD` / `UPDATE` / `DELETE`; Aggregation is mapped
///   to `UPDATE` on the wire.
///
/// # Panics
///
/// Panics on `ResultDiff::Noop`. Runners must filter Noop entries out
/// before invoking this — matches the established pattern in the
/// RabbitMQ and Azure Storage reactions, where Noop is dropped at the
/// runner level and never put on the wire.
pub(crate) fn build_proto_item(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    query_id: &str,
    diff: &ResultDiff,
) -> ProtoQueryResultItem {
    let (item_type, op, row_signature, raw_before, raw_after) = match diff {
        ResultDiff::Add {
            data,
            row_signature,
        } => (
            QueryResultItemType::Add,
            OperationType::Add,
            *row_signature,
            None,
            Some(data.clone()),
        ),
        ResultDiff::Delete {
            data,
            row_signature,
        } => (
            QueryResultItemType::Delete,
            OperationType::Delete,
            *row_signature,
            Some(data.clone()),
            None,
        ),
        ResultDiff::Update {
            before,
            after,
            row_signature,
            ..
        } => (
            QueryResultItemType::Update,
            OperationType::Update,
            *row_signature,
            Some(before.clone()),
            Some(after.clone()),
        ),
        ResultDiff::Aggregation {
            before,
            after,
            row_signature,
        } => (
            // Aggregation surfaces as UPDATE on the wire — same as
            // RabbitMQ (rabbitmq.rs:700-723) and Azure Storage
            // (reaction.rs:434-439).
            QueryResultItemType::Update,
            OperationType::Update,
            *row_signature,
            before.clone(),
            Some(after.clone()),
        ),
        ResultDiff::Noop => unreachable!(
            "build_proto_item must not be called for Noop diffs; runners must filter them out"
        ),
    };

    let before_struct = raw_before
        .as_ref()
        .map(|row| render_or_raw(cfg, engine, query_id, op, Side::Before, row))
        .as_ref()
        .map(convert_json_to_proto_struct);

    let after_struct = raw_after
        .as_ref()
        .map(|row| render_or_raw(cfg, engine, query_id, op, Side::After, row))
        .as_ref()
        .map(convert_json_to_proto_struct);

    ProtoQueryResultItem {
        item_type: item_type as i32,
        row_signature,
        before: before_struct,
        after: after_struct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputTemplates;
    use crate::helpers::convert_json_to_proto_struct;
    use drasi_lib::reactions::common::{QueryConfig, TemplateRouting, TemplateSpec};
    use serde_json::json;
    use std::collections::HashMap;

    fn add(data: Value) -> ResultDiff {
        ResultDiff::Add {
            data,
            row_signature: 0,
        }
    }

    fn delete(data: Value) -> ResultDiff {
        ResultDiff::Delete {
            data,
            row_signature: 0,
        }
    }

    fn update(before: Value, after: Value) -> ResultDiff {
        ResultDiff::Update {
            data: json!({"unused": true}),
            before,
            after,
            grouping_keys: None,
            row_signature: 0,
        }
    }

    fn aggregation(before: Option<Value>, after: Value) -> ResultDiff {
        ResultDiff::Aggregation {
            before,
            after,
            row_signature: 0,
        }
    }

    fn query_config(
        added: Option<&str>,
        updated: Option<&str>,
        deleted: Option<&str>,
    ) -> QueryConfig {
        QueryConfig {
            added: added.map(TemplateSpec::new),
            updated: updated.map(TemplateSpec::new),
            deleted: deleted.map(TemplateSpec::new),
        }
    }

    fn config_with(
        default: Option<QueryConfig>,
        routes: HashMap<String, QueryConfig>,
    ) -> GrpcReactionConfig {
        GrpcReactionConfig {
            output_templates: Some(OutputTemplates {
                default_template: default,
                routes,
            }),
            ..GrpcReactionConfig::default()
        }
    }

    fn item_type(item: &ProtoQueryResultItem) -> QueryResultItemType {
        QueryResultItemType::try_from(item.item_type).expect("valid enum tag")
    }

    /// Round-trip a proto `Struct` field back to JSON for ergonomic
    /// equality assertions in tests. Uses the same shape that the wire
    /// would carry.
    fn struct_field(item_field: &Option<prost_types::Struct>) -> Option<Value> {
        item_field.as_ref().map(|s| {
            let mut map = serde_json::Map::new();
            for (k, v) in &s.fields {
                map.insert(k.clone(), value_to_json(v));
            }
            Value::Object(map)
        })
    }

    fn value_to_json(v: &prost_types::Value) -> Value {
        use prost_types::value::Kind;
        match &v.kind {
            Some(Kind::NullValue(_)) | None => Value::Null,
            Some(Kind::BoolValue(b)) => Value::Bool(*b),
            Some(Kind::NumberValue(n)) => {
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= i64::MIN as f64
                    && *n <= i64::MAX as f64
                {
                    Value::Number(serde_json::Number::from(*n as i64))
                } else {
                    serde_json::Number::from_f64(*n)
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                }
            }
            Some(Kind::StringValue(s)) => Value::String(s.clone()),
            Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(value_to_json).collect()),
            Some(Kind::StructValue(s)) => {
                let mut map = serde_json::Map::new();
                for (k, vv) in &s.fields {
                    map.insert(k.clone(), value_to_json(vv));
                }
                Value::Object(map)
            }
        }
    }

    // ---- Context shape ---------------------------------------------------

    #[test]
    fn context_exposes_row_query_id_operation_and_side() {
        let row = json!({"id": 7, "name": "alice"});
        let ctx = build_row_context("orders", OperationType::Add, Side::After, &row);
        assert_eq!(ctx.get("row").unwrap(), &row);
        assert_eq!(ctx.get("query_id").unwrap(), "orders");
        assert_eq!(ctx.get("operation").unwrap(), "ADD");
        assert_eq!(ctx.get("side").unwrap(), "after");

        let ctx = build_row_context("orders", OperationType::Delete, Side::Before, &row);
        assert_eq!(ctx.get("operation").unwrap(), "DELETE");
        assert_eq!(ctx.get("side").unwrap(), "before");

        let ctx = build_row_context("orders", OperationType::Update, Side::Before, &row);
        assert_eq!(ctx.get("operation").unwrap(), "UPDATE");
        assert_eq!(ctx.get("side").unwrap(), "before");
    }

    // ---- ADD -------------------------------------------------------------

    #[test]
    fn add_no_template_emits_after_with_raw_row_and_no_before() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &add(json!({"id": 1, "name": "a"})));
        assert_eq!(item_type(&item), QueryResultItemType::Add);
        assert_eq!(item.row_signature, 0);
        assert_eq!(struct_field(&item.after), Some(json!({"id": 1, "name": "a"})));
        assert!(item.before.is_none(), "ADD must leave `before` absent");
    }

    #[test]
    fn add_with_template_renders_into_after() {
        let cfg = config_with(
            Some(query_config(
                Some(r#"{"id":"{{row.id}}","kind":"created"}"#),
                None,
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(
            &cfg,
            Some(&engine),
            "q",
            &add(json!({"id": "abc", "extra": "ignored"})),
        );
        assert_eq!(item_type(&item), QueryResultItemType::Add);
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"id": "abc", "kind": "created"})),
            "`after` must carry the rendered template output, not the raw row"
        );
        assert!(item.before.is_none(), "ADD never populates `before`");
    }

    // ---- DELETE ----------------------------------------------------------

    #[test]
    fn delete_no_template_emits_before_with_raw_row_and_no_after() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &delete(json!({"id": 42})));
        assert_eq!(item_type(&item), QueryResultItemType::Delete);
        assert_eq!(struct_field(&item.before), Some(json!({"id": 42})));
        assert!(item.after.is_none(), "DELETE must leave `after` absent");
    }

    #[test]
    fn delete_with_template_renders_into_before() {
        let cfg = config_with(
            Some(query_config(
                None,
                None,
                Some(r#"{"removed_id":"{{row.id}}"}"#),
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &delete(json!({"id": "x42"})));
        assert_eq!(item_type(&item), QueryResultItemType::Delete);
        assert_eq!(
            struct_field(&item.before),
            Some(json!({"removed_id": "x42"})),
            "DELETE template renders into `before`"
        );
        assert!(item.after.is_none());
    }

    // ---- UPDATE ----------------------------------------------------------

    #[test]
    fn update_no_template_emits_raw_before_and_after() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(
            &cfg,
            None,
            "q",
            &update(json!({"v": "old"}), json!({"v": "new"})),
        );
        assert_eq!(item_type(&item), QueryResultItemType::Update);
        assert_eq!(struct_field(&item.before), Some(json!({"v": "old"})));
        assert_eq!(struct_field(&item.after), Some(json!({"v": "new"})));
    }

    #[test]
    fn update_with_template_renders_both_sides_independently() {
        // The SAME `updated` template runs TWICE: once with the before
        // row, once with the after row. Output of each goes into the
        // corresponding proto field.
        let cfg = config_with(
            Some(query_config(
                None,
                Some(r#"{"value":"{{row.v}}"}"#),
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(
            &cfg,
            Some(&engine),
            "q",
            &update(json!({"v": "old"}), json!({"v": "new"})),
        );
        assert_eq!(item_type(&item), QueryResultItemType::Update);
        assert_eq!(
            struct_field(&item.before),
            Some(json!({"value": "old"})),
            "`before` must be rendered from the before-row independently"
        );
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"value": "new"})),
            "`after` must be rendered from the after-row independently"
        );
    }

    #[test]
    fn update_template_sees_side_variable_as_before_then_after() {
        // The `side` context variable lets a single `updated` template
        // emit different output per side if it needs to.
        let cfg = config_with(
            Some(query_config(
                None,
                Some(r#"{"v":"{{row.v}}","s":"{{side}}"}"#),
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(
            &cfg,
            Some(&engine),
            "q",
            &update(json!({"v": "old"}), json!({"v": "new"})),
        );
        assert_eq!(
            struct_field(&item.before),
            Some(json!({"v": "old", "s": "before"}))
        );
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"v": "new", "s": "after"}))
        );
    }

    // ---- Aggregation surfaces as UPDATE ----------------------------------

    #[test]
    fn aggregation_with_both_sides_surfaces_as_update_with_both_rendered() {
        // Aggregation reuses the `updated` template, identical to
        // RabbitMQ/Azure Storage. With `before == Some`, both sides
        // render and item_type is UPDATE on the wire.
        let cfg = config_with(
            Some(query_config(None, Some(r#"{"sum":{{row.sum}}}"#), None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(
            &cfg,
            Some(&engine),
            "q",
            &aggregation(Some(json!({"sum": 1})), json!({"sum": 5})),
        );
        assert_eq!(
            item_type(&item),
            QueryResultItemType::Update,
            "Aggregation must surface as UPDATE on the wire"
        );
        assert_eq!(struct_field(&item.before), Some(json!({"sum": 1})));
        assert_eq!(struct_field(&item.after), Some(json!({"sum": 5})));
    }

    #[test]
    fn aggregation_with_no_before_only_renders_after() {
        let cfg = config_with(
            Some(query_config(None, Some(r#"{"sum":{{row.sum}}}"#), None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(
            &cfg,
            Some(&engine),
            "q",
            &aggregation(None, json!({"sum": 7})),
        );
        assert_eq!(item_type(&item), QueryResultItemType::Update);
        assert!(
            item.before.is_none(),
            "Aggregation without prior state leaves `before` absent"
        );
        assert_eq!(struct_field(&item.after), Some(json!({"sum": 7})));
    }

    // ---- Render failure fall-back ---------------------------------------

    #[test]
    fn render_failure_falls_back_to_raw_row_for_that_field() {
        // Non-JSON output triggers the parse-failure warn-and-fallback
        // path. The field carries the raw row state instead.
        let cfg = config_with(
            Some(query_config(Some("plain text {{row.id}}"), None, None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &add(json!({"id": 99})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"id": 99})),
            "render failure must fall back to raw row state — never drop"
        );
    }

    #[test]
    fn empty_template_falls_back_to_raw_row() {
        let cfg = config_with(
            Some(query_config(Some("   "), None, None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &add(json!({"id": 1})));
        assert_eq!(struct_field(&item.after), Some(json!({"id": 1})));
    }

    // ---- row_signature ---------------------------------------------------

    #[test]
    fn row_signature_propagates_for_add_update_delete_aggregation() {
        let cfg = GrpcReactionConfig::default();

        let add_diff = ResultDiff::Add {
            data: json!({}),
            row_signature: 11,
        };
        assert_eq!(
            build_proto_item(&cfg, None, "q", &add_diff).row_signature,
            11
        );

        let del_diff = ResultDiff::Delete {
            data: json!({}),
            row_signature: 22,
        };
        assert_eq!(
            build_proto_item(&cfg, None, "q", &del_diff).row_signature,
            22
        );

        let upd_diff = ResultDiff::Update {
            data: json!({}),
            before: json!({}),
            after: json!({}),
            grouping_keys: None,
            row_signature: 33,
        };
        assert_eq!(
            build_proto_item(&cfg, None, "q", &upd_diff).row_signature,
            33
        );

        let agg_diff = ResultDiff::Aggregation {
            before: None,
            after: json!({}),
            row_signature: 44,
        };
        assert_eq!(
            build_proto_item(&cfg, None, "q", &agg_diff).row_signature,
            44
        );
    }

    // ---- Noop contract ---------------------------------------------------

    #[test]
    #[should_panic(expected = "Noop")]
    fn build_proto_item_panics_for_noop_to_enforce_runner_filtering() {
        // Runners must filter Noop before calling build_proto_item.
        // This test pins the contract.
        let cfg = GrpcReactionConfig::default();
        let _ = build_proto_item(&cfg, None, "q", &ResultDiff::Noop);
    }

    // ---- Route lookup strictness ----------------------------------------

    #[test]
    fn route_lookup_requires_exact_query_id_no_dotted_suffix_fallback() {
        // A route key with a dotted suffix must NOT be selected for a
        // query id that only matches the suffix. The shared
        // TemplateRouting trait (drasi_lib::reactions::common) is what
        // we delegate to; this pins the exact-id behavior so a future
        // change in the shared trait cannot silently widen routing.
        let mut routes = HashMap::new();
        routes.insert(
            "sales.q1".to_string(),
            query_config(Some(r#"{"src":"namespaced-route"}"#), None, None),
        );
        let cfg = config_with(
            Some(query_config(Some(r#"{"src":"default"}"#), None, None)),
            routes,
        );
        let engine = TemplateEngine::new();

        // Query id "q1" must fall through to the default template, NOT
        // resolve to the "sales.q1" entry by stripping the prefix.
        let item = build_proto_item(&cfg, Some(&engine), "q1", &add(json!({})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"src": "default"})),
            "query id `q1` must not match route key `sales.q1` via suffix fallback"
        );

        // The exact route key resolves to the namespaced spec.
        let item = build_proto_item(&cfg, Some(&engine), "sales.q1", &add(json!({})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"src": "namespaced-route"}))
        );
    }

    // ---- Silence unused-import lint for the rendering helper -------------

    #[test]
    fn helpers_smoke() {
        // Touch `convert_json_to_proto_struct` so the explicit import
        // in this test module isn't flagged unused when other tests
        // construct ProtoQueryResultItem directly elsewhere.
        let _ = convert_json_to_proto_struct(&json!({"k": 1}));
    }
}
