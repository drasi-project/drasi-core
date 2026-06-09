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
//! When a [`TemplateSpec`] is registered for a `(query_id, OperationType)`
//! tuple, the rendered JSON string is parsed into a `serde_json::Value` and
//! populates the optional [`ProtoQueryResultItem::payload`] field. The
//! `before` / `after` fields carry the raw row state where applicable
//! (`after` for Add, `before` for Delete, both for Update) — only
//! `payload` is template-rendered and it is omitted entirely when no
//! template is configured or when render fails (events are never dropped).

use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, TemplateRouting, TemplateSpec};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext};
use log::warn;
use serde_json::{json, Map, Value};

use crate::config::GrpcReactionConfig;
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::drasi_v1::QueryResultItemType;
use crate::proto::ProtoQueryResultItem;

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

    /// Render the template against the diff into a JSON value suitable for
    /// `ProtoQueryResultItem.payload`.
    ///
    /// Returns `None` on render failure or when the rendered string is not
    /// valid JSON; a warning is logged. Callers omit `payload` in that case
    /// — receivers can still recover the change from `before` / `after`.
    pub(crate) fn render_payload(
        &self,
        query_id: &str,
        spec: &TemplateSpec,
        diff: &ResultDiff,
    ) -> Option<Value> {
        let context = build_context(query_id, diff);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
                Ok(value) => Some(value),
                Err(parse_err) => {
                    warn!(
                        "Template for query '{query_id}' rendered non-JSON output: {parse_err}; \
                         omitting payload — receivers can read before/after for the raw change"
                    );
                    None
                }
            },
            Err(render_err) => {
                warn!(
                    "Template render failed for query '{query_id}': {render_err}; \
                     omitting payload — receivers can read before/after for the raw change"
                );
                None
            }
        }
    }
}

/// Register the `json` Handlebars helper so templates can serialize a nested
/// object/array to a JSON string (e.g. `{{json after}}`). Mirrors the helper
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

