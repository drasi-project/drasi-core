// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{MsSqlStoredProcFactory, MsSqlStoredProcSink};
use drasi_computation_contracts::{ComponentRole, PortDirection, SinkCompletion};
use drasi_computation_plugin_sdk::{
    lifecycle::{DisposeReason, LifecycleErrorKind, StateIntent, StopMode},
    testing::{contexts, create_request},
    ActivationCredentials, Component, ConsumerMode, ControlSender, ExportedPlugin, Factory,
    NativeComponent, Scope,
};
use drasi_core_types::models::ElementValue;
use drasi_mssql_common::sql::MAX_BOUND_PARAMETERS;

fn request(configuration: Value, bound: bool) -> drasi_computation_plugin_sdk::CreateRequest {
    let mut request = create_request(
        &MsSqlStoredProcFactory,
        &ComponentId::try_new(SINK_ID).expect("id"),
        configuration,
    );
    request.inputs = inputs();
    request.scope = Some(Scope {
        instance_id: "instance".into(),
        graph_id: GRAPH.into(),
        generation: 1,
    });
    request.activation_credentials_bound = bound;
    request
}

fn bound_sink(configuration: Value) -> Result<MsSqlStoredProcSink> {
    MsSqlStoredProcSink::new(
        ComponentId::try_new(SINK_ID)?,
        GRAPH,
        crate::MsSqlStoredProcConfig::parse(configuration)?,
        &inputs(),
        true,
    )
}

#[test]
fn factory_is_an_external_consumer_with_management_and_erase() -> Result<()> {
    let plugin = crate::plugin()?;
    ExportedPlugin::new(plugin)?;
    #[cfg(feature = "dynamic-plugin")]
    assert!(!crate::drasi_computation_plugin_metadata().is_null());
    let factory = MsSqlStoredProcFactory;
    let metadata = factory.metadata();
    assert_eq!(metadata.role, ComponentRole::Sink);
    assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
    assert_eq!(factory.consumer_mode(), Some(ConsumerMode::External));
    ConsumerMode::External.validate_factory(&metadata)?;
    assert!(metadata.capabilities.binds_inputs);
    assert!(metadata.capabilities.management);
    assert!(metadata.capabilities.erases_owned_state);
    assert!(factory.supports_update_planning());
    assert!(factory.supports_activation_credentials());
    assert_eq!(metadata.ports, MsSqlStoredProcSink::ports()?);
    assert_eq!(metadata.ports[0].id().as_str(), "in");
    assert_eq!(metadata.ports[0].direction(), PortDirection::Input);
    assert_eq!(
        metadata.ports[0].schema(),
        GraphChangeCodec::schema().descriptor()
    );
    Ok(())
}

#[test]
fn creation_needs_a_graph_scope_and_does_no_io() -> Result<()> {
    let created =
        MsSqlStoredProcFactory.create(&request(config(), false), ControlSender::default())?;
    let Component::Consumer(consumer) = created.component else {
        anyhow::bail!("the factory must create an external consumer");
    };
    assert_eq!(
        consumer.configuration()?["host"],
        "no-network-at-construction"
    );
    let mut unscoped = request(config(), false);
    unscoped.scope = None;
    assert!(MsSqlStoredProcFactory
        .create(&unscoped, ControlSender::default())
        .is_err());
    let context = MsSqlStoredProcFactory.activation_credential_context(&request(config(), true))?;
    assert_eq!(
        (context.hostname.as_str(), context.port),
        ("no-network-at-construction", 1433)
    );
    Ok(())
}

#[test]
fn empty_literals_require_an_identity_binding() -> Result<()> {
    let mut empty = config();
    empty["user"] = json!("");
    empty.as_object_mut().expect("object").remove("password");
    assert!(MsSqlStoredProcFactory
        .create(&request(empty.clone(), false), ControlSender::default())
        .is_err());
    MsSqlStoredProcFactory.create(&request(empty, true), ControlSender::default())?;
    Ok(())
}

#[test]
fn activation_credentials_select_sql_or_entra_id_authentication() -> Result<()> {
    let literal = sink(config())?;
    assert_eq!(
        literal.authentication(None)?,
        crate::Authentication::Sql {
            user: "u".into(),
            password: "p".into()
        }
    );
    assert!(
        literal
            .authentication(Some(ActivationCredentials::UsernamePassword {
                username: "x".into(),
                password: "y".into()
            }))
            .is_err(),
        "credentials the host did not bind are refused"
    );
    let bound = bound_sink(config())?;
    let declared = bound.configuration()?;
    assert!(
        bound.authentication(None).is_err(),
        "a bound identity never falls back to literals"
    );
    assert_eq!(
        bound.authentication(Some(ActivationCredentials::UsernamePassword {
            username: "provider-user".into(),
            password: "provider-secret".into(),
        }))?,
        crate::Authentication::Sql {
            user: "provider-user".into(),
            password: "provider-secret".into()
        }
    );
    assert_eq!(
        bound.authentication(Some(ActivationCredentials::Token {
            username: "ignored".into(),
            token: "eyJ0eXAi".into(),
        }))?,
        crate::Authentication::AccessToken {
            token: "eyJ0eXAi".into()
        }
    );
    for empty in [
        ActivationCredentials::UsernamePassword {
            username: String::new(),
            password: "x".into(),
        },
        ActivationCredentials::Token {
            username: "u".into(),
            token: String::new(),
        },
    ] {
        assert!(bound.authentication(Some(empty)).is_err());
    }
    assert_eq!(
        bound.configuration()?,
        declared,
        "declared configuration is immutable"
    );
    Ok(())
}

