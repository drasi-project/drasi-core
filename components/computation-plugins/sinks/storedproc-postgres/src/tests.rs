// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::config::{Command, Commands};
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, DisposeReason, OperationScope, StateIntent, StopMode},
    ComponentId, InputBinding, OutputEndpoint, PortId, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        ApplyContext, DisposeContext, InitializeContext, LifecycleErrorKind, PauseContext,
        ResumeContext, StartContext, StopContext, UpdateBoundary,
    },
    ApplyUpdateRequest, ExportedPlugin, NativeComponent, NativeManagement, Scope,
};
use drasi_reaction_common::OperationType;
use serde_json::{json, Value};

const OUTPUT_COMPONENT_ID: &str = "6439b52f-06c0-5e90-8905-d736f18e34ee";
const SINK_ID: &str = "a21a4fde-f642-54e1-93cf-bb9e177949d7";
const GRAPH: &str = "graph-under-test";

fn config() -> Value {
    json!({
        "host": "no-database-is-opened-at-construction",
        "database": "test", "user": "user", "password": "secret",
        "outputs": {"people": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
        "defaultTemplate": {"added": {"template": "CALL add_user({{param after.id}})"}}
    })
}

fn full_config() -> Value {
    json!({
        "host": "db.example", "port": 6543, "database": "test", "user": "user",
        "password": "secret", "sslMode": "require",
        "outputs": {"people": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
        "routes": {"people": {
            "added": {"template": "CALL add_user({{param after.id}})"},
            "updated": {"template": "CALL change_user({{param after.id}})"},
            "deleted": {"template": ""}
        }},
        "defaultTemplate": {"deleted": {"template": "CALL remove({{param before.id}})"}},
        "progressTable": "drasi.progress", "maxStreams": 8,
        "commandTimeoutMs": 1000, "connectTimeoutMs": 2000,
        "reconnectBudgetMs": 0, "shutdownTimeoutMs": 3000
    })
}

fn with(base: Value, field: &str, value: Value) -> Value {
    let mut config = base;
    config[field] = value;
    config
}

fn without(base: Value, field: &str) -> Value {
    let mut config = base;
    config.as_object_mut().unwrap().remove(field);
    config
}

fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").unwrap(),
        output: OutputEndpoint::new(
            ComponentId::try_new(OUTPUT_COMPONENT_ID).unwrap(),
            PortId::try_new("out").unwrap(),
        ),
        stream: StreamId::try_new("people/out").unwrap(),
    }]
}

fn request(configuration: Value) -> CreateRequest {
    let metadata = PostgresStoredProcFactory.metadata();
    CreateRequest {
        id: ComponentId::try_new(SINK_ID).unwrap(),
        implementation: metadata.implementation,
        configuration_version: metadata.configuration_version,
        configuration,
        scope: Some(Scope {
            instance_id: "instance".into(),
            graph_id: GRAPH.into(),
            generation: 1,
        }),
        inputs: inputs(),
        activation_credentials_bound: false,
        update_directive: None,
    }
}

fn sink(configuration: Value) -> Result<PostgresStoredProcSink> {
    PostgresStoredProcFactory.build(&request(configuration))
}

fn scope() -> OperationScope {
    OperationScope::new(
        "operation",
        SINK_ID,
        1,
        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10)),
        CancellationToken::new(),
    )
}

fn update(current: Value, proposed: Value) -> UpdateRequest {
    UpdateRequest {
        current: request(current),
        proposed: request(proposed),
    }
}

fn boundary(current: &Value, proposed: Value) -> Option<UpdateBoundary> {
    match PostgresStoredProcFactory.assess(&update(current.clone(), proposed)) {
        Ok(Assessment::Supported { boundary, .. }) => Some(boundary),
        _ => None,
    }
}

