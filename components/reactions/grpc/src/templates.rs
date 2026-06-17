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
//! The gRPC reaction keeps a fixed protobuf envelope for transport-native
//! consumers (`item_type`, `before`, `after`, `sequence`, etc.) and exposes the
//! guide-aligned body template output in `QueryResultItem.payload`. When no
//! template applies, `payload` carries the canonical default item whenever
//! `outputFormat = "canonicalJson"` (the default). `outputFormat = "proto"`
//! omits the payload unless a body template is configured.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext};
use log::warn;
use serde_json::{json, Map, Value};

use crate::config::{GrpcQueryConfig, GrpcReactionConfig, GrpcTemplateExtension, OutputFormat};
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::drasi_v1::QueryResultItemType;
use crate::proto::ProtoQueryResultItem;

/// A protobuf item plus per-item request metadata rendered from the selected
/// template extension. Runners merge this metadata with top-level configured
/// metadata before sending the batch.
#[derive(Debug, Clone)]
pub(crate) struct BuiltProtoItem {
    pub item: ProtoQueryResultItem,
    pub request_metadata: HashMap<String, String>,
}

/// Cached Handlebars renderer.
pub(crate) struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl TemplateEngine {
    pub(crate) fn new() -> Self {
        let mut handlebars = Handlebars::new();
        // Treat missing fields as render errors so we can fall back to the
        // canonical item instead of silently emitting empty strings.
        handlebars.set_strict_mode(true);
        register_json_helper(&mut handlebars);
        Self { handlebars }
    }

    fn render_json_body(
        &self,
        item: &ItemContext<'_>,
        spec: &TemplateSpec<GrpcTemplateExtension>,
    ) -> Option<Value> {
        let context = build_template_context(item);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
                Ok(value) => Some(value),
                Err(parse_err) => {
                    warn!(
                        "Template for query '{query_id}' ({op}) rendered non-JSON output: \
                         {parse_err}; falling back to canonical payload",
                        query_id = item.emission.query_id,
                        op = op_as_str(item.operation),
                    );
                    None
                }
            },
            Err(render_err) => {
                warn!(
                    "Template render failed for query '{query_id}' ({op}): {render_err}; \
                     falling back to canonical payload",
                    query_id = item.emission.query_id,
                    op = op_as_str(item.operation),
                );
                None
            }
        }
    }

    fn render_extension_metadata(
        &self,
        item: &ItemContext<'_>,
        spec: &TemplateSpec<GrpcTemplateExtension>,
    ) -> HashMap<String, String> {
        let context = build_template_context(item);
        let mut rendered = HashMap::new();
        for (key, value_template) in &spec.extension.metadata {
            match self.handlebars.render_template(value_template, &context) {
                Ok(value) => {
                    rendered.insert(key.clone(), value);
                }
                Err(render_err) => {
                    warn!(
                        "Metadata template '{key}' failed for query '{query_id}' ({op}): \
                         {render_err}; omitting rendered metadata entry",
                        query_id = item.emission.query_id,
                        op = op_as_str(item.operation),
                    );
                }
            }
        }
        rendered
    }
}

fn op_as_str(op: OperationType) -> &'static str {
    match op {
        OperationType::Add => "ADD",
        OperationType::Update => "UPDATE",
        OperationType::Delete => "DELETE",
    }
}

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

/// Query-level context assembled once per dequeued `QueryResult` and shared
/// across every item rendered from it.
pub(crate) struct QueryEmissionContext<'a> {
    pub query_id: &'a str,
    pub sequence: u64,
    pub timestamp: DateTime<Utc>,
    pub metadata: &'a HashMap<String, Value>,
}

struct ItemContext<'a> {
    emission: &'a QueryEmissionContext<'a>,
    operation: OperationType,
    row_signature: u64,
    before: Option<&'a Value>,
    after: Option<&'a Value>,
    data: Option<&'a Value>,
}

fn build_template_context(item: &ItemContext<'_>) -> Map<String, Value> {
    let mut ctx = Map::new();
    ctx.insert("query_id".into(), json!(item.emission.query_id));
    ctx.insert("query_name".into(), json!(item.emission.query_id));
    ctx.insert("operation".into(), json!(op_as_str(item.operation)));
    ctx.insert(
        "timestamp".into(),
        json!(item.emission.timestamp.to_rfc3339()),
    );
    ctx.insert("sequence_id".into(), json!(item.emission.sequence));
    ctx.insert("row_signature".into(), json!(item.row_signature));
    ctx.insert("metadata".into(), metadata_value(item.emission.metadata));
    if let Some(before) = item.before {
        ctx.insert("before".into(), before.clone());
    }
    if let Some(after) = item.after {
        ctx.insert("after".into(), after.clone());
    }
    if let Some(data) = item.data {
        ctx.insert("data".into(), data.clone());
    }
    let row = match item.operation {
        OperationType::Add | OperationType::Update => item.after,
        OperationType::Delete => item.before,
    };
    if let Some(row) = row {
        ctx.insert("row".into(), row.clone());
    }
    ctx
}

