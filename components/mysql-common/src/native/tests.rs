// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

pub(super) fn config(port: u16, output: MySqlOutput) -> MySqlConfig {
    MySqlConfig {
        connection: MySqlConnectionConfig {
            host: "127.0.0.1".into(),
            port,
            database: "test".into(),
            user: "root".into(),
            password: "root".into(),
            ssl_mode: MySqlTlsMode::Disabled,
            tls_ca_pem: None,
            tls_client_cert_pem: None,
            tls_client_key_pem: None,
            server_id: NonZeroU32::new(874).unwrap(),
            heartbeat_interval_ms: NonZeroU64::new(100).unwrap(),
        },
        tables: vec!["items".into()],
        table_keys: Vec::new(),
        start: MySqlStartPosition::End,
        output,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: NonZeroUsize::new(20).unwrap(),
            max_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
            max_duration_ms: NonZeroU64::new(5000).unwrap(),
        },
        max_protocol_bytes: NonZeroUsize::new(1024 * 1024).unwrap(),
        io_timeout_ms: NonZeroU64::new(5000).unwrap(),
        reconnect_budget_ms: 0,
    }
}

#[test]
fn gtids_and_manual_keys_are_validated_without_changing_existing_configuration_shape() {
    let mut value = config(3306, MySqlOutput::Changes);
    assert!(serde_json::to_value(&value)
        .unwrap()
        .get("table_keys")
        .is_none());
    value.tables.clear();
    value.table_keys.push(crate::TableKeyConfig {
        table: "later".into(),
        key_columns: vec!["tenant".into(), "key".into()],
    });
    assert!(value.validate().is_ok());
    value.tables = vec!["items".into()];
    assert!(value.validate().is_err());
    value.table_keys[0].table = "items".into();
    assert!(value.validate().is_ok());
    value.table_keys[0].key_columns.push("key".into());
    assert!(value.validate().is_err());
    assert!(parse_gtid_set("24bc7850-2c16-11e6-a073-0242ac110002:1-9:12").is_ok());
    for set in [
        "",
        ",",
        "not-a-uuid:1",
        "24bc7850-2c16-11e6-a073-0242ac110002:bad",
    ] {
        assert!(parse_gtid_set(set).is_err(), "{set}");
    }
    assert!(parse_gtid_set(&"x".repeat(60 * 1024 + 1)).is_err());
}

#[test]
fn shared_transaction_binding_uses_the_component_uuid_stably() -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut value = config(3306, MySqlOutput::Transactions);
    value.start = MySqlStartPosition::Position {
        file: "mysql-bin.000001".into(),
        position: 4,
    };
    let tables = catalog::Tables::from([(
        "items".into(),
        vec![catalog::Column {
            name: "id".into(),
            sql_type: "int".into(),
            key: true,
            nullable: false,
            charset: None,
            collation: None,
        }],
    )]);
    let source =
        drasi_computation_contracts::ComponentId::try_new("8c950040-bfe1-5104-b7f0-744bb1e73e84")?;
    let stream = drasi_computation_contracts::StreamId::try_new("changes")?;
    // Binding format v2: only the selected table set, key columns and their
    // types are bound, so non-key column changes keep committed cursors.
    let original = br#"["mysql-native-consumers-v2","server","test",{"items":[["id","int"]]},"8c950040-bfe1-5104-b7f0-744bb1e73e84","changes",{"mode":"position","file":"mysql-bin.000001","position":4},["graph","8c950040-bfe1-5104-b7f0-744bb1e73e84"]]"#;
    let expected: [u8; 32] = Sha256::digest(original).into();
    assert_eq!(
        source::shared_source_binding(&value, "server", &tables, &source, &stream, "graph")?,
        expected
    );
    let mut widened = tables.clone();
    widened.get_mut("items").unwrap().push(catalog::Column {
        name: "note".into(),
        sql_type: "text".into(),
        key: false,
        nullable: true,
        charset: Some("utf8mb4".into()),
        collation: Some(255),
    });
    assert_eq!(
        source::shared_source_binding(&value, "server", &widened, &source, &stream, "graph")?,
        expected,
        "a non-key column change keeps the binding"
    );
    let mut retyped = tables.clone();
    retyped.get_mut("items").unwrap()[0].sql_type = "bigint".into();
    assert_ne!(
        source::shared_source_binding(&value, "server", &retyped, &source, &stream, "graph")?,
        expected,
        "a key type change alters the binding"
    );
    value.table_keys.push(crate::TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["id".into(), "note".into()],
    });
    assert_ne!(
        source::shared_source_binding(&value, "server", &widened, &source, &stream, "graph")?,
        expected,
        "different key columns alter the binding"
    );
    Ok(())
}