#[test]
fn factory_metadata_declares_consumer_management_erase_and_secrets() -> Result<()> {
    let exported = ExportedPlugin::new(plugin()?)?;
    let _ = exported.metadata();
    #[cfg(feature = "dynamic-plugin")]
    assert!(!drasi_computation_plugin_metadata().is_null());
    let factory = PostgresStoredProcFactory;
    let metadata = factory.metadata();
    assert_eq!(metadata.role, ComponentRole::Sink);
    assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
    assert_eq!(factory.consumer_mode(), Some(ConsumerMode::External));
    assert!(metadata.capabilities.erases_owned_state);
    assert!(metadata.capabilities.management && factory.supports_update_planning());
    assert!(factory.supports_activation_credentials());
    assert_eq!(metadata.ports.len(), 1);
    assert_eq!(metadata.ports[0].id().as_str(), "in");
    let details = factory.configuration_details().context("details")?;
    details.validate_schema(&metadata.configuration)?;
    let fields = &metadata.configuration.fields;
    for secret in ["password", "tlsClientPrivateKeyPem"] {
        assert!(fields[secret].secret, "{secret}");
    }
    for public in [
        "host",
        "routes",
        "defaultTemplate",
        "outputs",
        "progressTable",
        "tlsCaPem",
        "tlsClientCertificatePem",
        "deliveryGuarantee",
    ] {
        assert!(!fields[public].secret, "{public}");
    }
    assert_eq!(
        details.fields["progressTable"].default,
        Some(json!(DEFAULT_PROGRESS_TABLE))
    );
    assert_eq!(details.fields["maxStreams"].default, Some(json!(4096)));
    assert_eq!(details.fields["sslMode"].default, Some(json!("require")));
    assert_eq!(
        factory.activation_credential_context(&request(full_config()))?,
        ActivationCredentialContext {
            hostname: "db.example".into(),
            port: 6543
        }
    );
    Ok(())
}

#[test]
fn configuration_round_trips_with_published_defaults() -> Result<()> {
    let metadata = PostgresStoredProcFactory.metadata();
    let details = PostgresStoredProcFactory
        .configuration_details()
        .context("details")?;
    for configuration in [config(), full_config()] {
        let exposed = sink(configuration.clone())?.configuration()?;
        assert!(
            details.equivalent_with_defaults(&metadata.configuration, &configuration, &exposed)?,
            "{configuration} vs {exposed}"
        );
        assert_eq!(sink(exposed.clone())?.configuration()?, exposed);
        assert_eq!(exposed["password"], "secret");
    }
    let exposed = sink(config())?.configuration()?;
    assert_eq!(exposed["port"], 5432);
    assert_eq!(exposed["sslMode"], "require");
    assert_eq!(exposed["progressTable"], DEFAULT_PROGRESS_TABLE);
    assert_eq!(exposed["commandTimeoutMs"], 30_000);
    assert_eq!(exposed["deliveryGuarantee"], "exactlyOnce");
    let opted = sink(with(config(), "deliveryGuarantee", json!("atLeastOnce")))?;
    assert_eq!(opted.configuration()?["deliveryGuarantee"], "atLeastOnce");
    assert!(PostgresStoredProcConfig::parse(with(
        config(),
        "deliveryGuarantee",
        json!("at_least_once")
    ))
    .is_err());
    // Debug output never reveals the password.
    let parsed = PostgresStoredProcConfig::parse(config())?;
    assert!(!format!("{parsed:?}").contains("secret"));
    Ok(())
}

