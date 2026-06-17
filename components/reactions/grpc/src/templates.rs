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
//! Per-render Handlebars context shape. Alongside the row being
//! rendered, every render receives the standard cross-reaction template
//! keys so templates are portable with the other Drasi reactions:
//!
//! ```text
//! {
//!   "query_id":  "...",                       // originating query id
//!   "query_name":"...",                       // alias of query_id
//!   "operation": "ADD" | "UPDATE" | "DELETE", // Aggregation surfaces as UPDATE
//!   "timestamp": "RFC3339",                   // emission timestamp
//!   "metadata":  { ... },                     // result metadata (may be empty)
//!   "before":    <before row>,                // present for UPDATE/DELETE
//!   "after":     <after row>,                 // present for ADD/UPDATE
//!   "data":      <raw data>,                  // present for UPDATE
//!   "row":       <the single row being rendered>, // reaction-specific convenience
//!   "side":      "before" | "after"               // reaction-specific convenience
//! }
//! ```

use std::collections::HashMap;

use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
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
    fn render_row(
        &self,
        item: &ItemContext<'_>,
        side: Side,
        row: &Value,
        spec: &TemplateSpec,
    ) -> Option<Value> {
        let context = build_row_context(item, side, row);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
                Ok(value) => Some(value),
                Err(parse_err) => {
                    warn!(
                        "Template for query '{query_id}' ({op} {side}) rendered non-JSON output: \
                         {parse_err}; falling back to raw row state",
                        query_id = item.emission.query_id,
                        op = op_as_str(item.operation),
                        side = side.as_str()
                    );
                    None
                }
            },
            Err(render_err) => {
                warn!(
                    "Template render failed for query '{query_id}' ({op} {side}): {render_err}; \
                     falling back to raw row state",
                    query_id = item.emission.query_id,
                    op = op_as_str(item.operation),
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

/// Query-level context assembled once per dequeued `QueryResult` and
/// shared across every item rendered from it.
pub(crate) struct QueryEmissionContext<'a> {
    /// Originating continuous-query id.
    pub query_id: &'a str,
    /// Monotonic per-query emission sequence number.
    pub sequence: u64,
    /// Emission timestamp, pre-formatted as RFC3339.
    pub timestamp: &'a str,
    /// Result metadata (may be empty).
    pub metadata: &'a HashMap<String, Value>,
}

/// Per-item context: the query-level emission context plus the row
/// states carried by one `ResultDiff`. Used while rendering the item's
/// `before` / `after` sides.
struct ItemContext<'a> {
    emission: &'a QueryEmissionContext<'a>,
    operation: OperationType,
    before: Option<&'a Value>,
    after: Option<&'a Value>,
    data: Option<&'a Value>,
}

/// Build the per-render Handlebars context for one row state.
///
/// Populates the standard cross-reaction keys (`query_id`, `query_name`,
/// `operation`, `timestamp`, `metadata`, plus `before` / `after` / `data`
/// where present) so templates are portable with the other Drasi
/// reactions, alongside the reaction-specific `row` (the single row being
/// rendered) and `side` (`"before"` / `"after"`) conveniences.
fn build_row_context(item: &ItemContext<'_>, side: Side, row: &Value) -> Map<String, Value> {
    let mut ctx = Map::new();
    // Standard cross-reaction keys (stable names; see developer guide §11).
    ctx.insert("query_id".into(), json!(item.emission.query_id));
    ctx.insert("query_name".into(), json!(item.emission.query_id));
    ctx.insert("operation".into(), json!(op_as_str(item.operation)));
    ctx.insert("timestamp".into(), json!(item.emission.timestamp));
    ctx.insert(
        "metadata".into(),
        Value::Object(item.emission.metadata.clone().into_iter().collect()),
    );
    if let Some(before) = item.before {
        ctx.insert("before".into(), before.clone());
    }
    if let Some(after) = item.after {
        ctx.insert("after".into(), after.clone());
    }
    if let Some(data) = item.data {
        ctx.insert("data".into(), data.clone());
    }
    // Reaction-specific conveniences for the per-side rendering model.
    ctx.insert("row".into(), row.clone());
    ctx.insert("side".into(), json!(side.as_str()));
    ctx
}

