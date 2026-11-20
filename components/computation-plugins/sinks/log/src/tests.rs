// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::component::LogSink;
use crate::config::{LogFormat, LogLevel, LogOutput};
use crate::output::{write_lines, Emitter, StreamWriter};
use crate::render::{format_plain, resolve_routes};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentId, InputBinding, InputEnvelope, OutputEndpoint, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{LifecycleErrorKind, StopMode, UpdateBoundary};
use drasi_computation_plugin_sdk::testing::{contexts, create_sink, SinkConformance, SinkProbe};
use drasi_computation_plugin_sdk::{ExportedPlugin, NativeComponent, NativeSink};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, Once};

const ID: &str = "b11e8f74-9ac7-5734-b9b8-d9fd4c356f13";
const SOURCE: &str = "5c3e49cc-32f8-5b75-888d-30b75b572221";
const OTHER: &str = "7d2a1f0e-4c3b-4a59-8e6d-1f2a3b4c5d6e";

// ---------------------------------------------------------------- test logger

/// Records every `log` record. The `log` facade has one process-wide logger,
/// so each test filters by its own unique target.
struct CaptureLogger;

static RECORDS: Mutex<Vec<(log::Level, String, String)>> = Mutex::new(Vec::new());
static LOGGER: CaptureLogger = CaptureLogger;
static INSTALL: Once = Once::new();

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if let Ok(mut records) = RECORDS.lock() {
            records.push((
                record.level(),
                record.target().to_owned(),
                record.args().to_string(),
            ));
        }
    }
    fn flush(&self) {}
}

fn install_logger() {
    INSTALL.call_once(|| {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
    });
}

fn records(target: &str) -> Vec<(log::Level, String)> {
    install_logger();
    RECORDS
        .lock()
        .map(|records| {
            records
                .iter()
                .filter(|(_, t, _)| t == target)
                .map(|(level, _, message)| (*level, message.clone()))
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ fixtures

fn stream(source: &str) -> Result<StreamId> {
    Ok(StreamId::try_new(format!("{source}/out"))?)
}

fn binding(source: &str) -> Result<InputBinding> {
    Ok(InputBinding {
        input_port: PortId::try_new("in")?,
        output: OutputEndpoint::new(ComponentId::try_new(source)?, PortId::try_new("out")?),
        stream: stream(source)?,
    })
}

fn inputs() -> Vec<InputBinding> {
    vec![binding(SOURCE).expect("fixture binding")]
}

fn two_inputs() -> Vec<InputBinding> {
    vec![
        binding(SOURCE).expect("fixture binding"),
        binding(OTHER).expect("fixture binding"),
    ]
}

/// `{"outputs": {alias: SOURCE}}` merged with `extra`.
fn aliased(alias: &str, extra: Value) -> Value {
    let mut value = json!({"outputs": {alias: {"component": SOURCE, "port": "out"}}});
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn element(id: &str, name: &str) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert("id", ElementValue::String(Arc::from(id)));
    properties.insert("name", ElementValue::String(Arc::from(name)));
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(SOURCE, id),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1,
        },
        properties,
    }
}

fn envelope(source: &str, changes: &[SourceChange], sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(changes, stream(source)?, sequence, None)?,
    })
}

fn insert(sequence: u64) -> Result<InputEnvelope> {
    envelope(
        SOURCE,
        &[SourceChange::Insert {
            element: element("one", "Alice"),
        }],
        sequence,
    )
}

fn update() -> Result<InputEnvelope> {
    envelope(SOURCE, &[SourceChange::update(element("one", "Bob"))], 2)
}

fn delete() -> Result<InputEnvelope> {
    let Element::Node { metadata, .. } = element("one", "Alice") else {
        unreachable!("fixture is a node")
    };
    envelope(SOURCE, &[SourceChange::Delete { metadata }], 3)
}

