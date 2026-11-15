// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::lifecycle_ctx;

use super::*;
use drasi_computation_contracts::{ComponentId, InputBinding, OutputEndpoint, PortId, StreamId};
use drasi_computation_plugin_sdk::{
    lifecycle::UpdateBoundary, testing::SinkConformance, ActivationCredentials, ExportedPlugin,
    NativeComponent, Scope,
};
use drasi_reaction_common::OperationType;
use serde_json::{json, Value};

const OUTPUT_COMPONENT_ID: &str = "05a0315c-fc56-5d56-ab43-c9391eddf076";
const SINK_ID: &str = "ada6c229-bc29-54e3-ae61-b603c07a41b2";

fn settings() -> Value {
    json!({
        "hostname": "no-connection-at-construction", "database": "test",
        "user": "user", "password": "secret",
        "outputs": {"db.people": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
        "defaultTemplate": {"added": {"template": "CALL added({{param after.id}},{{param after}})"}},
        "routes": {"people": {"deleted": {"template": "CALL deleted({{param before.id}})"}}}
    })
}

fn inputs() -> Vec<InputBinding> {
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new(OUTPUT_COMPONENT_ID).expect("component"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new("people/out").expect("stream"),
    }]
}

fn request(configuration: Value) -> CreateRequest {
    CreateRequest {
        inputs: inputs(),
        id: ComponentId::try_new(SINK_ID).expect("id"),
        implementation: MySqlStoredProcFactory.metadata().implementation,
        configuration_version: 1,
        configuration,
        scope: Some(Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
        activation_credentials_bound: false,
        update_directive: None,
    }
}

fn consumer(
    request: &CreateRequest,
) -> Result<Box<dyn drasi_computation_plugin_sdk::NativeConsumer>> {
    match MySqlStoredProcFactory
        .create(request, ControlSender::default())?
        .component
    {
        Component::Consumer(consumer) => Ok(consumer),
        _ => anyhow::bail!("expected the MySQL stored-procedure consumer"),
    }
}

#[test]
fn factory_is_an_external_consumer_with_flat_published_configuration() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    #[cfg(feature = "dynamic-plugin")]
    assert!(!drasi_computation_plugin_metadata().is_null());
    let factory = MySqlStoredProcFactory;
    let metadata = factory.metadata();
    assert_eq!(factory.consumer_mode(), Some(ConsumerMode::External));
    ConsumerMode::External.validate_factory(&metadata)?;
    assert_eq!(metadata.role, ComponentRole::Sink);
    assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
    assert!(metadata.capabilities.erases_owned_state && metadata.capabilities.management);
    assert_eq!(metadata.ports, MySqlStoredProcSink::ports()?);
    assert_eq!(metadata.ports[0].id().as_str(), "in");
    let fields = &metadata.configuration.fields;
    assert!(fields["password"].secret);
    for field in ["routes", "defaultTemplate", "user", "hostname", "tlsCaPem"] {
        assert!(!fields[field].secret, "{field}");
    }
    assert!(!fields.contains_key("settings"));
    let details = factory.configuration_details().context("details")?;
    details.validate_schema(&metadata.configuration)?;
    let id = ComponentId::generate();
    assert_eq!(MySqlStoredProcSink::describe(id.clone())?.id(), &id);
    Ok(())
}