/// Resolve the template spec for `(query_id, op)` following the standard
/// reaction resolution order: exact route → last-dotted-segment route →
/// default template.
fn resolve_spec<'a>(
    cfg: &'a GrpcReactionConfig,
    query_id: &str,
    op: OperationType,
) -> Option<&'a TemplateSpec> {
    let routes = cfg.routes();
    if let Some(spec) = routes.get(query_id).and_then(|qc| spec_for_op(qc, op)) {
        return Some(spec);
    }
    // Fall back to the last dotted segment of the query id, so a route
    // keyed `my_query` matches a wire id `source.my_query`.
    if let Some(segment) = query_id.rsplit('.').next() {
        if segment != query_id {
            if let Some(spec) = routes.get(segment).and_then(|qc| spec_for_op(qc, op)) {
                return Some(spec);
            }
        }
    }
    cfg.default_template().and_then(|qc| spec_for_op(qc, op))
}

fn spec_for_op(qc: &QueryConfig, op: OperationType) -> Option<&TemplateSpec> {
    match op {
        OperationType::Add => qc.added.as_ref(),
        OperationType::Update => qc.updated.as_ref(),
        OperationType::Delete => qc.deleted.as_ref(),
    }
}

/// Compile-check a single Handlebars template string. Empty / whitespace
/// templates are valid — they signal "use the raw row state".
pub(crate) fn validate_template(template: &str) -> anyhow::Result<()> {
    if template.trim().is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(template)
        .map_err(|e| anyhow::anyhow!("invalid Handlebars template: {e}"))?;
    Ok(())
}

/// Render a single row through the configured `(query_id, op)` template
/// if one exists, falling back to the raw row on miss or render failure.
fn render_or_raw(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    item: &ItemContext<'_>,
    side: Side,
    row: &Value,
) -> Value {
    let Some(engine) = engine else {
        return row.clone();
    };
    let Some(spec) = resolve_spec(cfg, item.emission.query_id, item.operation) else {
        return row.clone();
    };
    if spec.template.trim().is_empty() {
        return row.clone();
    }
    engine
        .render_row(item, side, row, spec)
        .unwrap_or_else(|| row.clone())
}