fn resolve_spec<'a>(
    cfg: &'a GrpcReactionConfig,
    query_id: &str,
    op: OperationType,
) -> Option<&'a TemplateSpec<GrpcTemplateExtension>> {
    let routes = cfg.routes();
    if let Some(spec) = routes.get(query_id).and_then(|qc| spec_for_op(qc, op)) {
        return Some(spec);
    }
    if let Some(segment) = query_id.rsplit('.').next() {
        if segment != query_id {
            if let Some(spec) = routes.get(segment).and_then(|qc| spec_for_op(qc, op)) {
                return Some(spec);
            }
        }
    }
    cfg.default_template().and_then(|qc| spec_for_op(qc, op))
}

fn spec_for_op(
    qc: &GrpcQueryConfig,
    op: OperationType,
) -> Option<&TemplateSpec<GrpcTemplateExtension>> {
    match op {
        OperationType::Add => qc.added.as_ref(),
        OperationType::Update => qc.updated.as_ref(),
        OperationType::Delete => qc.deleted.as_ref(),
    }
}

/// Compile-check a single Handlebars template string. Empty / whitespace
/// templates are valid and mean "use the default payload for this field".
pub(crate) fn validate_template(template: &str) -> anyhow::Result<()> {
    if template.trim().is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(template)
        .map_err(|e| anyhow::anyhow!("invalid Handlebars template: {e}"))?;
    Ok(())
}

fn metadata_value(metadata: &HashMap<String, Value>) -> Value {
    Value::Object(metadata.clone().into_iter().collect())
}

fn metadata_struct(metadata: &HashMap<String, Value>) -> Option<prost_types::Struct> {
    if metadata.is_empty() {
        None
    } else {
        Some(convert_json_to_proto_struct(&metadata_value(metadata)))
    }
}

fn timestamp_to_proto(timestamp: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: timestamp.timestamp(),
        nanos: timestamp.timestamp_subsec_nanos() as i32,
    }
}

fn canonical_payload(item: &ItemContext<'_>) -> Value {
    let mut payload = Map::new();
    payload.insert("queryId".into(), json!(item.emission.query_id));
    payload.insert("sequenceId".into(), json!(item.emission.sequence));
    payload.insert(
        "timestamp".into(),
        json!(item.emission.timestamp.to_rfc3339()),
    );
    payload.insert("operation".into(), json!(op_as_str(item.operation)));
    payload.insert("rowSignature".into(), json!(item.row_signature));
    if let Some(before) = item.before {
        payload.insert("before".into(), before.clone());
    }
    if let Some(after) = item.after {
        payload.insert("after".into(), after.clone());
    }
    if !item.emission.metadata.is_empty() {
        payload.insert("metadata".into(), metadata_value(item.emission.metadata));
    }
    Value::Object(payload)
}

fn build_payload(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    item: &ItemContext<'_>,
    spec: Option<&TemplateSpec<GrpcTemplateExtension>>,
) -> Option<prost_types::Struct> {
    let canonical = canonical_payload(item);
    let rendered = match (engine, spec) {
        (Some(engine), Some(spec)) if !spec.template.trim().is_empty() => Some(
            engine
                .render_json_body(item, spec)
                .unwrap_or_else(|| canonical.clone()),
        ),
        _ => None,
    };

    let payload = rendered
        .or_else(|| (cfg.output_format == OutputFormat::CanonicalJson).then_some(canonical))?;
    Some(convert_json_to_proto_struct(&payload))
}

