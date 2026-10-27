// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::{ActivationCredentials, ExportedPlugin, NativeComponent};
use drasi_reaction_common::OperationType;
use serde_json::{json, Value};

fn settings() -> Value {
    json!({
        "hostname":"no-connection-at-construction","database":"test","user":"user","password":"secret",
        "outputs":{"db.people":{"component":"db.people","port":"out"}},
        "default_template":{"added":{"template":"CALL added({{param after.id}},{{param after}})"}},
        "routes":{"people":{"deleted":{"template":"CALL deleted({{param before.id}})"}}}
    })
}

fn inputs() -> Vec<drasi_computation_contracts::InputBinding> {
    use drasi_computation_contracts::{InputBinding, OutputEndpoint, PortId, StreamId};
    vec![InputBinding {
        input_port: PortId::try_new("in").expect("port"),
        output: OutputEndpoint::new(
            ComponentId::try_new("db.people").expect("component"),
            PortId::try_new("out").expect("port"),
        ),
        stream: StreamId::try_new("people/out").expect("stream"),
    }]
}

#[tokio::test]
async fn activation_credentials_are_explicit_transient_and_not_constructor_io() -> Result<()> {
    let factory = MySqlStoredProcFactory;
    assert!(factory.supports_activation_credentials());
    let mut settings = settings();
    settings.as_object_mut().unwrap().remove("user");
    settings.as_object_mut().unwrap().remove("password");
    let mut request = CreateRequest {
        inputs: inputs(),
        id: ComponentId::try_new("sink")?,
        implementation: factory.metadata().implementation,
        configuration_version: 1,
        configuration: json!({"settings":settings}),
        scope: None,
        activation_credentials_bound: false,
    };
    assert!(factory.create(&request, ControlSender::default()).is_err());
    request.activation_credentials_bound = true;
    let context = factory.activation_credential_context(&request)?;
    assert_eq!(context.hostname, "no-connection-at-construction");
    assert_eq!(context.port, 3306);
    let Component::Sink(mut sink) = factory
        .create(&request, ControlSender::default())?
        .component
    else {
        anyhow::bail!("expected native MySQL sink");
    };
    let original = sink.configuration()?;
    assert_eq!(original["settings"]["user"], "");
    assert_eq!(original["settings"]["password"], "");
    assert!(
        sink.start().await.is_err(),
        "bound startup cannot fall back to literals"
    );
    assert!(sink
        .start_with_credentials(ActivationCredentials::Token {
            username: String::new(),
            token: "transient-only".into(),
        })
        .await
        .is_err());
    assert_eq!(sink.configuration()?, original);
    assert!(!sink.configuration()?.to_string().contains("transient-only"));
    sink.stop().await?;
    Ok(())
}

#[test]
fn native_mysql_factory_is_handled_and_construction_only() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let metadata = MySqlStoredProcFactory.metadata();
    assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
    assert!(metadata.configuration.fields["settings"].secret);
    let sink = MySqlStoredProcSink::new(
        ComponentId::try_new("sink")?,
        serde_json::from_value(settings())?,
        &inputs(),
    )?;
    let config = sink.configuration()?;
    assert_eq!(config["settings"]["password"], "secret");
    assert_eq!(config["settings"]["port"], 3306);
    assert_eq!(config["settings"]["retry_attempts"], 3);
    let config: MySqlStoredProcConfig = serde_json::from_value(config["settings"].clone())?;
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
fn native_mysql_rejects_unsafe_sql_and_invalid_resource_limits() {
    for (field, value) in [
        ("outputs", json!({"bad id": {"component": "db.people"}})),
        ("port", json!(0)),
        ("retry_attempts", json!(9)),
        ("command_timeout_ms", json!(0)),
        ("shutdown_timeout_ms", json!(0)),
        ("tls_ca_pem", json!("CA-without-TLS")),
        (
            "routes",
            json!({"unconfigured":{"added":{"template":"CALL x()"}}}),
        ),
        (
            "default_template",
            json!({"added":{"template":"CALL x('{{after.name}}')"}}),
        ),
        (
            "default_template",
            json!({"added":{"template":"CALL x({{json after}})"}}),
        ),
    ] {
        let mut value_config = settings();
        value_config[field] = value;
        let parsed = serde_json::from_value::<MySqlStoredProcConfig>(value_config)
            .map_err(anyhow::Error::from)
            .and_then(|config| {
                config.validate()?;
                let outputs = drasi_computation_contracts::SinkOutputs::from_inputs(
                    &inputs(),
                    &config.outputs,
                )?;
                config.validate_outputs(&outputs)
            });
        assert!(parsed.is_err(), "{field}");
    }
}
