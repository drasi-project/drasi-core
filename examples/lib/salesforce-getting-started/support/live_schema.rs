// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::BTreeMap;

use drasi_lib::schema::{GraphSchema, PropertyType, SourceSchema};
use drasi_lib::sources::Source;
use drasi_salesforce_common::InitialCursorBehavior;
use drasi_source_salesforce::SalesforceSource;

use super::*;

async fn live_catalog(connection: &Connection) -> Result<Catalog> {
    let auth = SalesforceAuthManager::new(
        connection.config.connection.clone(),
        Some(connection.identity.clone()),
    )?;
    let session = auth.session_token().await?;
    let rest = SalesforceRestClient::new(connection.config.connection.clone())?;
    let mut descriptions = Vec::new();
    for object in &connection.config.objects {
        descriptions.push(rest.describe_object(&session, object).await?);
    }
    Catalog::from_describes(descriptions)
}

async fn verify_public_schema(drasi: &DrasiLib, expected: &SourceSchema) -> Result<()> {
    let actual = drasi
        .get_source_schema(SOURCE)
        .await?
        .context("The live Salesforce source did not publish a schema")?;
    ensure!(
        &actual == expected,
        "Source inspection differs from fresh live Describe metadata"
    );
    let mut expected_graph = GraphSchema::default();
    expected_graph.merge_source_schema(SOURCE, expected);
    ensure!(
        drasi.get_graph_schema().await? == expected_graph,
        "Merged graph inspection differs from fresh live Describe metadata"
    );
    Ok(())
}

async fn wait_for_status(drasi: &DrasiLib, expected: ComponentStatus) -> Result<()> {
    let (_, mut events) = drasi.subscribe_source_events(SOURCE).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = drasi.get_source_status(SOURCE).await?;
            if status == expected {
                return Ok(());
            }
            ensure!(
                status != ComponentStatus::Error,
                "Live source entered Error while waiting for {expected:?}"
            );
            events
                .recv()
                .await
                .context("Source lifecycle event subscription failed")?;
        }
    })
    .await
    .with_context(|| format!("Timed out waiting for source status {expected:?}"))?
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires SALESFORCE_TEST_ORG and an authenticated Salesforce CLI login"]
async fn live_schema_discovery() -> Result<()> {
    let org = std::env::var("SALESFORCE_TEST_ORG")
        .context("Set SALESFORCE_TEST_ORG to the authenticated developer-org alias")?;
    ensure!(
        !org.trim().is_empty(),
        "SALESFORCE_TEST_ORG must not be empty"
    );
    let state = tempfile::Builder::new()
        .prefix("drasi-salesforce-schema-live-")
        .tempdir()?;
    let args = Args {
        org: Some(org),
        state_dir: Some(state.path().to_path_buf()),
        full_scan_seconds: 3600,
        objects: vec!["Account".into(), "Contact".into()],
        schema: false,
        query: Vec::new(),
        diagnose: false,
        local: false,
    };
    let mut connection = connect(&args).await?;
    connection.config.initial_cursor_behavior = InitialCursorBehavior::Latest;
    ensure!(
        Path::new(&connection.config.spool_root).starts_with(state.path()),
        "Live verification must use an isolated temporary spool"
    );
    let expected = live_catalog(&connection).await?.source_schema();
    ensure!(
        expected.nodes.len() == 2,
        "Expected both selected object schemas"
    );
    for label in ["Account", "Contact"] {
        let node = expected
            .nodes
            .iter()
            .find(|node| node.label == label)
            .with_context(|| format!("Missing live {label} schema"))?;
        ensure!(
            node.properties.iter().any(|field| {
                field.name == "Id" && field.data_type == Some(PropertyType::String)
            }),
            "Missing typed Id property for {label}"
        );
    }
    ensure!(
        expected.relations.iter().any(|relation| {
            relation.label == "REL_Account"
                && relation.from.as_deref() == Some("Contact")
                && relation.to.as_deref() == Some("Account")
        }),
        "Live Describe did not expose the Contact-to-Account relationship"
    );

    let source = SalesforceSource::builder(SOURCE)
        .with_config(connection.config.clone())
        .with_identity_provider(connection.identity.clone())
        .build()?;
    ensure!(
        source.describe_schema().is_none(),
        "Fresh source has a cached schema"
    );
    let drasi = DrasiLib::builder()
        .with_id("salesforce-live-schema-check")
        .with_source(source)
        .build()
        .await?;
    let outcome = async {
        drasi.start().await?;
        verify_public_schema(&drasi, &expected).await?;
        wait_for_status(&drasi, ComponentStatus::Running).await?;
        ensure!(
            Path::new(&connection.config.spool_root)
                .join("mirror.redb")
                .is_file(),
            "Live source did not create its isolated spool"
        );
        drasi.stop_source(SOURCE).await?;
        wait_for_status(&drasi, ComponentStatus::Stopped).await?;
        verify_public_schema(&drasi, &expected).await?;
        drasi.start_source(SOURCE).await?;
        wait_for_status(&drasi, ComponentStatus::Running).await?;
        let rediscovered = live_catalog(&connection).await?.source_schema();
        verify_public_schema(&drasi, &rediscovered).await?;
        drasi.stop_source(SOURCE).await?;
        wait_for_status(&drasi, ComponentStatus::Stopped).await?;
        drasi.remove_source(SOURCE, true).await?;
        ensure!(
            drasi.get_graph_schema().await? == GraphSchema::default(),
            "Removed source remains in graph inspection"
        );
        ensure!(
            !Path::new(&connection.config.spool_root)
                .join("mirror.redb")
                .exists(),
            "Verification spool was not deprovisioned"
        );
        Ok::<_, anyhow::Error>(rediscovered)
    }
    .await;
    let shutdown = drasi.shutdown().await;
    let verified = match (outcome, shutdown) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(shutdown_error)) => {
            Err(error.context(format!("Shutdown also failed: {shutdown_error}")))
        }
    }?;
    drop(drasi);
    state
        .close()
        .context("Failed to remove temporary test state")?;

    let properties = verified
        .nodes
        .iter()
        .map(|node| (node.label.as_str(), node.properties.len()))
        .collect::<BTreeMap<_, _>>();
    let relationships = verified
        .relations
        .iter()
        .map(|relation| {
            json!({
                "label": relation.label,
                "from": relation.from,
                "to": relation.to,
            })
        })
        .collect::<Vec<_>>();
    println!(
        "LIVE_SCHEMA_VERIFIED {}",
        json!({
            "endpoint": "real-salesforce",
            "object_property_counts": properties,
            "relationships": relationships,
            "fresh_describe_matches_source_api": true,
            "fresh_describe_matches_graph_api": true,
            "stopped_schema_retained": true,
            "restart_verified": true,
            "local_spool_deprovisioned": true,
            "salesforce_data_or_settings_mutated": false,
        })
    );
    Ok(())
}