/// Build the proto item emitted for a single `ResultDiff`, or `None` for
/// `ResultDiff::Noop` (which carries no row data and is never emitted).
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
/// - `sequence` is copied from the originating emission so receivers can
///   order / de-duplicate.
pub(crate) fn build_proto_item(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    emission: &QueryEmissionContext<'_>,
    diff: &ResultDiff,
) -> Option<ProtoQueryResultItem> {
    let (item_type, op, row_signature, raw_before, raw_after, raw_data) = match diff {
        ResultDiff::Add {
            data,
            row_signature,
        } => (
            QueryResultItemType::Add,
            OperationType::Add,
            *row_signature,
            None,
            Some(data.clone()),
            None,
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
            None,
        ),
        ResultDiff::Update {
            before,
            after,
            data,
            row_signature,
            ..
        } => (
            QueryResultItemType::Update,
            OperationType::Update,
            *row_signature,
            Some(before.clone()),
            Some(after.clone()),
            Some(data.clone()),
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
            None,
        ),
        // Noop carries no row data and is never put on the wire. Runners
        // filter it before calling, but returning None here keeps the
        // function panic-free as a defensive measure.
        ResultDiff::Noop => return None,
    };

    let item = ItemContext {
        emission,
        operation: op,
        before: raw_before.as_ref(),
        after: raw_after.as_ref(),
        data: raw_data.as_ref(),
    };

    let before_struct = raw_before
        .as_ref()
        .map(|row| render_or_raw(cfg, engine, &item, Side::Before, row))
        .as_ref()
        .map(convert_json_to_proto_struct);

    let after_struct = raw_after
        .as_ref()
        .map(|row| render_or_raw(cfg, engine, &item, Side::After, row))
        .as_ref()
        .map(convert_json_to_proto_struct);

    Some(ProtoQueryResultItem {
        item_type: item_type as i32,
        row_signature,
        before: before_struct,
        after: after_struct,
        sequence: emission.sequence,
    })
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

    /// Owns the backing strings/maps for a `QueryEmissionContext` so tests
    /// can build one with a single borrow.
    struct TestEmission {
        query_id: String,
        ts: String,
        metadata: HashMap<String, Value>,
        sequence: u64,
    }

    impl TestEmission {
        fn new(query_id: &str) -> Self {
            Self {
                query_id: query_id.to_string(),
                ts: "2026-06-16T00:00:00Z".to_string(),
                metadata: HashMap::new(),
                sequence: 0,
            }
        }

        fn ctx(&self) -> QueryEmissionContext<'_> {
            QueryEmissionContext {
                query_id: &self.query_id,
                sequence: self.sequence,
                timestamp: &self.ts,
                metadata: &self.metadata,
            }
        }
    }

    /// Test convenience: build the item for a non-Noop diff with a default
    /// emission context (sequence 0, empty metadata). Panics if the diff is
    /// Noop, which has no item.
    fn item_for(
        cfg: &GrpcReactionConfig,
        engine: Option<&TemplateEngine>,
        query_id: &str,
        diff: &ResultDiff,
    ) -> ProtoQueryResultItem {
        let emission = TestEmission::new(query_id);
        build_proto_item(cfg, engine, &emission.ctx(), diff).expect("non-noop diff yields an item")
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
    fn context_exposes_standard_keys_plus_row_and_side() {
        let before = json!({"id": 7, "name": "old"});
        let after = json!({"id": 7, "name": "new"});
        let data = json!({"raw": true});
        let mut metadata = HashMap::new();
        metadata.insert("tenant".to_string(), json!("acme"));
        let emission = QueryEmissionContext {
            query_id: "orders",
            sequence: 9,
            timestamp: "2026-06-16T00:00:00Z",
            metadata: &metadata,
        };
        let item = ItemContext {
            emission: &emission,
            operation: OperationType::Update,
            before: Some(&before),
            after: Some(&after),
            data: Some(&data),
        };

        let ctx = build_row_context(&item, Side::After, &after);
        // Standard cross-reaction keys.
        assert_eq!(ctx.get("query_id").unwrap(), "orders");
        assert_eq!(ctx.get("query_name").unwrap(), "orders");
        assert_eq!(ctx.get("operation").unwrap(), "UPDATE");
        assert_eq!(ctx.get("timestamp").unwrap(), "2026-06-16T00:00:00Z");
        assert_eq!(ctx.get("metadata").unwrap(), &json!({"tenant": "acme"}));
        assert_eq!(ctx.get("before").unwrap(), &before);
        assert_eq!(ctx.get("after").unwrap(), &after);
        assert_eq!(ctx.get("data").unwrap(), &data);
        // Reaction-specific conveniences.
        assert_eq!(ctx.get("row").unwrap(), &after);
        assert_eq!(ctx.get("side").unwrap(), "after");

        let ctx = build_row_context(&item, Side::Before, &before);
        assert_eq!(ctx.get("side").unwrap(), "before");
        assert_eq!(ctx.get("row").unwrap(), &before);
    }

    #[test]
    fn context_omits_absent_row_states() {
        // For an ADD the context carries `after` but not `before`/`data`.
        let after = json!({"id": 1});
        let metadata = HashMap::new();
        let emission = QueryEmissionContext {
            query_id: "q",
            sequence: 0,
            timestamp: "2026-06-16T00:00:00Z",
            metadata: &metadata,
        };
        let item = ItemContext {
            emission: &emission,
            operation: OperationType::Add,
            before: None,
            after: Some(&after),
            data: None,
        };
        let ctx = build_row_context(&item, Side::After, &after);
        assert!(ctx.get("before").is_none());
        assert!(ctx.get("data").is_none());
        assert_eq!(ctx.get("after").unwrap(), &after);
    }

    #[test]
    fn template_can_use_standard_after_key() {
        // A template written with the portable `{{after.id}}` key (rather
        // than the reaction-specific `{{row.id}}`) renders correctly.
        let cfg = config_with(
            Some(query_config(Some(r#"{"id":"{{after.id}}"}"#), None, None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = item_for(&cfg, Some(&engine), "q", &add(json!({"id": "abc"})));
        assert_eq!(struct_field(&item.after), Some(json!({"id": "abc"})));
    }

    // ---- ADD -------------------------------------------------------------

    #[test]
    fn add_no_template_emits_after_with_raw_row_and_no_before() {
        let cfg = GrpcReactionConfig::default();
        let item = item_for(&cfg, None, "q", &add(json!({"id": 1, "name": "a"})));
        assert_eq!(item_type(&item), QueryResultItemType::Add);
        assert_eq!(item.row_signature, 0);
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"id": 1, "name": "a"}))
        );
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
        let item = item_for(
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
        let item = item_for(&cfg, None, "q", &delete(json!({"id": 42})));
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
        let item = item_for(&cfg, Some(&engine), "q", &delete(json!({"id": "x42"})));
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
        let item = item_for(
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
            Some(query_config(None, Some(r#"{"value":"{{row.v}}"}"#), None)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = item_for(
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
        let item = item_for(
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
        let item = item_for(
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
        let item = item_for(
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
        let item = item_for(&cfg, Some(&engine), "q", &add(json!({"id": 99})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"id": 99})),
            "render failure must fall back to raw row state — never drop"
        );
    }

    #[test]
    fn empty_template_falls_back_to_raw_row() {
        let cfg = config_with(Some(query_config(Some("   "), None, None)), HashMap::new());
        let engine = TemplateEngine::new();
        let item = item_for(&cfg, Some(&engine), "q", &add(json!({"id": 1})));
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
        assert_eq!(item_for(&cfg, None, "q", &add_diff).row_signature, 11);

        let del_diff = ResultDiff::Delete {
            data: json!({}),
            row_signature: 22,
        };
        assert_eq!(item_for(&cfg, None, "q", &del_diff).row_signature, 22);

        let upd_diff = ResultDiff::Update {
            data: json!({}),
            before: json!({}),
            after: json!({}),
            grouping_keys: None,
            row_signature: 33,
        };
        assert_eq!(item_for(&cfg, None, "q", &upd_diff).row_signature, 33);

        let agg_diff = ResultDiff::Aggregation {
            before: None,
            after: json!({}),
            row_signature: 44,
        };
        assert_eq!(item_for(&cfg, None, "q", &agg_diff).row_signature, 44);
    }

    // ---- Noop contract ---------------------------------------------------

    #[test]
    fn build_proto_item_returns_none_for_noop() {
        // Noop carries no row data and produces no wire item. The function
        // returns None rather than panicking.
        let cfg = GrpcReactionConfig::default();
        let emission = TestEmission::new("q");
        assert!(build_proto_item(&cfg, None, &emission.ctx(), &ResultDiff::Noop).is_none());
    }

    // ---- sequence --------------------------------------------------------

    #[test]
    fn sequence_propagates_from_emission_onto_item() {
        let cfg = GrpcReactionConfig::default();
        let emission = TestEmission {
            sequence: 4242,
            ..TestEmission::new("q")
        };
        let item = build_proto_item(&cfg, None, &emission.ctx(), &add(json!({"id": 1})))
            .expect("add yields an item");
        assert_eq!(item.sequence, 4242);
    }

    // ---- Route resolution: exact, dotted-suffix, default -----------------

    #[test]
    fn route_lookup_resolves_exact_then_dotted_suffix_then_default() {
        let mut routes = HashMap::new();
        routes.insert(
            "q1".to_string(),
            query_config(Some(r#"{"src":"suffix-route"}"#), None, None),
        );
        routes.insert(
            "sales.q2".to_string(),
            query_config(Some(r#"{"src":"exact-route"}"#), None, None),
        );
        let cfg = config_with(
            Some(query_config(Some(r#"{"src":"default"}"#), None, None)),
            routes,
        );
        let engine = TemplateEngine::new();

        // Exact route key wins.
        let item = item_for(&cfg, Some(&engine), "sales.q2", &add(json!({})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"src": "exact-route"}))
        );

        // Dotted wire id falls back to the last-segment route key `q1`.
        let item = item_for(&cfg, Some(&engine), "sales.q1", &add(json!({})));
        assert_eq!(
            struct_field(&item.after),
            Some(json!({"src": "suffix-route"})),
            "wire id `sales.q1` must resolve to route key `q1` via the last-segment fallback"
        );

        // No matching route falls through to the default template.
        let item = item_for(&cfg, Some(&engine), "orders.q9", &add(json!({})));
        assert_eq!(struct_field(&item.after), Some(json!({"src": "default"})));
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
