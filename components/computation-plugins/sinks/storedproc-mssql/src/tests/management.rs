// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{management::FIELD_BOUNDARIES, MsSqlStoredProcFactory};
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, StopMode, UpdateBoundary},
    testing::{contexts, update_request},
    ApplyUpdateRequest, Factory, NativeComponent,
};

fn changed(field: &str, value: Value) -> Value {
    let mut configuration = config();
    configuration[field] = value;
    configuration
}

fn assess(
    current: &Value,
    proposed: &Value,
    proposed_inputs: Vec<InputBinding>,
) -> Result<Assessment> {
    let mut request = update_request(
        &MsSqlStoredProcFactory,
        &ComponentId::try_new(SINK_ID)?,
        current.clone(),
        proposed.clone(),
    );
    request.current.inputs = inputs();
    request.proposed.inputs = proposed_inputs;
    MsSqlStoredProcFactory
        .update_planner()
        .expect("factory planner")
        .assess(&request)
}

fn boundary(assessment: Assessment) -> Option<UpdateBoundary> {
    match assessment {
        Assessment::Supported { boundary, .. } => Some(boundary),
        Assessment::Unsupported { .. } => None,
    }
}

#[test]
fn every_field_has_a_classified_boundary() -> Result<()> {
    let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    let cases: Vec<(&str, Value)> = vec![
        ("host", json!("other-host")),
        ("port", json!(1444)),
        ("database", json!("other")),
        ("user", json!("other-user")),
        ("password", json!("rotated")),
        ("encryption", json!("loginOnly")),
        ("trustServerCertificate", json!(true)),
        ("trustedCaPem", json!(pem)),
        ("hostNameInCertificate", json!("sql.example")),
        ("progressTable", json!("dbo.other_progress")),
        ("onPermanentFailure", json!("skip")),
        ("maxStreams", json!(8)),
        ("deliveryGuarantee", json!("atLeastOnce")),
        (
            "outputs",
            json!({"renamed": {"component": OUTPUT_ID, "port": "out"}}),
        ),
        (
            "routes",
            json!({"q": {"deleted": {"template": "EXEC dbo.d {{param before.id}}"}}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "EXEC dbo.other {{param after.id}}"}}),
        ),
        ("commandTimeoutMs", json!(1000)),
        ("connectTimeoutMs", json!(1000)),
        ("shutdownTimeoutMs", json!(500)),
    ];
    // Every published field is covered by the matrix and the table.
    let published: Vec<String> = MsSqlStoredProcFactory
        .metadata()
        .configuration
        .fields
        .keys()
        .cloned()
        .collect();
    for field in &published {
        assert!(
            cases.iter().any(|(name, _)| name == field),
            "{field} untested"
        );
        assert!(
            FIELD_BOUNDARIES.iter().any(|(name, _)| name == field),
            "{field} unclassified"
        );
    }
    let current = config();
    for (field, value) in cases {
        let expected = FIELD_BOUNDARIES
            .iter()
            .find(|(name, _)| *name == field)
            .map(|(_, boundary)| *boundary)
            .expect("classified");
        let actual = boundary(assess(&current, &changed(field, value), inputs())?);
        assert_eq!(actual, expected, "{field}");
    }
    assert_eq!(
        boundary(assess(&current, &current, inputs())?),
        Some(UpdateBoundary::Pause),
        "no change"
    );
    // A change to the bound inputs replaces the component.
    let mut other_inputs = inputs();
    other_inputs[0].stream = StreamId::try_new("q/other")?;
    assert_eq!(
        boundary(assess(&current, &current, other_inputs)?),
        Some(UpdateBoundary::Replace)
    );
    // The strongest changed field wins.
    let mut both = changed("routes", json!({}));
    both["user"] = json!("u2");
    assert_eq!(
        boundary(assess(&current, &both, inputs())?),
        Some(UpdateBoundary::Stop)
    );
    // Invalid proposals are refused, not planned.
    assert!(assess(&current, &changed("maxStreams", json!(0)), inputs()).is_err());
    Ok(())
}

async fn apply(
    sink: &mut crate::MsSqlStoredProcSink,
    current: Value,
    proposed: Value,
    at: UpdateBoundary,
) -> Result<(), drasi_computation_plugin_sdk::lifecycle::ApplyError> {
    let mut request = update_request(
        &MsSqlStoredProcFactory,
        &ComponentId::try_new(SINK_ID).expect("id"),
        current,
        proposed,
    );
    request.current.inputs = inputs();
    request.proposed.inputs = inputs();
    let management = sink.management().expect("management");
    let assessment = management.assess(&request).expect("assessment");
    let plan = assessment.plan().expect("supported");
    let update = ApplyUpdateRequest {
        operation_id: "test-apply".into(),
        request,
        plan,
        fingerprint: assessment.fingerprint().map(str::to_owned),
    };
    management.apply(&contexts::apply(at), &update).await
}

#[tokio::test]
async fn stopped_sinks_apply_in_place_and_restart_updates() -> Result<()> {
    super::within(async {
        let mut sink = sink(config())?;
        let routes = changed(
            "defaultTemplate",
            json!({"added": {"template": "EXEC dbo.v2 {{param after.id}}"}}),
        );
        apply(&mut sink, config(), routes.clone(), UpdateBoundary::Pause)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(
            sink.configuration()?["defaultTemplate"]["added"]["template"],
            "EXEC dbo.v2 {{param after.id}}"
        );
        let mut host = routes.clone();
        host["user"] = json!("u2");
        assert!(
            apply(
                &mut sink,
                routes.clone(),
                host.clone(),
                UpdateBoundary::Pause
            )
            .await
            .is_err(),
            "a restart field is refused at the Pause boundary"
        );
        apply(
            &mut sink,
            routes.clone(),
            host.clone(),
            UpdateBoundary::Stop,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        assert_eq!(sink.configuration()?["user"], "u2");
        assert!(
            apply(&mut sink, routes, config(), UpdateBoundary::Stop)
                .await
                .is_err(),
            "a stale current configuration is refused"
        );
        sink.stop(&contexts::stop(StopMode::Drain)).await?;
        Ok(())
    })
    .await
}