fn sink_with(configuration: Value, inputs: &[InputBinding]) -> Result<LogSink> {
    let config = LogSinkConfig::parse(configuration)?;
    let outputs = SinkOutputs::from_inputs(inputs, &config.outputs)?;
    LogSink::new(
        LogSinkFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        config,
        outputs,
    )
}

fn sink(configuration: Value) -> Result<LogSink> {
    sink_with(configuration, &inputs())
}

/// The rendered messages without the `[id] ` prefix.
fn messages(sink: &LogSink, input: &InputEnvelope) -> Result<Vec<String>> {
    let prefix = format!("[{ID}] ");
    Ok(sink
        .lines(input)?
        .into_iter()
        .map(|line| {
            line.strip_prefix(&prefix)
                .map(str::to_owned)
                .unwrap_or(line)
        })
        .collect())
}

fn templates(added: &str, updated: &str, deleted: &str) -> Value {
    json!({
        "added": {"template": added},
        "updated": {"template": updated},
        "deleted": {"template": deleted},
    })
}

// ------------------------------------------------------------- conformance

struct Probe {
    target: String,
}

impl Probe {
    fn new(target: &str) -> Self {
        install_logger();
        Self {
            target: target.into(),
        }
    }
    fn config(&self) -> Value {
        json!({"target": self.target})
    }
}

#[async_trait]
impl SinkProbe for Probe {
    fn input(&self, sequence: u64) -> Result<InputEnvelope> {
        insert(sequence)
    }
    async fn expect_delivered(&self, configuration: &Value, count: usize) -> Result<()> {
        let target = configuration["target"].as_str().context("target")?;
        let delivered = records(target).len();
        anyhow::ensure!(
            delivered >= count,
            "expected {count} deliveries to {target}, saw {delivered}"
        );
        Ok(())
    }
}

fn conformance(inputs: Vec<InputBinding>) -> SinkConformance<'static> {
    SinkConformance::new(&LogSinkFactory, ID, inputs).expect("conformance")
}

#[test]
fn metadata_is_valid() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let metadata = LogSinkFactory.metadata();
    assert_eq!(metadata.role, ComponentRole::Sink);
    assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
    let capabilities = &metadata.capabilities;
    assert!(
        capabilities.binds_inputs && capabilities.management && capabilities.erases_owned_state
    );
    assert_eq!(
        capabilities.update_modes,
        [ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate]
    );
    Ok(())
}

#[test]
fn published_defaults_match_the_parsed_defaults() -> Result<()> {
    let details = LogSinkFactory.configuration_details();
    details.validate_schema(&LogSinkFactory.metadata().configuration)?;
    let defaults = serde_json::to_value(LogSinkConfig::default())?;
    for (name, field) in &details.fields {
        if let Some(default) = &field.default {
            assert_eq!(&defaults[name], default, "default of {name}");
        }
    }
    assert_eq!(LogSinkConfig::parse(json!({}))?, LogSinkConfig::default());
    Ok(())
}

