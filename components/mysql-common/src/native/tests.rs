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
fn existing_shared_transaction_binding_bytes_remain_unchanged() -> Result<()> {
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
    let source = drasi_computation_contracts::ComponentId::try_new("source")?;
    let stream = drasi_computation_contracts::StreamId::try_new("changes")?;
    let original = br#"["mysql-native-consumers-v1","server","test",{"items":[{"name":"id","sql_type":"int","key":true,"nullable":false,"charset":null,"collation":null}]},"source","changes",{"mode":"position","file":"mysql-bin.000001","position":4},["graph","source"]]"#;
    let expected: [u8; 32] = Sha256::digest(original).into();
    assert_eq!(
        source::shared_source_binding(&value, "server", &tables, &source, &stream, "graph")?,
        expected
    );
    value.table_keys.push(crate::TableKeyConfig {
        table: "items".into(),
        key_columns: vec!["id".into()],
    });
    assert_ne!(
        source::shared_source_binding(&value, "server", &tables, &source, &stream, "graph")?,
        expected
    );
    Ok(())
}