/// Build the proto item emitted for a single `ResultDiff`, or `None` for
/// `ResultDiff::Noop` (which carries no row data and is never emitted).
pub(crate) fn build_proto_item(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    emission: &QueryEmissionContext<'_>,
    diff: &ResultDiff,
) -> Option<BuiltProtoItem> {
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
            QueryResultItemType::Update,
            OperationType::Update,
            *row_signature,
            before.clone(),
            Some(after.clone()),
            None,
        ),
        ResultDiff::Noop => return None,
    };

    let item_context = ItemContext {
        emission,
        operation: op,
        row_signature,
        before: raw_before.as_ref(),
        after: raw_after.as_ref(),
        data: raw_data.as_ref(),
    };
    let spec = resolve_spec(cfg, emission.query_id, op);
    let request_metadata = match (engine, spec) {
        (Some(engine), Some(spec)) => engine.render_extension_metadata(&item_context, spec),
        _ => HashMap::new(),
    };

    Some(BuiltProtoItem {
        item: ProtoQueryResultItem {
            item_type: item_type as i32,
            row_signature,
            before: raw_before.as_ref().map(convert_json_to_proto_struct),
            after: raw_after.as_ref().map(convert_json_to_proto_struct),
            sequence: emission.sequence,
            timestamp: Some(timestamp_to_proto(emission.timestamp)),
            metadata: metadata_struct(emission.metadata),
            payload: build_payload(cfg, engine, &item_context, spec),
        },
        request_metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GrpcTemplateExtension, OutputTemplates};
    use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
    use serde_json::json;

    fn add(data: Value) -> ResultDiff {
        ResultDiff::Add {
            data,
            row_signature: 11,
        }
    }

    fn delete(data: Value) -> ResultDiff {
        ResultDiff::Delete {
            data,
            row_signature: 22,
        }
    }

    fn update(before: Value, after: Value) -> ResultDiff {
        ResultDiff::Update {
            data: json!({"raw": true}),
            before,
            after,
            grouping_keys: None,
            row_signature: 33,
        }
    }

    fn aggregation(before: Option<Value>, after: Value) -> ResultDiff {
        ResultDiff::Aggregation {
            before,
            after,
            row_signature: 44,
        }
    }

    fn query_config(
        added: Option<TemplateSpec<GrpcTemplateExtension>>,
        updated: Option<TemplateSpec<GrpcTemplateExtension>>,
        deleted: Option<TemplateSpec<GrpcTemplateExtension>>,
    ) -> QueryConfig<GrpcTemplateExtension> {
        QueryConfig {
            added,
            updated,
            deleted,
        }
    }

    fn spec(template: &str) -> TemplateSpec<GrpcTemplateExtension> {
        TemplateSpec::with_extension(template, GrpcTemplateExtension::default())
    }

    fn spec_with_metadata(
        template: &str,
        metadata: HashMap<String, String>,
    ) -> TemplateSpec<GrpcTemplateExtension> {
        TemplateSpec::with_extension(template, GrpcTemplateExtension { metadata })
    }

    fn config_with(
        default: Option<QueryConfig<GrpcTemplateExtension>>,
        routes: HashMap<String, QueryConfig<GrpcTemplateExtension>>,
    ) -> GrpcReactionConfig {
        GrpcReactionConfig {
            output_templates: Some(OutputTemplates {
                default_template: default,
                routes,
            }),
            ..GrpcReactionConfig::default()
        }
    }

    struct TestEmission {
        query_id: String,
        timestamp: DateTime<Utc>,
        metadata: HashMap<String, Value>,
        sequence: u64,
    }

    impl TestEmission {
        fn new(query_id: &str) -> Self {
            Self {
                query_id: query_id.to_string(),
                timestamp: DateTime::parse_from_rfc3339("2026-06-16T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                metadata: HashMap::new(),
                sequence: 4242,
            }
        }

        fn ctx(&self) -> QueryEmissionContext<'_> {
            QueryEmissionContext {
                query_id: &self.query_id,
                sequence: self.sequence,
                timestamp: self.timestamp,
                metadata: &self.metadata,
            }
        }
    }

    fn item_for(
        cfg: &GrpcReactionConfig,
        engine: Option<&TemplateEngine>,
        query_id: &str,
        diff: &ResultDiff,
    ) -> BuiltProtoItem {
        let emission = TestEmission::new(query_id);
        build_proto_item(cfg, engine, &emission.ctx(), diff).expect("non-noop diff yields an item")
    }

    fn struct_field(item_field: &Option<prost_types::Struct>) -> Option<Value> {
        item_field.as_ref().map(struct_to_json)
    }

    fn struct_to_json(s: &prost_types::Struct) -> Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &s.fields {
            map.insert(k.clone(), value_to_json(v));
        }
        Value::Object(map)
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
            Some(Kind::StructValue(s)) => struct_to_json(s),
        }
    }

    #[test]
    fn canonical_payload_is_emitted_by_default() {
        let cfg = GrpcReactionConfig::default();
        let built = item_for(&cfg, None, "q", &add(json!({"id": 1})));
        assert_eq!(built.item.sequence, 4242);
        assert!(built.item.timestamp.is_some());
        assert_eq!(struct_field(&built.item.after), Some(json!({"id": 1})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({
                "queryId": "q",
                "sequenceId": 4242,
                "timestamp": "2026-06-16T00:00:00+00:00",
                "operation": "ADD",
                "rowSignature": 11,
                "after": {"id": 1}
            }))
        );
    }

    #[test]
    fn proto_output_omits_payload_without_template() {
        let cfg = GrpcReactionConfig {
            output_format: OutputFormat::Proto,
            ..GrpcReactionConfig::default()
        };
        let built = item_for(&cfg, None, "q", &add(json!({"id": 1})));
        assert!(built.item.payload.is_none());
        assert_eq!(struct_field(&built.item.after), Some(json!({"id": 1})));
    }

    #[test]
    fn body_template_renders_into_payload_and_leaves_typed_fields_raw() {
        let cfg = config_with(
            Some(query_config(
                Some(spec(r#"{"id":"{{after.id}}","op":"{{operation}}"}"#)),
                None,
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let built = item_for(&cfg, Some(&engine), "q", &add(json!({"id": "abc"})));
        assert_eq!(struct_field(&built.item.after), Some(json!({"id": "abc"})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({"id": "abc", "op": "ADD"}))
        );
    }

    #[test]
    fn render_failure_falls_back_to_canonical_payload() {
        let cfg = config_with(
            Some(query_config(
                Some(spec("plain text {{after.id}}")),
                None,
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let built = item_for(&cfg, Some(&engine), "q", &add(json!({"id": 99})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({
                "queryId": "q",
                "sequenceId": 4242,
                "timestamp": "2026-06-16T00:00:00+00:00",
                "operation": "ADD",
                "rowSignature": 11,
                "after": {"id": 99}
            }))
        );
    }

    #[test]
    fn query_metadata_is_preserved_on_item_and_payload() {
        let cfg = GrpcReactionConfig::default();
        let mut emission = TestEmission::new("q");
        emission.metadata.insert("tenant".into(), json!("acme"));
        let built = build_proto_item(&cfg, None, &emission.ctx(), &add(json!({"id": 1})))
            .expect("add yields item");
        assert_eq!(
            struct_field(&built.item.metadata),
            Some(json!({"tenant": "acme"}))
        );
        assert_eq!(
            struct_field(&built.item.payload).and_then(|v| v.get("metadata").cloned()),
            Some(json!({"tenant": "acme"}))
        );
    }

    #[test]
    fn extension_metadata_is_rendered_from_standard_context() {
        let mut metadata = HashMap::new();
        metadata.insert("x-query".into(), "{{query_id}}".into());
        metadata.insert("x-op".into(), "{{operation}}".into());
        let cfg = config_with(
            Some(query_config(
                Some(spec_with_metadata(r#"{"id":"{{after.id}}"}"#, metadata)),
                None,
                None,
            )),
            HashMap::new(),
        );
        let engine = TemplateEngine::new();
        let built = item_for(&cfg, Some(&engine), "orders", &add(json!({"id": 1})));
        assert_eq!(
            built.request_metadata.get("x-query").map(String::as_str),
            Some("orders")
        );
        assert_eq!(
            built.request_metadata.get("x-op").map(String::as_str),
            Some("ADD")
        );
    }

    #[test]
    fn route_lookup_resolves_exact_then_dotted_suffix_then_default() {
        let mut routes = HashMap::new();
        routes.insert(
            "q1".to_string(),
            query_config(Some(spec(r#"{"src":"suffix"}"#)), None, None),
        );
        routes.insert(
            "sales.q2".to_string(),
            query_config(Some(spec(r#"{"src":"exact"}"#)), None, None),
        );
        let cfg = config_with(
            Some(query_config(Some(spec(r#"{"src":"default"}"#)), None, None)),
            routes,
        );
        let engine = TemplateEngine::new();

        let built = item_for(&cfg, Some(&engine), "sales.q2", &add(json!({})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({"src": "exact"}))
        );

        let built = item_for(&cfg, Some(&engine), "sales.q1", &add(json!({})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({"src": "suffix"}))
        );

        let built = item_for(&cfg, Some(&engine), "orders.q9", &add(json!({})));
        assert_eq!(
            struct_field(&built.item.payload),
            Some(json!({"src": "default"}))
        );
    }

    #[test]
    fn update_delete_aggregation_and_noop_follow_contract() {
        let cfg = GrpcReactionConfig::default();
        let update = item_for(
            &cfg,
            None,
            "q",
            &update(json!({"v":"old"}), json!({"v":"new"})),
        );
        assert_eq!(struct_field(&update.item.before), Some(json!({"v": "old"})));
        assert_eq!(struct_field(&update.item.after), Some(json!({"v": "new"})));

        let delete = item_for(&cfg, None, "q", &delete(json!({"id": 7})));
        assert!(delete.item.after.is_none());
        assert_eq!(struct_field(&delete.item.before), Some(json!({"id": 7})));

        let aggregation = item_for(&cfg, None, "q", &aggregation(None, json!({"sum": 5})));
        assert_eq!(
            aggregation.item.item_type,
            QueryResultItemType::Update as i32
        );
        assert!(aggregation.item.before.is_none());
        assert_eq!(
            struct_field(&aggregation.item.after),
            Some(json!({"sum": 5}))
        );

        let emission = TestEmission::new("q");
        assert!(build_proto_item(&cfg, None, &emission.ctx(), &ResultDiff::Noop).is_none());
    }
}