#[test]
fn configuration_rejects_injection_unknown_fields_and_invalid_bounds() {
    let loopback_plaintext = with(
        with(config(), "host", json!("127.0.0.1")),
        "sslMode",
        json!("disable"),
    );
    sink(loopback_plaintext.clone()).expect("plaintext to loopback is allowed");
    sink(with(
        with(config(), "sslMode", json!("disable")),
        "allowRemotePlaintext",
        json!(true),
    ))
    .expect("explicitly allowed remote plaintext");
    let template = |template: &str| json!({"added": {"template": template}});
    let cases = [
        ("unknown field", with(config(), "retryAttempts", json!(3))),
        (
            "legacy snake case",
            with(config(), "default_template", template("CALL x()")),
        ),
        ("legacy settings object", json!({"settings": config()})),
        (
            "bare interpolation",
            with(
                config(),
                "defaultTemplate",
                template("CALL x({{after.id}})"),
            ),
        ),
        (
            "raw interpolation",
            with(
                config(),
                "defaultTemplate",
                template("CALL x({{{after.id}}})"),
            ),
        ),
        (
            "json helper",
            with(
                config(),
                "defaultTemplate",
                template("CALL x({{json after}})"),
            ),
        ),
        (
            "partial",
            with(config(), "defaultTemplate", template("CALL x({{> other}})")),
        ),
        (
            "unknown block",
            with(
                config(),
                "defaultTemplate",
                template("{{#custom}}CALL x(){{/custom}}"),
            ),
        ),
        (
            "injection in block",
            with(
                config(),
                "defaultTemplate",
                template("{{#if after}}CALL x('{{after.name}}'){{/if}}"),
            ),
        ),
        (
            "param without value",
            with(config(), "defaultTemplate", template("CALL x({{param}})")),
        ),
        (
            "unclosed",
            with(
                config(),
                "defaultTemplate",
                template("CALL x({{param after.id)"),
            ),
        ),
        (
            "unknown command field",
            with(
                config(),
                "defaultTemplate",
                json!({"added": {"template": "CALL x()", "extra": 1}}),
            ),
        ),
        (
            "unknown operation",
            with(
                config(),
                "defaultTemplate",
                json!({"inserted": {"template": "CALL x()"}}),
            ),
        ),
        (
            "no command",
            with(config(), "defaultTemplate", template("")),
        ),
        (
            "route without output",
            with(config(), "routes", json!({"other": template("CALL x()")})),
        ),
        (
            "bad output alias",
            with(
                config(),
                "outputs",
                json!({"bad id": {"component": OUTPUT_COMPONENT_ID}}),
            ),
        ),
        ("port 0", with(config(), "port", json!(0))),
        ("missing database", without(config(), "database")),
        ("empty user", with(config(), "user", json!(""))),
        ("NUL password", with(config(), "password", json!("a\u{0}b"))),
        (
            "command timeout 0",
            with(config(), "commandTimeoutMs", json!(0)),
        ),
        (
            "command timeout overflow",
            with(config(), "commandTimeoutMs", json!(u64::from(u32::MAX))),
        ),
        (
            "connect timeout 0",
            with(config(), "connectTimeoutMs", json!(0)),
        ),
        (
            "shutdown timeout 0",
            with(config(), "shutdownTimeoutMs", json!(0)),
        ),
        (
            "reconnect budget too large",
            with(config(), "reconnectBudgetMs", json!(3_600_001)),
        ),
        ("max streams 0", with(config(), "maxStreams", json!(0))),
        (
            "max streams too large",
            with(config(), "maxStreams", json!(1_000_001)),
        ),
        (
            "unqualified table",
            with(config(), "progressTable", json!("progress")),
        ),
        (
            "quoted table",
            with(config(), "progressTable", json!("public.\"Progress\"")),
        ),
        (
            "table injection",
            with(config(), "progressTable", json!("public.t; DROP TABLE x")),
        ),
        (
            "unknown ssl mode",
            with(config(), "sslMode", json!("prefer")),
        ),
        (
            "remote plaintext",
            with(config(), "sslMode", json!("disable")),
        ),
        (
            "CA with plaintext",
            with(
                loopback_plaintext.clone(),
                "tlsCaPem",
                json!("-----BEGIN CERTIFICATE-----"),
            ),
        ),
        (
            "invalid CA",
            with(config(), "tlsCaPem", json!("not a certificate")),
        ),
        (
            "certificate without key",
            with(config(), "tlsClientCertificatePem", json!("pem")),
        ),
        (
            "key without certificate",
            with(config(), "tlsClientPrivateKeyPem", json!("pem")),
        ),
    ];
    for (name, configuration) in cases {
        assert!(sink(configuration).is_err(), "{name}");
    }
    let mut unscoped = request(config());
    unscoped.scope = None;
    assert!(PostgresStoredProcFactory.build(&unscoped).is_err());
    let mut unbound = request(config());
    unbound.inputs.clear();
    assert!(PostgresStoredProcFactory.build(&unbound).is_err());
}

#[test]
fn owner_is_derived_from_graph_and_component() -> Result<()> {
    let sink = sink(config())?;
    assert_eq!(sink.owner(), &DeliveryOwner::derive(GRAPH, SINK_ID));
    let mut other = request(config());
    other.scope.as_mut().unwrap().graph_id = "another-graph".into();
    assert_ne!(
        PostgresStoredProcFactory.build(&other)?.owner(),
        sink.owner()
    );
    Ok(())
}

#[test]
fn routes_resolve_alias_then_last_segment_then_default() -> Result<()> {
    let config = PostgresStoredProcConfig::parse(json!({
        "database": "d", "user": "u",
        "routes": {
            "app.people": {"added": {"template": "CALL full()"}},
            "people": {"updated": {"template": "CALL segment()"}, "deleted": {"template": ""}}
        },
        "defaultTemplate": {
            "added": {"template": "CALL default_add()"},
            "deleted": {"template": "CALL default_delete()"}
        }
    }))?;
    let commands = Commands::compile(&config)?;
    let render = |alias: &str, operation| match commands.resolve(alias, operation) {
        Command::Template(template) => Some(template.render(&json!({})).unwrap().0),
        Command::None => None,
    };
    assert_eq!(
        render("app.people", OperationType::Add).as_deref(),
        Some("CALL full()")
    );
    assert_eq!(
        render("app.people", OperationType::Update).as_deref(),
        Some("CALL segment()")
    );
    // An explicit empty template is "no command" and stops the fallback.
    assert_eq!(render("app.people", OperationType::Delete), None);
    assert_eq!(
        render("other", OperationType::Add).as_deref(),
        Some("CALL default_add()")
    );
    assert_eq!(
        render("other", OperationType::Delete).as_deref(),
        Some("CALL default_delete()")
    );
    assert_eq!(render("other", OperationType::Update), None);
    Ok(())
}