/// Build the Handlebars context map for a single `ResultDiff`.
///
/// The shape (`before` / `after` / `data` / `operation` / `query_id`)
/// matches the SSE and HTTP reactions, so existing template authors can
/// reuse familiar placeholders. `operation` uses the canonical uppercase
/// values (`ADD` / `UPDATE` / `DELETE` / `AGGREGATION` / `NOOP`) shared
/// across reactions. `data` is included for backwards-compatible template
/// authoring even though the v3 proto envelope no longer carries it.
fn build_context(query_id: &str, diff: &ResultDiff) -> Map<String, Value> {
    let mut ctx = Map::new();
    ctx.insert("query_id".into(), json!(query_id));
    match diff {
        ResultDiff::Add { data, .. } => {
            ctx.insert("operation".into(), json!("ADD"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("after".into(), data.clone());
        }
        ResultDiff::Delete { data, .. } => {
            ctx.insert("operation".into(), json!("DELETE"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("before".into(), data.clone());
        }
        ResultDiff::Update {
            data,
            before,
            after,
            ..
        } => {
            ctx.insert("operation".into(), json!("UPDATE"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("before".into(), before.clone());
            ctx.insert("after".into(), after.clone());
        }
        ResultDiff::Aggregation { before, after, .. } => {
            ctx.insert("operation".into(), json!("AGGREGATION"));
            if let Some(b) = before {
                ctx.insert("before".into(), b.clone());
            }
            ctx.insert("after".into(), after.clone());
        }
        ResultDiff::Noop => {
            ctx.insert("operation".into(), json!("NOOP"));
        }
    }
    ctx
}

/// Map a `ResultDiff` to its canonical [`OperationType`] for template
/// lookup. Returns `None` for diffs that do not map to a CRUD operation
/// (Noop, Aggregation) — those bypass template rendering.
pub(crate) fn diff_to_op(diff: &ResultDiff) -> Option<OperationType> {
    match diff {
        ResultDiff::Add { .. } => Some(OperationType::Add),
        ResultDiff::Update { .. } => Some(OperationType::Update),
        ResultDiff::Delete { .. } => Some(OperationType::Delete),
        ResultDiff::Aggregation { .. } | ResultDiff::Noop => None,
    }
}

/// Discriminator emitted on `ProtoQueryResultItem.item_type` for each
/// diff variant. The v3 schema uses a proto enum, eliminating the prior
/// string-casing inconsistency between CRUD ops and aggregation/noop.
fn item_type_for(diff: &ResultDiff) -> QueryResultItemType {
    match diff {
        ResultDiff::Add { .. } => QueryResultItemType::Add,
        ResultDiff::Update { .. } => QueryResultItemType::Update,
        ResultDiff::Delete { .. } => QueryResultItemType::Delete,
        ResultDiff::Aggregation { .. } => QueryResultItemType::Aggregation,
        ResultDiff::Noop => QueryResultItemType::Noop,
    }
}

/// Row identity carried by the diff. Surfaced on every item via the v3
/// `row_signature` field so receivers can correlate/de-duplicate without
/// parsing the payload.
fn row_signature_for(diff: &ResultDiff) -> u64 {
    match diff {
        ResultDiff::Add { row_signature, .. }
        | ResultDiff::Delete { row_signature, .. }
        | ResultDiff::Update { row_signature, .. }
        | ResultDiff::Aggregation { row_signature, .. } => *row_signature,
        ResultDiff::Noop => 0,
    }
}

/// Raw `(before, after)` row state for a diff, as emitted on the v3
/// envelope's framework-controlled fields.
fn before_after_for(diff: &ResultDiff) -> (Option<Value>, Option<Value>) {
    match diff {
        ResultDiff::Add { data, .. } => (None, Some(data.clone())),
        ResultDiff::Delete { data, .. } => (Some(data.clone()), None),
        ResultDiff::Update { before, after, .. } => (Some(before.clone()), Some(after.clone())),
        ResultDiff::Aggregation { before, after, .. } => (before.clone(), Some(after.clone())),
        ResultDiff::Noop => (None, None),
    }
}

/// Build the proto item emitted for a single `ResultDiff` under the v3
/// schema.
///
/// `before` / `after` carry the raw row state where applicable (`after`
/// for Add, `before` for Delete, both for Update; both for Aggregation
/// where `before` is known). `payload` is populated only when an output
/// template is configured for the `(query_id, op)` pair and renders to
/// valid JSON; it is omitted on render failure, empty templates, missing
/// template, or for AGGREGATION / NOOP. Events are never dropped — a
/// receiver can always recover the change from `before` / `after`.
pub(crate) fn build_proto_item(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    query_id: &str,
    diff: &ResultDiff,
) -> ProtoQueryResultItem {
    let (before, after) = before_after_for(diff);

    let payload = engine
        .zip(diff_to_op(diff))
        .and_then(|(eng, op)| cfg.get_template_spec(query_id, op).map(|spec| (eng, spec)))
        .and_then(|(eng, spec)| {
            if spec.template.trim().is_empty() {
                None
            } else {
                eng.render_payload(query_id, spec, diff)
            }
        })
        .map(|value| convert_json_to_proto_struct(&value));

    ProtoQueryResultItem {
        before: before.as_ref().map(convert_json_to_proto_struct),
        after: after.as_ref().map(convert_json_to_proto_struct),
        item_type: item_type_for(diff) as i32,
        row_signature: row_signature_for(diff),
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputTemplates;
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

    fn query_config_add(template: &str) -> QueryConfig {
        QueryConfig {
            added: Some(TemplateSpec::new(template)),
            updated: None,
            deleted: None,
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

    #[test]
    fn diff_to_op_maps_crud_and_skips_aggregation_noop() {
        assert_eq!(diff_to_op(&add(json!({}))), Some(OperationType::Add));
        assert_eq!(
            diff_to_op(&update(json!({}), json!({}))),
            Some(OperationType::Update)
        );
        assert_eq!(diff_to_op(&delete(json!({}))), Some(OperationType::Delete));
        assert_eq!(diff_to_op(&ResultDiff::Noop), None);
        assert_eq!(
            diff_to_op(&ResultDiff::Aggregation {
                before: None,
                after: json!({}),
                row_signature: 0
            }),
            None
        );
    }

    #[test]
    fn build_context_uses_uppercase_operation_and_injects_query_id() {
        let ctx = build_context("orders", &add(json!({"id": 1})));
        assert_eq!(ctx.get("operation").unwrap(), "ADD");
        assert_eq!(ctx.get("query_id").unwrap(), "orders");
        assert_eq!(ctx.get("after").unwrap(), &json!({"id": 1}));
        assert!(ctx.get("before").is_none());

        let ctx = build_context("orders", &delete(json!({"id": 2})));
        assert_eq!(ctx.get("operation").unwrap(), "DELETE");
        assert_eq!(ctx.get("before").unwrap(), &json!({"id": 2}));
        assert!(ctx.get("after").is_none());

        let ctx = build_context(
            "orders",
            &update(json!({"id": 3, "v": "old"}), json!({"id": 3, "v": "new"})),
        );
        assert_eq!(ctx.get("operation").unwrap(), "UPDATE");
        assert_eq!(ctx.get("before").unwrap(), &json!({"id": 3, "v": "old"}));
        assert_eq!(ctx.get("after").unwrap(), &json!({"id": 3, "v": "new"}));

        let ctx = build_context("orders", &ResultDiff::Noop);
        assert_eq!(ctx.get("operation").unwrap(), "NOOP");
    }

    #[test]
    fn render_payload_returns_some_for_valid_json_template() {
        let engine = TemplateEngine::new();
        let spec = TemplateSpec::new(r#"{"snapshot":{{json after}},"q":"{{query_id}}"}"#);
        let out = engine
            .render_payload("orders", &spec, &add(json!({"id": 7, "name": "x"})))
            .expect("template renders to valid JSON");
        assert_eq!(
            out,
            json!({"snapshot": {"id": 7, "name": "x"}, "q": "orders"})
        );
    }

    #[test]
    fn render_payload_returns_none_on_non_json_output() {
        let engine = TemplateEngine::new();
        let spec = TemplateSpec::new("this is not json {{after.id}}");
        let out = engine.render_payload("q", &spec, &add(json!({"id": 7})));
        assert!(
            out.is_none(),
            "non-JSON render must omit payload so receivers fall back to before/after"
        );
    }

    fn item_type(item: &ProtoQueryResultItem) -> QueryResultItemType {
        QueryResultItemType::try_from(item.item_type).expect("valid enum tag")
    }

    #[test]
    fn build_proto_item_add_populates_after_only_with_item_type_add() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &add(json!({"id": 1})));
        assert_eq!(item_type(&item), QueryResultItemType::Add);
        assert!(item.after.is_some(), "ADD must populate `after`");
        assert!(
            item.before.is_none(),
            "ADD must leave `before` absent — receivers use after-only"
        );
        assert!(
            item.payload.is_none(),
            "no template configured ⇒ payload absent"
        );
    }

    #[test]
    fn build_proto_item_delete_populates_before_only_with_item_type_delete() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &delete(json!({"id": 1})));
        assert_eq!(item_type(&item), QueryResultItemType::Delete);
        assert!(item.before.is_some(), "DELETE must populate `before`");
        assert!(
            item.after.is_none(),
            "DELETE must leave `after` absent — receivers use before-only"
        );
        assert!(item.payload.is_none());
    }

    #[test]
    fn build_proto_item_update_populates_both_with_item_type_update() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &update(json!({"id": 1}), json!({"id": 2})));
        assert_eq!(item_type(&item), QueryResultItemType::Update);
        assert!(item.before.is_some());
        assert!(item.after.is_some());
        assert!(item.payload.is_none());
    }

    #[test]
    fn build_proto_item_aggregation_populates_after_and_optional_before() {
        let cfg = GrpcReactionConfig::default();
        let diff = ResultDiff::Aggregation {
            before: Some(json!({"prev": 1})),
            after: json!({"curr": 2}),
            row_signature: 99,
        };
        let item = build_proto_item(&cfg, None, "q", &diff);
        assert_eq!(item_type(&item), QueryResultItemType::Aggregation);
        assert!(item.before.is_some());
        assert!(item.after.is_some());
        assert_eq!(item.row_signature, 99);

        let diff_no_before = ResultDiff::Aggregation {
            before: None,
            after: json!({"curr": 3}),
            row_signature: 5,
        };
        let item = build_proto_item(&cfg, None, "q", &diff_no_before);
        assert_eq!(item_type(&item), QueryResultItemType::Aggregation);
        assert!(item.before.is_none());
        assert!(item.after.is_some());
    }

    #[test]
    fn build_proto_item_noop_emits_no_before_no_after_no_payload() {
        let cfg = GrpcReactionConfig::default();
        let item = build_proto_item(&cfg, None, "q", &ResultDiff::Noop);
        assert_eq!(item_type(&item), QueryResultItemType::Noop);
        assert!(item.before.is_none());
        assert!(item.after.is_none());
        assert!(item.payload.is_none());
        assert_eq!(item.row_signature, 0);
    }

    #[test]
    fn build_proto_item_propagates_row_signature_for_every_variant() {
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

        assert_eq!(
            build_proto_item(&cfg, None, "q", &ResultDiff::Noop).row_signature,
            0
        );
    }

    #[test]
    fn build_proto_item_populates_payload_when_template_renders_successfully() {
        let cfg = config_with(
            Some(query_config_add(r#"{"hello":"{{after.id}}"}"#)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &add(json!({"id": "abc"})));
        let payload = item.payload.as_ref().expect("payload must be present");
        let hello = payload
            .fields
            .get("hello")
            .and_then(|v| match &v.kind {
                Some(prost_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .unwrap();
        assert_eq!(hello, "abc");
        // before/after still carry the raw row state.
        assert!(item.after.is_some());
    }

    #[test]
    fn build_proto_item_omits_payload_when_template_render_fails_but_keeps_before_after() {
        // Non-JSON output triggers the parse-failure warn-and-omit path.
        let cfg = config_with(
            Some(query_config_add("plain text {{after.id}}")),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &add(json!({"id": 7})));
        assert!(
            item.payload.is_none(),
            "render failure must omit payload so receivers fall back to before/after"
        );
        assert!(
            item.after.is_some(),
            "render failure must not drop the raw `after` row state"
        );
    }

    #[test]
    fn build_proto_item_omits_payload_when_template_string_is_empty() {
        let cfg = config_with(Some(query_config_add("   ")), HashMap::new());
        let engine = TemplateEngine::new();
        let item = build_proto_item(&cfg, Some(&engine), "q", &add(json!({"id": 1})));
        assert!(
            item.payload.is_none(),
            "empty template must omit payload — never falls back to a raw-data payload"
        );
        assert!(item.after.is_some());
    }

    #[test]
    fn build_proto_item_omits_payload_for_aggregation_even_with_template_configured() {
        // Aggregation bypasses template rendering: diff_to_op returns None,
        // so the (engine, op) zip is None and payload is omitted regardless
        // of whether routes/default would otherwise apply.
        let cfg = config_with(
            Some(query_config_add(r#"{"should":"never render"}"#)),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let diff = ResultDiff::Aggregation {
            before: None,
            after: json!({"k": 1}),
            row_signature: 0,
        };
        let item = build_proto_item(&cfg, Some(&engine), "q", &diff);
        assert!(item.payload.is_none());
    }

    #[test]
    fn route_lookup_requires_exact_query_id_no_dotted_suffix_fallback() {
        // Regression guard for HTTP commit ca0b826's strict-routing rule:
        // a route key with a dotted suffix must NOT be selected for a query
        // id that only matches the suffix. The shared TemplateRouting trait
        // (drasi_lib::reactions::common) is what we delegate to; this test
        // pins the exact-id behavior so a future change in the shared trait
        // cannot silently widen routing.
        let mut routes = HashMap::new();
        routes.insert(
            "sales.q1".to_string(),
            query_config_add(r#"{"src":"namespaced-route"}"#),
        );
        let cfg = config_with(Some(query_config_add(r#"{"src":"default"}"#)), routes);

        // Query id "q1" must fall through to the default template, NOT
        // resolve to the "sales.q1" entry by stripping the "sales." prefix.
        let engine = TemplateEngine::new();
        let spec = cfg.get_template_spec("q1", OperationType::Add).unwrap();
        assert_eq!(
            engine.render_payload("q1", spec, &add(json!({}))).unwrap(),
            json!({"src": "default"}),
            "query id `q1` must not match route key `sales.q1` via suffix fallback"
        );

        // Conversely, the exact route key resolves to the namespaced spec.
        let spec = cfg
            .get_template_spec("sales.q1", OperationType::Add)
            .unwrap();
        assert_eq!(
            engine
                .render_payload("sales.q1", spec, &add(json!({})))
                .unwrap(),
            json!({"src": "namespaced-route"})
        );
    }
}
