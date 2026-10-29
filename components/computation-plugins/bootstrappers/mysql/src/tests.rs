// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::ExportedPlugin;

#[test]
fn mysql_bootstrap_is_query_owned_and_not_a_source_or_private_graph() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let single = MySqlBootstrapFactory.metadata();
    let transactions = MySqlSharedTransactionBootstrapFactory.metadata();
    let rows = MySqlSharedRowBootstrapFactory.metadata();
    for metadata in [&single, &transactions, &rows] {
        assert!(metadata.source_progress);
        assert!(metadata.configuration.fields["settings"].secret);
    }
    assert_ne!(single.implementation, transactions.implementation);
    assert_ne!(transactions.implementation, rows.implementation);
    let standalone = MySqlStandaloneBootstrapFactory.metadata();
    assert!(!standalone.source_progress);
    assert!(standalone.configuration.fields["settings"].secret);
    assert!(standalone.configuration.fields.contains_key("labels"));
    assert_ne!(standalone.implementation, rows.implementation);
    Ok(())
}
