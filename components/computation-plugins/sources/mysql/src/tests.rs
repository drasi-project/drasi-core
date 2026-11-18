// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, DisposeReason, OperationScope, StateIntent, StopMode},
    ComponentId, PortDirection,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        DisposeContext, InitializeContext, PauseContext, ResumeContext, StartContext, StopContext,
    },
    ExportedPlugin, NativeComponent,
};
use serde_json::{json, Value};
use std::time::Duration;

fn request(factory: &dyn Factory, configuration: Value) -> Result<CreateRequest> {
    Ok(CreateRequest {
        inputs: Vec::new(),
        id: ComponentId::try_new("8b637385-6167-54c4-9589-3dda2fcd1d21")?,
        implementation: factory.metadata().implementation,
        configuration_version: 1,
        configuration,
        scope: None,
        activation_credentials_bound: false,
        update_directive: None,
    })
}

/// A source configuration whose server refuses connections at once.
fn unreachable(extra: Value) -> Value {
    let mut value = json!({
        "stream": "changes",
        "host": "127.0.0.1",
        "port": 1,
        "database": "test",
        "user": "user",
        "password": "secret",
        "sslMode": "disabled",
        "ioTimeoutMs": 2000,
        "reconnectBudgetMs": 0
    });
    for (key, field) in extra.as_object().into_iter().flatten() {
        value[key] = field.clone();
    }
    value
}

fn scope(deadline: Option<tokio::time::Instant>, token: CancellationToken) -> OperationScope {
    OperationScope::new(
        "operation",
        "8b637385-6167-54c4-9589-3dda2fcd1d21",
        1,
        deadline,
        token,
    )
}

fn source(factory: &dyn Factory, configuration: Value) -> Result<Box<dyn NativeSourceObject>> {
    let created = factory.create(&request(factory, configuration)?, ControlSender::default())?;
    match created.component {
        Component::Source(source) => Ok(source),
        _ => anyhow::bail!("not a source"),
    }
}

use drasi_computation_plugin_sdk::NativeSource as NativeSourceObject;

#[test]
fn positional_factories_are_explicit_and_do_not_claim_coordinated_loading() -> Result<()> {
    let factory = MySqlPositionalChangesFactory;
    let mut request = request(
        &factory,
        unreachable(json!({
            "tables": ["items"],
            "tableKeys": [{"table": "items", "keyColumns": ["@0"]}]
        })),
    )?;
    factory.create(&request, ControlSender::default())?;
    assert!(!factory.supports_consumer_progress());
    assert!(MySqlSharedPositionalChangesFactory.supports_consumer_progress());
    assert_ne!(
        factory.metadata().implementation,
        MySqlChangesFactory.metadata().implementation
    );
    for keys in [json!([]), json!(["id"]), json!(["@00"]), json!(["@4096"])] {
        request.configuration["tableKeys"][0]["keyColumns"] = keys;
        assert!(factory.create(&request, ControlSender::default()).is_err());
    }
    request.configuration["tableKeys"][0]["keyColumns"] = json!(["@0"]);
    request.configuration["startFrom"] = json!("snapshot");
    assert!(factory.create(&request, ControlSender::default()).is_err());
    Ok(())
}