#[test]
fn configuration_round_trips_with_published_defaults() -> Result<()> {
    let factory = MySqlStoredProcFactory;
    let metadata = factory.metadata();
    let details = factory.configuration_details().context("details")?;
    let mut full = settings();
    full["ssl"] = json!(true);
    full["tlsCaPem"] = json!("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n");
    full["progressTable"] = json!("custom_progress");
    full["maxStreams"] = json!(8);
    for configuration in [settings(), full] {
        let exposed = consumer(&request(configuration.clone()))?.configuration()?;
        assert!(
            details.equivalent_with_defaults(&metadata.configuration, &configuration, &exposed)?,
            "{configuration} vs {exposed}"
        );
        assert_eq!(
            consumer(&request(exposed.clone()))?.configuration()?,
            exposed
        );
        assert_eq!(
            exposed["password"], "secret",
            "configuration() persists secrets"
        );
    }
    let config = MySqlStoredProcConfig::parse(settings())?;
    assert_eq!(
        config.template("db.people", OperationType::Delete),
        Some("CALL deleted({{param before.id}})")
    );
    assert_eq!(
        config.template("db.people", OperationType::Add),
        Some("CALL added({{param after.id}},{{param after}})")
    );
    Ok(())
}

#[test]
fn construction_rejects_unsafe_templates_and_invalid_settings() {
    for (field, value) in [
        (
            "outputs",
            json!({"bad id": {"component": OUTPUT_COMPONENT_ID}}),
        ),
        ("port", json!(0)),
        ("connectRetryAttempts", json!(33)),
        ("commandTimeoutMs", json!(0)),
        ("shutdownTimeoutMs", json!(0)),
        ("tlsCaPem", json!("CA-without-TLS")),
        ("progressTable", json!("drasi`; DROP TABLE x; --")),
        ("maxStreams", json!(0)),
        (
            "routes",
            json!({"unconfigured": {"added": {"template": "CALL x()"}}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "CALL x('{{after.name}}')"}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "CALL x({{json after}})"}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "CALL x({{{after.name}}})"}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "CALL {{after.proc}}()"}}),
        ),
        ("defaultTemplate", json!({"added": {"templat": "CALL x()"}})),
        ("settings", json!({})),
        ("retry_attempts", json!(1)),
        ("max_protocol_bytes", json!(1024)),
    ] {
        let mut configuration = settings();
        configuration[field] = value;
        assert!(consumer(&request(configuration)).is_err(), "{field}");
    }
    let mut no_scope = request(settings());
    no_scope.scope = None;
    assert!(
        consumer(&no_scope).is_err(),
        "the owner requires the graph scope"
    );
}

#[test]
fn owner_is_derived_from_graph_and_component() -> Result<()> {
    let expected = owner_id("graph", SINK_ID);
    assert_eq!(expected.len(), 64);
    assert_ne!(expected, owner_id("other-graph", SINK_ID));
    let sink = MySqlStoredProcSink::new(
        ComponentId::try_new(SINK_ID)?,
        MySqlStoredProcConfig::parse(settings())?,
        false,
        &inputs(),
        DeliveryScope::new(
            "instance".into(),
            "graph".into(),
            ComponentId::try_new(SINK_ID)?,
        )?,
    )?;
    assert_eq!(sink.owner(), expected);
    Ok(())
}