#[test]
fn assess_matrix_classifies_every_field() {
    use UpdateBoundary::*;
    let current = full_config();
    let tls = |field: &str, value: Value| with(full_config(), field, value);
    let cases: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
        ("unchanged", full_config(), Some(Pause)),
        (
            "routes",
            with(
                full_config(),
                "routes",
                json!({"people": {"added": {"template": "CALL other({{param after.id}})"}}}),
            ),
            Some(Pause),
        ),
        (
            "defaultTemplate",
            with(
                full_config(),
                "defaultTemplate",
                json!({"added": {"template": "CALL d()"}}),
            ),
            Some(Pause),
        ),
        (
            "maxStreams",
            with(full_config(), "maxStreams", json!(9)),
            Some(Pause),
        ),
        (
            "commandTimeoutMs",
            with(full_config(), "commandTimeoutMs", json!(5000)),
            Some(Pause),
        ),
        (
            "reconnectBudgetMs",
            with(full_config(), "reconnectBudgetMs", json!(1000)),
            Some(Pause),
        ),
        (
            "shutdownTimeoutMs",
            with(full_config(), "shutdownTimeoutMs", json!(1000)),
            Some(Pause),
        ),
        (
            "host",
            with(full_config(), "host", json!("db2.example")),
            Some(Stop),
        ),
        ("port", with(full_config(), "port", json!(5432)), Some(Stop)),
        (
            "database",
            with(full_config(), "database", json!("other")),
            Some(Stop),
        ),
        (
            "user",
            with(full_config(), "user", json!("other")),
            Some(Stop),
        ),
        (
            "password",
            with(full_config(), "password", json!("rotated")),
            Some(Stop),
        ),
        (
            "sslMode",
            with(
                with(full_config(), "sslMode", json!("disable")),
                "allowRemotePlaintext",
                json!(true),
            ),
            Some(Stop),
        ),
        (
            "allowRemotePlaintext",
            with(full_config(), "allowRemotePlaintext", json!(true)),
            Some(Stop),
        ),
        (
            "tlsCaPem",
            tls("tlsCaPem", json!(include_str!("../tests/fixtures/ca.pem"))),
            Some(Stop),
        ),
        (
            "connectTimeoutMs",
            with(full_config(), "connectTimeoutMs", json!(500)),
            Some(Stop),
        ),
        (
            "deliveryGuarantee",
            with(full_config(), "deliveryGuarantee", json!("atLeastOnce")),
            Some(Stop),
        ),
        (
            "outputs",
            with(
                full_config(),
                "outputs",
                json!({"people": {"component": OUTPUT_COMPONENT_ID}}),
            ),
            Some(Replace),
        ),
        (
            "progressTable",
            with(full_config(), "progressTable", json!("drasi.other")),
            None,
        ),
        (
            "invalid proposal",
            with(
                full_config(),
                "defaultTemplate",
                json!({"added": {"template": "CALL x({{after.id}})"}}),
            ),
            None,
        ),
    ];
    for (name, proposed, expected) in cases {
        assert_eq!(boundary(&current, proposed), expected, "{name}");
    }
    // TLS client identity fields are restart fields too (checked through the
    // planner, since a valid identity needs real key material).
    let base = PostgresStoredProcConfig::parse(full_config()).unwrap();
    for change in [
        |c: &mut PostgresStoredProcConfig| c.tls_client_certificate_pem = Some("cert".into()),
        |c: &mut PostgresStoredProcConfig| c.tls_client_private_key_pem = Some("key".into()),
    ] {
        let mut proposed = base.clone();
        change(&mut proposed);
        assert_eq!(
            management::plan(&base, &proposed, false),
            Assessment::supported(Stop, ComponentUpdateData::Preserve)
        );
    }
    assert_eq!(
        management::plan(&base, &base, true),
        Assessment::supported(Replace, ComponentUpdateData::Preserve)
    );
    // A different graph changes the cursor owner.
    let mut moved = update(full_config(), full_config());
    moved.proposed.scope.as_mut().unwrap().graph_id = "elsewhere".into();
    assert!(matches!(
        PostgresStoredProcFactory.assess(&moved),
        Ok(Assessment::Unsupported { .. })
    ));
    // A different component is not an update.
    let mut renamed = update(full_config(), full_config());
    renamed.proposed.id = ComponentId::generate();
    assert!(PostgresStoredProcFactory.assess(&renamed).is_err());
}

