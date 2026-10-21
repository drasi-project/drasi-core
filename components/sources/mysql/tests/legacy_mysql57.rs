// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
#[path = "../../../host-sdk/tests/fixtures/native_mysql/database.rs"]
mod database;

use anyhow::{Context, Result};
use drasi_core::models::{ElementValue, SourceChange};
use drasi_lib::{
    channels::SourceEvent, config::SourceSubscriptionSettings, error::OperationFailures, Source,
};
use drasi_source_mysql::{MySqlReplicationSource, SslMode, StartPosition, TableKeyConfig};
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL 5.7"]
async fn legacy_mysql57_cdc_with_explicit_positional_keys() -> Result<()> {
    let mut database = database::Database::start(false, true).await?;
    database
        .sql("CREATE TABLE items(`@0` INT PRIMARY KEY,`@1` VARCHAR(80) NOT NULL)")
        .await?;
    let source = MySqlReplicationSource::builder("mysql-bulk")
        .with_host("127.0.0.1")
        .with_port(database.port)
        .with_database("test")
        .with_user("root")
        .with_password("native-test")
        .with_ssl_mode(SslMode::Disabled)
        .with_tables(vec!["items".to_owned()])
        .with_start_position(StartPosition::FromEnd)
        .with_table_keys(vec![TableKeyConfig {
            table: "items".into(),
            key_columns: vec!["@0".into()],
        }])
        .build()?;
    let (status, _receiver) = tokio::sync::mpsc::channel(64);
    source
        .initialize(drasi_lib::context::SourceRuntimeContext::new(
            "mysql57-legacy",
            "mysql-bulk",
            None,
            status,
            None,
        ))
        .await;
    let proof = async {
        source.start().await.context("legacy source startup")?;
        let mut subscription = source
            .subscribe(SourceSubscriptionSettings {
                source_id: "mysql-bulk".into(),
                query_id: "legacy-query".into(),
                enable_bootstrap: false,
                nodes: ["items".to_owned()].into_iter().collect(),
                relations: Default::default(),
                resume_from: None,
                resume_sequence: None,
                request_position_handle: false,
            })
            .await?;
        database.expect_count(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE COMMAND LIKE 'Binlog Dump%'",
            "1",
        ).await.context("legacy MySQL 5.7 replication registration")?;
        database.sql("INSERT INTO items VALUES(7,'seven')").await?;
        let event = tokio::time::timeout(Duration::from_secs(15), subscription.receiver.recv())
            .await
            .context("legacy MySQL 5.7 row delivery")?
            .context("legacy MySQL 5.7 change")?;
        let SourceEvent::Change(SourceChange::Insert { element }) = &event.event else {
            anyhow::bail!("legacy MySQL 5.7 did not produce the inserted row");
        };
        assert_eq!(element.get_reference().element_id.as_ref(), "items:7");
        assert_eq!(element.get_property("@0"), &ElementValue::Integer(7));
        assert_eq!(
            element.get_property("@1"),
            &ElementValue::String("seven".into())
        );
        assert!(event.source_position.is_some());
        Ok(())
    }
    .await;
    let stopped = source.stop().await;
    let removed = database.cleanup();
    let failures: Vec<_> = [proof, stopped, removed]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(OperationFailures::new("legacy MySQL 5.7 source capability", failures).into())
    }
}