#[test]
fn factory_metadata_constructs_without_database_io() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    #[cfg(feature = "dynamic-plugin")]
    assert!(!drasi_computation_plugin_metadata().is_null());
    let factories: [(&str, &dyn Factory, MySqlOutput); 7] = [
        (MYSQL_CHANGES, &MySqlChangesFactory, MySqlOutput::Changes),
        (
            MYSQL_UNTRACKED_TRANSACTIONS,
            &MySqlUntrackedTransactionsFactory,
            MySqlOutput::Transactions,
        ),
        (
            MYSQL_TRANSACTIONS,
            &MySqlTransactionsFactory,
            MySqlOutput::Transactions,
        ),
        (
            MYSQL_SHARED_TRANSACTIONS,
            &MySqlSharedTransactionsFactory,
            MySqlOutput::Transactions,
        ),
        (
            MYSQL_SHARED_CHANGES,
            &MySqlSharedChangesFactory,
            MySqlOutput::Changes,
        ),
        (
            MYSQL_POSITIONAL_CHANGES,
            &MySqlPositionalChangesFactory,
            MySqlOutput::Changes,
        ),
        (
            MYSQL_SHARED_POSITIONAL_CHANGES,
            &MySqlSharedPositionalChangesFactory,
            MySqlOutput::Changes,
        ),
    ];
    for (name, factory, output) in factories {
        let metadata = factory.metadata();
        assert_eq!(
            metadata.implementation,
            ImplementationIdentity::try_new(name, "1")?
        );
        assert_eq!(metadata.role, ComponentRole::Source);
        assert_eq!(metadata.completion, None);
        assert_eq!(metadata.ports, MySqlSource::ports(output)?);
        assert_eq!(metadata.ports.len(), 1);
        assert_eq!(metadata.ports[0].id().as_str(), "out");
        assert_eq!(metadata.ports[0].direction(), PortDirection::Output);
        assert_eq!(
            metadata.ports[0].schema(),
            GraphChangeCodec::schema().descriptor()
        );
        assert!(metadata.configuration.fields["password"].secret);
        assert!(metadata.configuration.fields["tlsClientKeyPem"].secret);
        assert!(!metadata.configuration.fields.contains_key("settings"));
        assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
        assert!(factory.supports_update_planning());
        assert!(factory.supports_activation_credentials());
        let details = factory.configuration_details().context("details")?;
        details.validate_schema(&metadata.configuration)?;
        let id = ComponentId::generate();
        let descriptor = MySqlSource::describe(id.clone(), output)?;
        assert_eq!(descriptor.id(), &id);
        assert_eq!(descriptor.ports(), metadata.ports.as_slice());
    }
    assert!(!MySqlChangesFactory.supports_source_progress());
    assert!(!MySqlChangesFactory.supports_consumer_progress());
    assert!(MySqlTransactionsFactory.supports_source_progress());
    assert!(!MySqlTransactionsFactory.supports_consumer_progress());
    assert!(MySqlSharedTransactionsFactory.supports_consumer_progress());
    assert!(MySqlSharedChangesFactory.supports_consumer_progress());
    assert!(!MySqlSharedChangesFactory.supports_source_progress());
    assert!(MySqlSharedChangesFactory
        .metadata()
        .configuration
        .fields
        .contains_key("persistent"));
    Ok(())
}

