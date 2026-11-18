// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{BootstrapLabels, StreamId};
use drasi_computation_plugin_sdk::{ExportedPlugin, Scope, ServedSource};

#[test]
fn coordinated_providers_are_paired_and_take_no_configuration() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let factories = [
        (coordinated_factory(), coordination::TRANSACTIONS),
        (
            shared_transaction_factory(),
            coordination::SHARED_TRANSACTIONS,
        ),
        (shared_row_factory(), coordination::SHARED_ROWS),
    ];
    for (factory, protocol) in &factories {
        let metadata = factory.metadata();
        assert!(metadata.source_progress);
        assert_eq!(metadata.pairing, Some(coordination::protocol(protocol)));
        assert!(
            metadata.configuration.fields.is_empty(),
            "the source owns its settings, tables, keys, retention and identity"
        );
        metadata.validate_description(&factory.describe(&serde_json::json!({}))?)?;
        assert!(factory
            .describe(&serde_json::json!({"source": "8b637385-6167-54c4-9589-3dda2fcd1d21"}))
            .is_err());
    }
    assert_ne!(
        factories[0].0.metadata().implementation,
        factories[1].0.metadata().implementation
    );
    Ok(())
}

fn request() -> Result<CreateRequest> {
    Ok(CreateRequest {
        inputs: Vec::new(),
        activation_credentials_bound: false,
        update_directive: None,
        id: ComponentId::try_new("31ad5d55-58a2-52d3-a1c3-1b3ad9b2d1ed")?,
        implementation: MySqlStandaloneBootstrapFactory.metadata().implementation,
        configuration_version: 1,
        scope: Some(Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
        configuration: serde_json::json!({
            "host": "no-connection-at-construction", "database": "app", "user": "drasi",
            "tables": ["items", "orders"], "labels": ["items", "orders"]
        }),
    })
}

#[test]
fn standalone_snapshot_serves_its_relation_source() -> Result<()> {
    let standalone = MySqlStandaloneBootstrapFactory.metadata();
    assert!(!standalone.source_progress);
    assert_eq!(standalone.pairing, None);
    assert!(standalone.configuration.fields["password"].secret);
    assert!(!standalone.configuration.fields.contains_key("source"));
    assert!(standalone.configuration.fields.contains_key("labels"));
    let request = request()?;
    let description = MySqlStandaloneBootstrapFactory.describe(&request.configuration)?;
    assert_eq!(description.namespace, ElementNamespace::ServedSource);
    standalone.validate_description(&description)?;
    let session = BootstrapSession::new(
        Some(ServedSource {
            source: ComponentId::try_new("8b637385-6167-54c4-9589-3dda2fcd1d21")?,
            stream: StreamId::try_new("items/out")?,
        }),
        Some(BootstrapLabels::new(["orders"], Vec::<String>::new())?),
    );
    let provider = MySqlStandaloneBootstrapFactory.create(&request, &session, None)?;
    assert_eq!(provider.completion(), description.completion);
    provider.validate_query_scope("instance", "graph", &request.id)?;
    assert!(MySqlStandaloneBootstrapFactory
        .create(&request, &BootstrapSession::default(), None)
        .is_err());
    Ok(())
}