#[test]
fn error_classification_retries_only_transient_failures() {
    use std::time::Duration;
    let transient = [
        anyhow::Error::from(DeliveryError::Timeout(Duration::from_secs(1))),
        DeliveryError::CommitTimeout(Duration::from_secs(1)).into(),
    ];
    for error in &transient {
        assert!(component::retryable(error), "{error}");
    }
    let render = anyhow::Error::from(RenderError::for_test());
    let permanent = [
        DeliveryError::Rejected("gap".into()).into(),
        DeliveryError::Effect(render).into(),
        DeliveryError::Capacity {
            owner: "o".into(),
            table: "t".into(),
            limit: 1,
        }
        .into(),
        anyhow::anyhow!("an untyped failure is not retried"),
    ];
    for error in &permanent {
        assert!(!component::retryable(error), "{error}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_without_a_database_fails_retryably_and_stays_startable() -> Result<()> {
    // Port 1 on loopback refuses connections immediately.
    let configuration = with(
        with(
            with(config(), "host", json!("127.0.0.1")),
            "sslMode",
            json!("disable"),
        ),
        "port",
        json!(1),
    );
    let mut sink = sink(configuration.clone())?;
    sink.initialize(&InitializeContext::new(scope())).await?;
    sink.stop(&StopContext::new(scope(), StopMode::Drain))
        .await
        .context("stop before start")?;
    let cancelled = scope();
    cancelled.cancellation().cancel();
    let error = sink
        .start(&mut StartContext::new(cancelled, None))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    for _ in 0..2 {
        let error = sink
            .start(&mut StartContext::new(scope(), None))
            .await
            .unwrap_err();
        assert!(error.is_retryable(), "{error:?}");
    }
    sink.pause(&PauseContext::new(scope())).await?;
    sink.resume(&ResumeContext::new(scope())).await?;
    sink.dispose(&DisposeContext::new(
        scope(),
        DisposeReason::Shutdown,
        StateIntent::Preserve,
        1,
    )?)
    .await?;
    // Erase needs the database: it fails retryably and may be repeated.
    let mut erasing = self::sink(configuration)?;
    for attempt in 1..=2 {
        let error = erasing
            .dispose(&DisposeContext::new(
                scope(),
                DisposeReason::Removal,
                StateIntent::Erase,
                attempt,
            )?)
            .await
            .unwrap_err();
        assert!(error.is_retryable());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_stopped_sink() -> Result<()> {
    let mut sink = sink(full_config())?;
    let apply = |current: Value, proposed: Value| {
        let request = update(current, proposed);
        let plan = PostgresStoredProcFactory
            .assess(&request)
            .unwrap()
            .plan()
            .unwrap();
        ApplyUpdateRequest {
            operation_id: "operation".into(),
            request,
            plan,
            fingerprint: None,
        }
    };
    let templates = with(
        full_config(),
        "defaultTemplate",
        json!({"added": {"template": "CALL replaced({{param after.id}})"}}),
    );
    sink.apply(
        &ApplyContext::new(scope(), UpdateBoundary::Pause),
        &apply(full_config(), templates.clone()),
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(
        sink.configuration()?["defaultTemplate"],
        templates["defaultTemplate"]
    );
    let moved = with(templates.clone(), "host", json!("db2.example"));
    let stop = apply(templates.clone(), moved.clone());
    assert!(sink
        .apply(&ApplyContext::new(scope(), UpdateBoundary::Pause), &stop)
        .await
        .is_err());
    sink.apply(&ApplyContext::new(scope(), UpdateBoundary::Stop), &stop)
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    assert_eq!(sink.configuration()?["host"], "db2.example");
    // A stale current configuration is refused.
    assert!(sink
        .apply(
            &ApplyContext::new(scope(), UpdateBoundary::Pause),
            &apply(full_config(), full_config())
        )
        .await
        .is_err());
    Ok(())
}
