// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::ExportedPlugin;
use serde_json::json;

#[test]
fn positional_factories_are_explicit_and_do_not_claim_coordinated_loading() -> Result<()> {
    let factory = MySqlPositionalChangesFactory;
    let mut request = CreateRequest {
        inputs: Vec::new(),
        id: ComponentId::try_new("source")?,
        implementation: factory.metadata().implementation,
        configuration_version: 1,
        configuration: json!({
            "stream":"changes",
            "settings":{
                "connection":{"host":"no-io-at-construction","port":3306,"database":"test",
                    "user":"user","password":"secret","ssl_mode":"disabled","tls_ca_pem":null,
                    "server_id":1,"heartbeat_interval_ms":100},
                "tables":["items"],"table_keys":[{"table":"items","key_columns":["@0"]}],
                "output":"changes","replay":null,"start":{"mode":"end"},
                "transactions":{"max_changes":16,"max_bytes":1048576,"max_duration_ms":1000},
                "max_protocol_bytes":1048576,"io_timeout_ms":1000
            }
        }),
        scope: None,
        activation_credentials_bound: false,
    };
    factory.create(&request, ControlSender::default())?;
    assert!(!factory.supports_consumer_progress());
    assert!(MySqlSharedPositionalChangesFactory.supports_consumer_progress());
    assert_ne!(
        factory.metadata().implementation,
        MySqlChangesFactory.metadata().implementation
    );
    for keys in [json!([]), json!(["id"]), json!(["@00"]), json!(["@4096"])] {
        request.configuration["settings"]["table_keys"][0]["key_columns"] = keys;
        assert!(factory.create(&request, ControlSender::default()).is_err());
    }
    request.configuration["settings"]["table_keys"][0]["key_columns"] = json!(["@0"]);
    request.configuration["settings"]["start"] = json!({"mode":"snapshot","lock_timeout_ms":1000});
    assert!(factory.create(&request, ControlSender::default()).is_err());
    Ok(())
}

#[test]
fn native_mysql_modes_share_the_standard_schema_with_distinct_implementations_and_progress_contracts(
) -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let changes = MySqlChangesFactory.metadata();
    let rows = MySqlSharedChangesFactory.metadata();
    let transactions = MySqlTransactionsFactory.metadata();
    let shared = MySqlSharedTransactionsFactory.metadata();
    assert!(changes.configuration.fields["settings"].secret);
    assert_eq!(changes.ports, rows.ports);
    assert_eq!(transactions.ports, shared.ports);
    // Both modes publish standard graph data; the mode is the implementation, not a schema.
    assert_eq!(changes.ports, transactions.ports);
    assert_ne!(changes.implementation, transactions.implementation);
    assert!(!MySqlChangesFactory.supports_source_progress());
    assert!(!MySqlChangesFactory.supports_consumer_progress());
    assert!(MySqlTransactionsFactory.supports_source_progress());
    assert!(!MySqlTransactionsFactory.supports_consumer_progress());
    assert!(MySqlSharedTransactionsFactory.supports_consumer_progress());
    assert!(MySqlSharedChangesFactory.supports_consumer_progress());
    assert!(!MySqlSharedChangesFactory.supports_source_progress());
    assert!(rows.configuration.fields.contains_key("persistent"));
    assert_ne!(rows.implementation, shared.implementation);
    Ok(())
}