#[test]
fn assess_classifies_every_field() -> Result<()> {
    use UpdateBoundary::*;
    let suite = SinkConformance::new(&MyFactory, SINK_ID, inputs())?;
    let current = settings();
    let with = |field: &str, value: Value| {
        let mut proposed = current.clone();
        proposed[field] = value;
        proposed
    };
    let cases = vec![
        (current.clone(), Some(Pause)),
        (
            with(
                "routes",
                json!({"people": {"deleted": {"template": "CALL gone({{param before.id}})"}}}),
            ),
            Some(Pause),
        ),
        (
            with(
                "defaultTemplate",
                json!({"updated": {"template": "CALL u({{param after.id}})"}}),
            ),
            Some(Pause),
        ),
        (with("maxStreams", json!(10)), Some(Pause)),
        (with("commandTimeoutMs", json!(1000)), Some(Pause)),
        (with("connectTimeoutMs", json!(1000)), Some(Pause)),
        (with("shutdownTimeoutMs", json!(1000)), Some(Pause)),
        (with("connectRetryAttempts", json!(1)), Some(Pause)),
        (with("reconnectBudgetMs", json!(1000)), Some(Pause)),
        (with("hostname", json!("db.example")), Some(Stop)),
        (with("port", json!(3307)), Some(Stop)),
        (with("database", json!("other")), Some(Stop)),
        (with("user", json!("other")), Some(Stop)),
        (with("password", json!("rotated")), Some(Stop)),
        (with("ssl", json!(true)), Some(Stop)),
        (
            {
                let mut proposed = with("ssl", json!(true));
                proposed["tlsCaPem"] =
                    json!("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n");
                proposed
            },
            Some(Stop),
        ),
        (with("maxProtocolBytes", json!(1 << 20)), Some(Stop)),
        (
            with(
                "outputs",
                json!({"renamed": {"component": OUTPUT_COMPONENT_ID, "port": "out"}, "db.people": {"component": OUTPUT_COMPONENT_ID, "port": "out"}}),
            ),
            None,
        ),
        (with("progressTable", json!("other_progress")), None),
        (
            with(
                "defaultTemplate",
                json!({"added": {"template": "CALL x({{after.id}})"}}),
            ),
            None,
        ),
    ];
    suite.assess_matrix(&current, &cases)?;
    // Output alias changes on valid edges replace the component.
    let mut aliased = current.clone();
    aliased["outputs"] = json!({"people2": {"component": OUTPUT_COMPONENT_ID, "port": "out"}});
    aliased["routes"] = json!({});
    aliased["defaultTemplate"] = json!({"added": {"template": "CALL a({{param after.id}})"}});
    suite.assess_matrix(&current, &[(aliased, Some(Replace))])?;
    Ok(())
}

/// The factory, named for the conformance suite.
struct MyFactory;
impl Factory for MyFactory {
    fn metadata(&self) -> FactoryMetadata {
        MySqlStoredProcFactory.metadata()
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        MySqlStoredProcFactory.assess(request)
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        MySqlStoredProcFactory.create(request, control)
    }
}

#[tokio::test]
async fn activation_credentials_are_explicit_transient_and_not_constructor_io() -> Result<()> {
    let factory = MySqlStoredProcFactory;
    assert!(factory.supports_activation_credentials());
    let mut configuration = settings();
    configuration.as_object_mut().unwrap().remove("user");
    configuration.as_object_mut().unwrap().remove("password");
    let mut request = request(configuration);
    assert!(consumer(&request).is_err());
    request.activation_credentials_bound = true;
    let context = factory.activation_credential_context(&request)?;
    assert_eq!(context.hostname, "no-connection-at-construction");
    assert_eq!(context.port, 3306);
    let mut sink = consumer(&request)?;
    let original = sink.configuration()?;
    assert_eq!(original["user"], "");
    assert_eq!(original["password"], "");
    assert!(
        sink.start(&mut lifecycle_ctx::start()).await.is_err(),
        "bound startup cannot fall back to literals"
    );
    assert!(sink
        .start(&mut lifecycle_ctx::start_with(
            ActivationCredentials::Token {
                username: String::new(),
                token: "transient-only".into(),
            }
        ))
        .await
        .is_err());
    assert_eq!(sink.configuration()?, original);
    assert!(!sink.configuration()?.to_string().contains("transient-only"));
    sink.stop(&lifecycle_ctx::stop()).await?;
    assert!(
        sink.dispose(&lifecycle_ctx::erase()).await.is_err(),
        "erase without any activation account fails clearly instead of guessing"
    );
    Ok(())
}

#[tokio::test]
async fn a_cancelled_start_opens_nothing_and_stays_startable() -> Result<()> {
    let mut sink = consumer(&request(settings()))?;
    let mut ctx = lifecycle_ctx::start();
    ctx.cancellation().cancel();
    let error = sink.start(&mut ctx).await.expect_err("cancelled");
    assert!(error.is_retryable());
    sink.stop(&lifecycle_ctx::stop()).await?;
    Ok(())
}