/// Factory identity (and `persistent` on the shared row factories) selects
/// the declared progress contract. Every replaying factory names the
/// progress dependency its cursors live in; a factory whose cursors must
/// survive a restart requires it durable, so the graph refuses binding it to
/// volatile progress at wiring time. Untracked factories keep only volatile
/// progress (MY-36). Outputs carry source replay progress, never a producer
/// identity.
#[test]
fn every_factory_declares_its_progress_durability() -> Result<()> {
    use drasi_computation_contracts::{
        check_dependency, DurabilityRequirement, OutputDescription, PortId, ProgressPersistence,
        ResourceCapabilities,
    };
    let position = json!({"startFrom": "position", "startBinlogFile": "binlog.000001",
        "startBinlogPosition": 4});
    let durable = Some(DurabilityRequirement::Durable);
    let any = Some(DurabilityRequirement::Any);
    let cases: [(
        &dyn Factory,
        Value,
        MySqlOutput,
        ProgressPersistence,
        Option<(&str, DurabilityRequirement)>,
    ); 9] = [
        (
            &MySqlChangesFactory,
            json!({}),
            MySqlOutput::Changes,
            ProgressPersistence::Volatile,
            None,
        ),
        (
            &MySqlUntrackedTransactionsFactory,
            json!({}),
            MySqlOutput::Transactions,
            ProgressPersistence::Volatile,
            None,
        ),
        (
            &MySqlPositionalChangesFactory,
            json!({"tables": ["items"], "tableKeys": [{"table": "items", "keyColumns": ["@0"]}]}),
            MySqlOutput::Changes,
            ProgressPersistence::Volatile,
            None,
        ),
        (
            &MySqlTransactionsFactory,
            position.clone(),
            MySqlOutput::Transactions,
            ProgressPersistence::Durable,
            durable.map(|d| ("source_progress", d)),
        ),
        (
            &MySqlSharedTransactionsFactory,
            position,
            MySqlOutput::Transactions,
            ProgressPersistence::Durable,
            durable.map(|d| ("consumer_progress", d)),
        ),
        (
            &MySqlSharedChangesFactory,
            json!({"persistent": true}),
            MySqlOutput::Changes,
            ProgressPersistence::Durable,
            durable.map(|d| ("consumer_progress", d)),
        ),
        (
            &MySqlSharedChangesFactory,
            json!({"persistent": false}),
            MySqlOutput::Changes,
            ProgressPersistence::Volatile,
            any.map(|d| ("consumer_progress", d)),
        ),
        (
            &MySqlSharedPositionalChangesFactory,
            json!({"persistent": true, "tables": ["items"],
                "tableKeys": [{"table": "items", "keyColumns": ["@0"]}]}),
            MySqlOutput::Changes,
            ProgressPersistence::Durable,
            durable.map(|d| ("consumer_progress", d)),
        ),
        (
            &MySqlSharedPositionalChangesFactory,
            json!({"tables": ["items"],
                "tableKeys": [{"table": "items", "keyColumns": ["@0"]}]}),
            MySqlOutput::Changes,
            ProgressPersistence::Volatile,
            any.map(|d| ("consumer_progress", d)),
        ),
    ];
    for (factory, extra, output, progress, dependency) in cases {
        let name = factory.metadata().implementation.name;
        let request = request(factory, unreachable(extra))?;
        let descriptor = MySqlSource::describe(request.id.clone(), output)?;
        let declared = factory.describe(&drasi_computation_plugin_sdk::wire::DescribeRequest {
            request,
            descriptor: descriptor.clone(),
            dependencies: Default::default(),
        })?;
        assert_eq!(declared.component.progress, progress, "{name}");
        assert_eq!(
            declared.output(&PortId::try_new("out")?),
            OutputDescription::UNTRACKED,
            "{name}"
        );
        declared
            .validate(&descriptor)
            .map_err(|error| anyhow::anyhow!("{name}: {error}"))?;
        match dependency {
            None => assert!(declared.dependencies.is_empty(), "{name}"),
            Some((slot, durability)) => {
                assert_eq!(
                    declared.dependencies.keys().collect::<Vec<_>>(),
                    [slot],
                    "{name}"
                );
                assert_eq!(declared.dependencies[slot].durability, durability, "{name}");
                assert!(check_dependency(&declared, slot, ResourceCapabilities::DURABLE).is_ok());
                assert_eq!(
                    check_dependency(&declared, slot, ResourceCapabilities::VOLATILE).is_err(),
                    durability == DurabilityRequirement::Durable,
                    "{name} bound to volatile progress"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn configuration_round_trips_through_untracked_factories() -> Result<()> {
    for (factory, extra) in [
        (&MySqlChangesFactory as &dyn Factory, json!({})),
        (&MySqlUntrackedTransactionsFactory, json!({})),
        (
            &MySqlPositionalChangesFactory,
            json!({"tables": ["items"], "tableKeys": [{"table": "items", "keyColumns": ["@0"]}]}),
        ),
    ] {
        let configuration = unreachable(extra);
        let created = source(factory, configuration.clone())?;
        let reported = created.configuration()?;
        assert_eq!(reported["password"], json!("secret"));
        assert_eq!(reported["serverId"], Value::Null, "serverId stays derived");
        for (key, value) in configuration.as_object().into_iter().flatten() {
            assert_eq!(&reported[key], value, "{key}");
        }
        let again = source(factory, reported.clone())?;
        assert_eq!(again.configuration()?, reported);
    }
    Ok(())
}

#[test]
fn credential_context_names_the_configured_endpoint() -> Result<()> {
    let factory = MySqlChangesFactory;
    let context =
        factory.activation_credential_context(&request(&factory, unreachable(json!({})))?)?;
    assert_eq!(context.hostname, "127.0.0.1");
    assert_eq!(context.port, 1);
    Ok(())
}

async fn lifecycle_without_a_server() -> Result<()> {
    let mut source = source(&MySqlChangesFactory, unreachable(json!({})))?;
    source
        .initialize(&InitializeContext::new(scope(
            None,
            CancellationToken::new(),
        )))
        .await?;
    // Pause and resume of a source that is not running settle at once.
    source
        .pause(&PauseContext::new(scope(None, CancellationToken::new())))
        .await?;
    source
        .resume(&ResumeContext::new(scope(None, CancellationToken::new())))
        .await?;
    // A failed start (connection refused) keeps the cleanup guard.
    let failed = source
        .start(&mut StartContext::new(
            scope(None, CancellationToken::new()),
            None,
        ))
        .await;
    assert!(failed.is_err());
    assert!(source
        .start(&mut StartContext::new(
            scope(None, CancellationToken::new()),
            None
        ))
        .await
        .is_err_and(|error| format!("{error:#}").contains("cleanup")));
    source
        .stop(&StopContext::new(
            scope(None, CancellationToken::new()),
            StopMode::Abort,
        ))
        .await?;
    // A pre-cancelled start returns promptly and leaves nothing running.
    let token = CancellationToken::new();
    token.cancel();
    let started = tokio::time::Instant::now();
    assert!(source
        .start(&mut StartContext::new(scope(None, token), None))
        .await
        .is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    source
        .stop(&StopContext::new(
            scope(None, CancellationToken::new()),
            StopMode::Drain,
        ))
        .await?;
    // An expired deadline bounds the start.
    assert!(source
        .start(&mut StartContext::new(
            scope(Some(tokio::time::Instant::now()), CancellationToken::new()),
            None
        ))
        .await
        .is_err());
    source
        .stop(&StopContext::new(
            scope(None, CancellationToken::new()),
            StopMode::Drain,
        ))
        .await?;
    // Erase owns nothing and succeeds twice.
    for attempt in 1..=2 {
        source
            .dispose(&DisposeContext::new(
                scope(None, CancellationToken::new()),
                DisposeReason::Removal,
                StateIntent::Erase,
                attempt,
            )?)
            .await?;
    }
    assert!(source
        .source_schema_reader()
        .context("schema reader")?
        .describe_schema()
        .await
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_without_a_server_current_thread() -> Result<()> {
    lifecycle_without_a_server().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_without_a_server_multi_thread() -> Result<()> {
    lifecycle_without_a_server().await
}

#[test]
fn factory_assessment_classifies_requests() -> Result<()> {
    let factory = MySqlChangesFactory;
    let current = request(&factory, unreachable(json!({})))?;
    let mut proposed = current.clone();
    proposed.configuration["password"] = json!("rotated");
    let assessment = factory.assess(&UpdateRequest {
        current: current.clone(),
        proposed: proposed.clone(),
    })?;
    assert!(matches!(
        assessment,
        Assessment::Supported {
            boundary: drasi_computation_plugin_sdk::lifecycle::UpdateBoundary::Stop,
            ..
        }
    ));
    proposed.configuration["database"] = json!("other");
    assert!(matches!(
        factory.assess(&UpdateRequest { current, proposed })?,
        Assessment::Unsupported { .. }
    ));
    Ok(())
}