#[test]
fn operations_render_the_truthful_change_context_with_the_delivery_id() -> Result<()> {
    let mut configuration = config();
    configuration["defaultTemplate"] = json!({
        "added": {"template": "EXEC dbo.a {{param operation}}, {{param identity.id}}, {{param after.name}}, {{param output.alias}}, {{param deliveryId}}"},
        "deleted": {"template": "EXEC dbo.d {{param identity.id}}"}
    });
    let mut sink = sink(configuration)?;
    let batch = batch(
        1,
        &[
            insert("room-1", &[("name", ElementValue::String("one".into()))]),
            delete("room-1"),
        ],
    )?;
    let add = sink.render_item(&batch.item(0)?)?.expect("add command");
    assert_eq!(add.sql, "EXEC dbo.a @P1, @P2, @P3, @P4, @P5");
    assert_eq!(
        &add.parameters[..4],
        &[json!("ADD"), json!("room-1"), json!("one"), json!("q")]
    );
    assert_eq!(add.parameters[4], json!(batch.item(0)?.id.to_string()));
    let delete = sink.render_item(&batch.item(1)?)?.expect("delete command");
    assert_eq!(delete.parameters, [json!("room-1")]);
    Ok(())
}

#[test]
fn unconfigured_operations_render_nothing_and_missing_fields_fail() -> Result<()> {
    let mut sink = sink(config())?;
    let batch = batch(
        1,
        &[
            delete("room-1"),
            insert("room-2", &[("name", ElementValue::String("x".into()))]),
        ],
    )?;
    assert!(
        sink.render_item(&batch.item(0)?)?.is_none(),
        "no deleted template"
    );
    assert!(
        sink.render_item(&batch.item(1)?).is_err(),
        "after.id is absent: strict rendering fails"
    );
    Ok(())
}

#[test]
fn large_inputs_are_accepted_and_the_parameter_limit_is_per_command() -> Result<()> {
    let mut configuration = config();
    configuration["defaultTemplate"] = json!({"added": {"template":
        "EXEC dbo.p {{#each after.values}}{{param this}},{{/each}}"}});
    let mut sink = sink(configuration)?;
    let values = |count: usize| {
        ElementValue::List(
            (0..count as i64)
                .map(ElementValue::Integer)
                .collect::<Vec<_>>(),
        )
    };
    // Far more changes than the old 100 000-command cap would allow in
    // aggregate; each renders on its own.
    let changes: Vec<_> = (0..2_000)
        .map(|index| insert(&format!("r{index}"), &[("values", values(60))]))
        .collect();
    let large = batch(1, &changes)?;
    for index in [0, 999, 1_999] {
        let command = sink.render_item(&large.item(index)?)?.expect("command");
        assert_eq!(command.parameters.len(), 60);
    }
    let over = batch(
        2,
        &[insert(
            "big",
            &[("values", values(MAX_BOUND_PARAMETERS + 1))],
        )],
    )?;
    let error = sink
        .render_item(&over.item(0)?)
        .expect_err("over the limit");
    assert!(error
        .downcast_ref::<drasi_mssql_common::sql::ParameterLimitExceeded>()
        .is_some());
    let at = batch(
        3,
        &[insert("max", &[("values", values(MAX_BOUND_PARAMETERS))])],
    )?;
    assert_eq!(
        sink.render_item(&at.item(0)?)?
            .expect("command")
            .parameters
            .len(),
        MAX_BOUND_PARAMETERS
    );
    Ok(())
}

#[tokio::test]
async fn lifecycle_hooks_without_a_database() -> Result<()> {
    super::within(async {
        let mut sink = sink(config())?;
        sink.initialize(&contexts::initialize()).await?;
        sink.stop(&contexts::stop(StopMode::Drain)).await?;
        let error = sink
            .start(&mut contexts::cancelled_start())
            .await
            .expect_err("cancelled start");
        assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
        assert!(!sink.running(), "a cancelled start leaves nothing running");
        sink.pause(&contexts::pause()).await?;
        sink.resume(&contexts::resume()).await?;
        sink.stop(&contexts::stop(StopMode::Abort)).await?;
        sink.dispose(&contexts::dispose(
            DisposeReason::Shutdown,
            StateIntent::Preserve,
            1,
        )?)
        .await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn handle_outside_an_activation_fails_without_latching() -> Result<()> {
    super::within(async {
        use drasi_computation_contracts::DeliveryHandler;
        let mut sink = sink(config())?;
        let batch = batch(1, &[insert("r", &[("id", ElementValue::Integer(1))])])?;
        let error = sink.handle(batch.item(0)?).await.expect_err("not running");
        assert!(!sink.retryable(&error));
        assert!(sink.handle(batch.item(0)?).await.is_err());
        Ok(())
    })
    .await
}

#[tokio::test]
async fn erase_without_credentials_reports_the_manual_runbook() -> Result<()> {
    super::within(async {
        let mut empty = config();
        empty["user"] = json!("");
        empty.as_object_mut().expect("object").remove("password");
        let mut sink = bound_sink(empty)?;
        let owner = sink.owner().to_owned();
        let error = sink
            .dispose(&contexts::dispose(
                DisposeReason::Removal,
                StateIntent::Erase,
                1,
            )?)
            .await
            .expect_err("no identity available");
        let message = format!("{error:#}");
        assert!(message.contains(&owner), "{message}");
        assert!(
            message.contains("DELETE FROM [dbo].[drasi_delivery_progress]"),
            "{message}"
        );
        Ok(())
    })
    .await
}