#[test]
fn published_schema_types_routes_and_templates() -> Result<()> {
    let schema = LogSinkFactory.metadata().configuration;
    schema.validate(&json!({"routes": {"a": {"added": {"template": "x"}}}}))?;
    assert!(schema
        .validate(&json!({"routes": {"a": {"inserted": {"template": "x"}}}}))
        .is_err());
    assert!(schema
        .validate(&json!({"defaultTemplate": {"added": {"text": "x"}}}))
        .is_err());
    assert!(schema.validate(&json!({"target": ""})).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_conformance_current_thread() -> Result<()> {
    let probe = Probe::new("test.lifecycle.current");
    conformance(inputs())
        .lifecycle(probe.config(), &probe)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_conformance_multi_thread() -> Result<()> {
    let probe = Probe::new("test.lifecycle.multi");
    let mut config = aliased("items", probe.config());
    config["defaultTemplate"] = templates("{{json after}}", "", "");
    config["level"] = json!("debug");
    conformance(inputs()).lifecycle(config, &probe).await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_idempotent() -> Result<()> {
    conformance(inputs()).erase(json!({})).await
}

#[tokio::test(flavor = "current_thread")]
async fn every_dispose_reason_ends_an_initialized_sink() -> Result<()> {
    conformance(inputs()).dispose_matrix(json!({})).await
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_reports_cancelled_and_stays_startable() -> Result<()> {
    let probe = Probe::new("test.cancelled");
    let mut sink = create_sink(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        probe.config(),
        &inputs(),
    )?;
    sink.initialize(&contexts::initialize()).await?;
    let error = sink
        .start(&mut contexts::cancelled_start())
        .await
        .err()
        .context("a pre-cancelled start must fail")?;
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    assert!(sink.handle(insert(1)?).await.is_err());
    sink.start(&mut contexts::start()).await?;
    sink.handle(insert(1)?).await?;
    assert_eq!(records("test.cancelled").len(), 1);
    sink.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn handle_while_paused_is_refused() -> Result<()> {
    let mut sink = create_sink(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        json!({"target": "test.paused"}),
        &inputs(),
    )?;
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    sink.pause(&contexts::pause()).await?;
    assert!(sink.handle(insert(1)?).await.is_err());
    sink.resume(&contexts::resume()).await?;
    sink.handle(insert(1)?).await?;
    assert_eq!(records("test.paused").len(), 1);
    Ok(())
}

fn full_configuration() -> Value {
    aliased(
        "east.items",
        json!({
            "routes": {"items": templates("a {{after.id}}", "u {{after.id}}", "")},
            "defaultTemplate": {"deleted": {"template": "d {{identity.id}}"}},
            "format": "compact",
            "includeHeader": true,
            "level": "warn",
            "target": "custom.target",
            "output": "stderr",
            "renderFailurePolicy": "strict",
            "onPermanentFailure": "skip",
        }),
    )
}

#[test]
fn configuration_round_trips() -> Result<()> {
    conformance(inputs()).configuration_round_trip(&[
        json!({}),
        aliased("items", json!({})),
        aliased(
            "items",
            json!({"defaultTemplate": {"added": {"template": "{{json after}}"}}}),
        ),
        full_configuration(),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = aliased("items", json!({}));
    let with = |extra: Value| aliased("items", extra);
    conformance(inputs()).assess_matrix(
        &current,
        &[
            (current.clone(), Some(UpdateBoundary::Pause)),
            (
                with(json!({"routes": {"items": {"added": {"template": "x"}}}})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(json!({"defaultTemplate": {"added": {"template": "{{json after}}"}}})),
                Some(UpdateBoundary::Pause),
            ),
            (with(json!({"level": "debug"})), Some(UpdateBoundary::Pause)),
            (
                with(json!({"target": "other"})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(json!({"output": "stdout"})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(json!({"format": "compact"})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(json!({"includeHeader": true})),
                Some(UpdateBoundary::Pause),
            ),
            (
                with(json!({"renderFailurePolicy": "strict"})),
                Some(UpdateBoundary::Pause),
            ),
            // The policy is part of the description captured at construction.
            (
                with(json!({"onPermanentFailure": "skip"})),
                Some(UpdateBoundary::Replace),
            ),
            (aliased("renamed", json!({})), Some(UpdateBoundary::Replace)),
            (json!({}), Some(UpdateBoundary::Replace)),
            // Rejected: an unknown helper, a route matching nothing, an
            // unknown field and an unsupported value.
            (
                with(json!({"defaultTemplate": {"added": {"template": "{{nope after}}"}}})),
                None,
            ),
            (
                with(json!({"routes": {"missing": {"added": {"template": "x"}}}})),
                None,
            ),
            (with(json!({"colour": "red"})), None),
            (with(json!({"level": "loud"})), None),
        ],
    )
}

#[test]
fn changed_input_bindings_require_replacement() -> Result<()> {
    let config = json!({});
    let mut request = drasi_computation_plugin_sdk::testing::update_request(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        config.clone(),
        config,
    );
    request.current.inputs = inputs();
    request.proposed.inputs = two_inputs();
    match LogSinkFactory
        .update_planner()
        .expect("factory planner")
        .assess(&request)?
    {
        Assessment::Supported { boundary, .. } => assert_eq!(boundary, UpdateBoundary::Replace),
        other => anyhow::bail!("unexpected {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_update_applies_to_a_running_sink() -> Result<()> {
    let probe = Probe::new("test.apply.before");
    let current = aliased("items", probe.config());
    let proposed = aliased(
        "items",
        json!({
            "target": "test.apply.before",
            "level": "trace",
            "defaultTemplate": {"added": {"template": "applied {{after.name}}"}},
        }),
    );
    conformance(inputs())
        .apply(current, proposed, &probe)
        .await?;
    let records = records("test.apply.before");
    assert_eq!(records[0].0, log::Level::Info);
    assert_eq!(records[1].0, log::Level::Trace);
    assert!(records[1].1.ends_with("applied Alice"), "{records:?}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn apply_refuses_a_stale_or_unpaused_update() -> Result<()> {
    use drasi_computation_plugin_sdk::ApplyUpdateRequest;
    let current = aliased("items", json!({}));
    let proposed = aliased("items", json!({"level": "debug"}));
    let mut sink = sink(current.clone())?;
    sink.initialize(&contexts::initialize()).await?;
    sink.start(&mut contexts::start()).await?;
    let mut request = drasi_computation_plugin_sdk::testing::update_request(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        current,
        proposed.clone(),
    );
    request.current.inputs = inputs();
    request.proposed.inputs = inputs();
    let assessment = NativeManagement::assess(&sink, &request)?;
    let update = ApplyUpdateRequest {
        operation_id: "op".into(),
        request: request.clone(),
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    let ctx = contexts::apply(UpdateBoundary::Pause);
    assert!(sink
        .management()
        .context("management")?
        .apply(&ctx, &update)
        .await
        .is_err());
    sink.pause(&contexts::pause()).await?;
    let mut stale = update.clone();
    stale.request.current.configuration = aliased("items", json!({"level": "warn"}));
    assert!(sink
        .management()
        .context("management")?
        .apply(&ctx, &stale)
        .await
        .is_err());
    sink.management()
        .context("management")?
        .apply(&ctx, &update)
        .await?;
    assert_eq!(sink.configuration()?["level"], "debug");
    Ok(())
}

use drasi_computation_plugin_sdk::NativeManagement;

// ------------------------------------------------- templates (legacy ports)

#[test]
fn templates_and_default_json_use_native_change_context() -> Result<()> {
    let config = aliased(
        "items",
        json!({"defaultTemplate": {"added": {"template": "{{output.alias}} {{operation}} {{identity.id}}"}}}),
    );
    assert_eq!(messages(&sink(config)?, &insert(7)?)?, ["items ADD one"]);
    let raw: Value =
        serde_json::from_str(&messages(&sink(aliased("items", json!({})))?, &insert(7)?)?[0])?;
    assert_eq!(raw["output"]["alias"], "items");
    assert_eq!(raw["operation"], "ADD");
    assert_eq!(raw["sequence"], 7);
    assert_eq!(raw["identity"]["namespace"], SOURCE);
    assert_eq!(raw["after"]["name"], "Alice");
    Ok(())
}

#[test]
fn default_template_covers_every_operation() -> Result<()> {
    let config = aliased(
        "items",
        json!({"defaultTemplate": templates(
            "[NEW] Item {{after.id}}",
            "[CHG] {{operation}} -> {{after.name}}",
            "[DEL] Item {{identity.id}}",
        )}),
    );
    let sink = sink(config)?;
    assert_eq!(messages(&sink, &insert(1)?)?, ["[NEW] Item one"]);
    assert_eq!(messages(&sink, &update()?)?, ["[CHG] PATCH -> Bob"]);
    assert_eq!(messages(&sink, &delete()?)?, ["[DEL] Item one"]);
    Ok(())
}

#[test]
fn per_output_routes_fall_back_to_the_default_per_operation() -> Result<()> {
    let config = json!({
        "outputs": {
            "sensor-query": {"component": SOURCE, "port": "out"},
            "other-query": {"component": OTHER, "port": "out"},
        },
        "routes": {"sensor-query": {
            "added": {"template": "[SENSOR] New: {{after.id}}"},
            "updated": {"template": "[SENSOR-UPD] {{after.id}}"},
        }},
        "defaultTemplate": {
            "added": {"template": "[DEFAULT] {{after.id}}"},
            "deleted": {"template": "[DEFAULT-DEL] {{identity.id}}"},
        },
    });
    let sink = sink_with(config, &two_inputs())?;
    assert_eq!(messages(&sink, &insert(1)?)?, ["[SENSOR] New: one"]);
    assert_eq!(messages(&sink, &update()?)?, ["[SENSOR-UPD] one"]);
    assert_eq!(messages(&sink, &delete()?)?, ["[DEFAULT-DEL] one"]);
    let other = envelope(
        OTHER,
        &[SourceChange::Insert {
            element: element("two", "Carol"),
        }],
        1,
    )?;
    assert_eq!(messages(&sink, &other)?, ["[DEFAULT] two"]);
    Ok(())
}

#[test]
fn invalid_template_syntax_is_rejected_at_construction() {
    for template in ["[ADD] {{after.id", "{{unclosed"] {
        let error = sink(aliased(
            "items",
            json!({"defaultTemplate": {"added": {"template": template}}}),
        ))
        .err()
        .expect("invalid template");
        assert!(
            format!("{error:#}").contains("invalid log template"),
            "{error:#}"
        );
    }
}

#[test]
fn route_without_matching_output_is_rejected() {
    let error = sink(aliased(
        "query1",
        json!({"routes": {"non-existent-query": {"added": {"template": "x"}}}}),
    ))
    .err()
    .expect("unmatched route");
    assert!(error
        .to_string()
        .contains("does not select any incoming output"));
}

#[test]
fn complex_json_templates_render_valid_json() -> Result<()> {
    let config = aliased(
        "items",
        json!({"defaultTemplate": templates(
            r#"{"event": "added", "id": "{{after.id}}", "data": {{json after}}}"#,
            r#"{"event": "updated", "before": {{#if before}}{{json before}}{{else}}null{{/if}}, "after": {{json after}}}"#,
            r#"{"event": "deleted", "id": "{{identity.id}}"}"#,
        )}),
    );
    let sink = sink(config)?;
    let added: Value = serde_json::from_str(&messages(&sink, &insert(1)?)?[0])?;
    assert_eq!(
        added,
        json!({"event": "added", "id": "one", "data": {"id": "one", "name": "Alice"}})
    );
    let updated: Value = serde_json::from_str(&messages(&sink, &update()?)?[0])?;
    assert_eq!(updated["before"], Value::Null);
    assert_eq!(updated["after"]["name"], "Bob");
    let deleted: Value = serde_json::from_str(&messages(&sink, &delete()?)?[0])?;
    assert_eq!(deleted["id"], "one");
    Ok(())
}

#[test]
fn values_are_not_html_escaped() -> Result<()> {
    let config = aliased(
        "items",
        json!({"defaultTemplate": {"added": {"template": "{{json after}} <{{after.name}}>"}}}),
    );
    let message = messages(&sink(config)?, &insert(1)?)?.remove(0);
    assert_eq!(message, r#"{"id":"one","name":"Alice"} <Alice>"#);
    Ok(())
}

#[test]
fn empty_template_uses_the_configured_format() -> Result<()> {
    let config = aliased(
        "items",
        json!({"format": "compact", "defaultTemplate": {"added": {"template": ""}}}),
    );
    assert_eq!(
        messages(&sink(config)?, &insert(1)?)?,
        [r#"[ADD] {"id":"one","name":"Alice"}"#]
    );
    Ok(())
}

#[test]
fn configuration_serialization_round_trips() -> Result<()> {
    let config = LogSinkConfig::parse(json!({
        "routes": {"test-query": {"added": {"template": "Test {{after.id}}"}}},
        "defaultTemplate": {"added": {"template": "Default {{after.id}}"}},
    }))?;
    let text = serde_json::to_string(&config)?;
    assert!(text.contains("Default {{after.id}}") && text.contains("Test {{after.id}}"));
    assert_eq!(LogSinkConfig::parse(serde_json::from_str(&text)?)?, config);
    assert!(LogSinkConfig::parse(json!({"routes": {"q": {"added": {}}}})).is_err());
    assert!(LogSinkConfig::parse(json!({"target": ""})).is_err());
    Ok(())
}

// ------------------------------------------------------------ helpers (LG-01)

#[test]
fn json_helper_renders_compact_json_and_a_missing_value_fails() -> Result<()> {
    let config = aliased(
        "items",
        json!({"defaultTemplate": {"added": {"template": "{{json after}}|{{json (lookup after \"name\")}}"}}}),
    );
    assert_eq!(
        messages(&sink(config)?, &insert(1)?)?,
        [r#"{"id":"one","name":"Alice"}|"Alice""#]
    );
    // Strict, as in every native sink: an absent image is a render failure,
    // not `null`; under `strict` the failure is permanent.
    let config = aliased(
        "items",
        json!({"defaultTemplate": {"added": {"template": "{{json before}}"}}, "renderFailurePolicy": "strict"}),
    );
    assert!(messages(&sink(config)?, &insert(1)?).is_err());
    Ok(())
}

#[test]
fn unknown_helpers_partials_and_decorators_are_rejected_at_construction() {
    for template in [
        "{{nope after}}",
        "{{#nope}}x{{/nope}}",
        "{{#if after}}ok{{else}}{{missing after}}{{/if}}",
        "{{json (nope after)}}",
        "{{#each after}}{{format this}}{{/each}}",
        "{{> partial}}",
        "{{#*inline \"p\"}}x{{/inline}}",
    ] {
        let result = sink(aliased(
            "items",
            json!({"defaultTemplate": {"added": {"template": template}}}),
        ));
        assert!(result.is_err(), "{template} must be rejected");
    }
    for template in [
        "{{#if after}}{{after.id}}{{else}}none{{/if}}",
        "{{#each after}}{{@key}}={{this}} {{/each}}",
        "{{#with after}}{{name}}{{/with}}",
        "{{#unless before}}new{{/unless}} {{len after}} {{eq operation \"ADD\"}}",
        "{{lookup after \"id\"}} {{json after}}",
    ] {
        sink(aliased(
            "items",
            json!({"defaultTemplate": {"added": {"template": template}}}),
        ))
        .unwrap_or_else(|error| panic!("{template} must be accepted: {error:#}"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn render_failure_strict_fails_and_fallback_emits_the_format() -> Result<()> {
    install_logger();
    // `lookup` with one argument passes the static check but fails to render.
    let failing = json!({"added": {"template": "{{lookup after}}"}});

    let strict = aliased(
        "items",
        json!({"defaultTemplate": failing, "renderFailurePolicy": "strict", "target": "test.strict", "includeHeader": true}),
    );
    let mut sink = create_sink(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        strict,
        &inputs(),
    )?;
    sink.start(&mut contexts::start()).await?;
    let error = sink.handle(insert(1)?).await.unwrap_err();
    assert_eq!(
        drasi_computation_contracts::delivery_failure_class(&error),
        Some(drasi_computation_contracts::DeliveryFailureClass::Permanent),
        "a strict render failure is permanent, so onPermanentFailure applies"
    );
    assert!(records("test.strict").is_empty(), "strict emits nothing");

    let fallback = aliased(
        "items",
        json!({"defaultTemplate": failing, "format": "compact", "target": "test.fallback"}),
    );
    let mut sink = create_sink(
        &LogSinkFactory,
        &ComponentId::try_new(ID)?,
        fallback,
        &inputs(),
    )?;
    sink.start(&mut contexts::start()).await?;
    sink.handle(insert(1)?).await?;
    sink.handle(insert(2)?).await?;
    let records = records("test.fallback");
    let warnings = records
        .iter()
        .filter(|(level, _)| *level == log::Level::Warn)
        .count();
    let lines: Vec<_> = records
        .iter()
        .filter(|(level, _)| *level == log::Level::Info)
        .map(|(_, message)| message.as_str())
        .collect();
    assert_eq!(warnings, 2);
    assert_eq!(
        lines,
        vec![format!(r#"[{ID}] [ADD] {{"id":"one","name":"Alice"}}"#); 2]
    );
    Ok(())
}

// ------------------------------------------------------------ routing (LG-02)

#[test]
fn dotted_suffix_routes_match_in_validation_and_lookup() -> Result<()> {
    // Reproduction: the old sink accepted this route but never applied it.
    let config = aliased(
        "source.sensor-data",
        json!({"routes": {"sensor-data": {"added": {"template": "[SENSOR] {{after.id}}"}}}}),
    );
    assert_eq!(messages(&sink(config)?, &insert(1)?)?, ["[SENSOR] one"]);
    Ok(())
}

#[test]
fn exact_alias_routes_take_precedence_over_suffix_routes() -> Result<()> {
    let config = json!({
        "outputs": {
            "east.orders": {"component": SOURCE, "port": "out"},
            "west.orders": {"component": OTHER, "port": "out"},
        },
        "routes": {
            "east.orders": {"added": {"template": "exact"}},
            "orders": {"added": {"template": "suffix"}},
        },
    });
    let sink = sink_with(config, &two_inputs())?;
    assert_eq!(messages(&sink, &insert(1)?)?, ["exact"]);
    let west = envelope(
        OTHER,
        &[SourceChange::Insert {
            element: element("w", "W"),
        }],
        1,
    )?;
    assert_eq!(messages(&sink, &west)?, ["suffix"]);
    Ok(())
}

#[test]
fn route_resolution_rejects_ambiguity_shadowing_and_partial_names() {
    let resolve = |routes: &[&str], aliases: &[&str]| {
        resolve_routes(routes.iter().copied(), aliases.iter().copied())
    };
    assert!(
        resolve(&["c", "b.c"], &["a.b.c"]).is_err(),
        "two suffix matches"
    );
    assert_eq!(
        resolve(&["c", "b.c", "a.b.c"], &["a.b.c", "x.c", "b.c"])
            .map(|r| r.len())
            .ok(),
        Some(3)
    );
    assert!(
        resolve(&["orders", "east.orders"], &["east.orders"]).is_err(),
        "shadowed route"
    );
    assert!(
        resolve(&["rders"], &["east.orders"]).is_err(),
        "suffix must start at a dot"
    );
    assert!(resolve(&["orders"], &[".orders"]).is_ok());
    assert!(resolve(&[], &["a"]).map(|r| r.is_empty()).unwrap_or(false));
}

// ------------------------------------------------------- formats and header

#[test]
fn compact_format_matches_the_legacy_shapes() -> Result<()> {
    let context = |value: Value| format_plain(LogFormat::Compact, &value);
    let identity = json!({"namespace": "n", "id": "1"});
    assert_eq!(
        context(json!({"operation": "ADD", "identity": identity, "after": {"a": 1}}))?,
        r#"[ADD] {"a":1}"#
    );
    assert_eq!(
        context(
            json!({"operation": "REPLACE", "identity": identity, "before": {"a": 1}, "after": {"a": 2}})
        )?,
        r#"[UPDATE] {"a":1} -> {"a":2}"#
    );
    assert_eq!(
        context(
            json!({"operation": "PATCH", "identity": identity, "after": {"a": 2}, "removedProperties": ["b"]})
        )?,
        r#"[UPDATE] {"id":"1","namespace":"n"} -> {"a":2} removed ["b"]"#
    );
    assert_eq!(
        context(json!({"operation": "DELETE", "identity": identity, "before": {"a": 1}}))?,
        r#"[DELETE] {"a":1}"#
    );
    assert_eq!(
        context(json!({"operation": "DELETE", "identity": identity}))?,
        r#"[DELETE] {"id":"1","namespace":"n"}"#
    );
    Ok(())
}

#[test]
fn header_line_precedes_indented_changes() -> Result<()> {
    let config = aliased("items", json!({"includeHeader": true, "format": "compact"}));
    let sink = sink(config)?;
    let input = envelope(
        SOURCE,
        &[
            SourceChange::Insert {
                element: element("one", "Alice"),
            },
            SourceChange::Insert {
                element: element("two", "Bob"),
            },
        ],
        9,
    )?;
    assert_eq!(
        sink.lines(&input)?,
        [
            format!("[{ID}] Output 'items' sequence 9 (2 changes):"),
            format!(r#"[{ID}]   [ADD] {{"id":"one","name":"Alice"}}"#),
            format!(r#"[{ID}]   [ADD] {{"id":"two","name":"Bob"}}"#),
        ]
    );
    let empty = envelope(SOURCE, &[], 10)?;
    assert!(
        sink.lines(&empty)?.is_empty(),
        "no header for an empty input"
    );
    Ok(())
}

// ------------------------------------------------------------ output (LG-03)

#[tokio::test(flavor = "current_thread")]
async fn level_and_target_are_honoured() -> Result<()> {
    install_logger();
    for (level, expected) in [
        (LogLevel::Error, log::Level::Error),
        (LogLevel::Warn, log::Level::Warn),
        (LogLevel::Info, log::Level::Info),
        (LogLevel::Debug, log::Level::Debug),
        (LogLevel::Trace, log::Level::Trace),
    ] {
        let target = format!("test.level.{expected}");
        Emitter {
            output: LogOutput::Log,
            level,
            target: target.clone(),
        }
        .emit(&["line".into()])
        .await?;
        assert_eq!(records(&target), [(expected, "line".to_owned())]);
    }
    Ok(())
}

#[test]
fn stream_output_writes_complete_flushed_lines() -> Result<()> {
    struct Recorder {
        writes: Vec<Vec<u8>>,
        flushed: bool,
    }
    impl std::io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushed = true;
            Ok(())
        }
    }
    let mut recorder = Recorder {
        writes: Vec::new(),
        flushed: false,
    };
    write_lines(&mut recorder, b"a\nb c\n")?;
    assert_eq!(recorder.writes, [b"a\nb c\n".to_vec()]);
    assert!(recorder.flushed);
    Ok(())
}

/// A stdout or stderr pipe that stops draining blocks only the writer
/// thread: the runtime keeps running, the waiting input can be cancelled,
/// and queued writes complete in order once the stream drains (LGA-4).
#[tokio::test(flavor = "current_thread")]
async fn a_blocked_stream_never_blocks_the_runtime() -> Result<()> {
    use std::sync::{Condvar, Mutex};
    static OPEN: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
    static WRITTEN: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    struct Gate;
    impl std::io::Write for Gate {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (open, changed) = &OPEN;
            let _open = changed
                .wait_while(open.lock().unwrap(), |open| !*open)
                .unwrap();
            WRITTEN.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let writer = StreamWriter::spawn("log-sink-test-gate", || Box::new(Gate))?;
    // The blocked write leaves the current-thread runtime free to run timers,
    // and the input waiting for it is cancelled by dropping the wait.
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(50),
        writer.write(b"first\n".to_vec())
    )
    .await
    .is_err());
    *OPEN.0.lock().unwrap() = true;
    OPEN.1.notify_all();
    writer.write(b"second\n".to_vec()).await?;
    assert_eq!(WRITTEN.lock().unwrap().as_slice(), b"first\nsecond\n");
    Ok(())
}