#[test]
fn bootstrap_tables_are_selected_by_the_querys_labels() {
    let labels = drasi_computation_contracts::BootstrapLabels::new(["items"], Vec::<String>::new())
        .expect("labels");
    assert!(table_selected(Some(&labels), "items"));
    assert!(!table_selected(Some(&labels), "orders"));
    assert!(table_selected(None, "orders"));
}

#[test]
fn paired_snapshots_connect_with_the_sources_activation_credentials() {
    let base = config(3306, MySqlOutput::Changes);
    let activation: ActivationCredentials = Default::default();
    assert_eq!(
        with_activation(&base, Some(&activation)).connection.user,
        base.connection.user,
        "before activation the configured login is used"
    );
    *activation.write().expect("activation") = Some(("granted".into(), "secret".into()));
    let active = with_activation(&base, Some(&activation));
    assert_eq!(active.connection.user, "granted");
    assert_eq!(active.connection.password, "secret");
}

/// Each progress binding declares exactly the durability its replay needs:
/// a whole-transaction or durable group source requires durable progress
/// (so binding it to volatile progress is refused before anything starts),
/// a non-durable group accepts any progress and keeps volatile progress
/// itself, and a source without replay keeps no progress or dependency.
#[test]
fn descriptions_declare_the_progress_durability_each_binding_requires() -> Result<()> {
    use drasi_computation_contracts::{
        check_dependency, DurabilityRequirement, OutputDescription, PortId, ProgressPersistence,
        ResourceCapabilities,
    };
    let id =
        drasi_computation_contracts::ComponentId::try_new("8c950040-bfe1-5104-b7f0-744bb1e73e84")?;
    let descriptor = MySqlSource::describe(id, MySqlOutput::Transactions)?;
    let out = PortId::try_new("out")?;
    for (progress, persistence, dependency) in [
        (MySqlProgress::None, ProgressPersistence::Volatile, None),
        (
            MySqlProgress::Single,
            ProgressPersistence::Durable,
            Some(("source_progress", DurabilityRequirement::Durable)),
        ),
        (
            MySqlProgress::Group { durable: true },
            ProgressPersistence::Durable,
            Some(("consumer_progress", DurabilityRequirement::Durable)),
        ),
        (
            MySqlProgress::Group { durable: false },
            ProgressPersistence::Volatile,
            Some(("consumer_progress", DurabilityRequirement::Any)),
        ),
    ] {
        let declared = description(progress, &descriptor);
        assert_eq!(declared.component.progress, persistence, "{progress:?}");
        // Outputs carry source replay progress, not a producer identity.
        assert_eq!(declared.output(&out), OutputDescription::UNTRACKED);
        declared
            .validate(&descriptor)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        match dependency {
            None => assert!(declared.dependencies.is_empty(), "{progress:?}"),
            Some((name, durability)) => {
                assert_eq!(declared.dependencies.len(), 1, "{progress:?}");
                assert_eq!(declared.dependencies[name].durability, durability);
                assert!(check_dependency(&declared, name, ResourceCapabilities::DURABLE).is_ok());
                assert_eq!(
                    check_dependency(&declared, name, ResourceCapabilities::VOLATILE).is_ok(),
                    durability == DurabilityRequirement::Any,
                    "{progress:?} against volatile progress"
                );
            }
        }
    }
    Ok(())
}
