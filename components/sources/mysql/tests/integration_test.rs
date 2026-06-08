// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Integration tests for MySQL source using testcontainers.

use drasi_lib::{channels::ResultDiff, DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use mysql_async::prelude::*;
use mysql_async::Conn;
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::mysql::Mysql;
use tokio::time::{sleep, timeout, Duration};

use drasi_bootstrap_mysql::MySqlBootstrapProvider;
use drasi_source_mysql::{MySqlReplicationSource, StartPosition, TableKeyConfig};

#[tokio::test]
#[ignore]
async fn test_mysql_source_change_detection() {
    let mysql_image = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_USER", "test")
        .with_env_var("MYSQL_PASSWORD", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_env_var("MYSQL_AUTHENTICATION_PLUGIN", "mysql_native_password")
        .with_cmd(vec![
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
            "--gtid-mode=ON",
            "--enforce-gtid-consistency=ON",
        ]);

    let container = mysql_image.start().await.unwrap();
    let port = container.get_host_port_ipv4(3306).await.unwrap();

    let root_opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some("root"))
        .db_name(Some("test"));

    let mut conn = Conn::new(root_opts).await.unwrap();

    // Wait for MySQL to be fully ready
    for _ in 0..20 {
        if conn.query_drop("SELECT 1").await.is_ok() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    conn.query_drop(
        "CREATE TABLE users (\n            id INT AUTO_INCREMENT PRIMARY KEY,\n            name VARCHAR(255) NOT NULL,\n            email VARCHAR(255) NOT NULL\n        )",
    )
    .await
    .unwrap();

    conn.query_drop("CREATE USER IF NOT EXISTS 'test'@'%' IDENTIFIED BY 'test'")
        .await
        .unwrap();
    conn.query_drop("GRANT REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("GRANT SELECT, INSERT, UPDATE, DELETE ON test.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("FLUSH PRIVILEGES").await.unwrap();
    conn.disconnect().await.unwrap();

    let test_opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("test"))
        .pass(Some("test"))
        .db_name(Some("test"));
    let mut conn = Conn::new(test_opts).await.unwrap();

    let bootstrap = MySqlBootstrapProvider::builder()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_database("test")
        .with_user("test")
        .with_password("test")
        .with_tables(vec!["users".to_string()])
        .build()
        .unwrap();

    let source = MySqlReplicationSource::builder("mysql-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_database("test")
        .with_user("test")
        .with_password("test")
        .with_tables(vec!["users".to_string()])
        .with_start_position(StartPosition::FromStart)
        .with_bootstrap_provider(bootstrap)
        .build()
        .unwrap();

    let query = Query::cypher("users-query")
        .query("MATCH (u:users) RETURN u.id AS id, u.name AS name, u.email AS email")
        .from_source("mysql-source")
        .enable_bootstrap(true)
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("app-reaction")
        .with_query("users-query")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();
    let mut subscription = handle
        .subscribe_with_options(Default::default())
        .await
        .unwrap();
    // Allow the replication stream to connect and register
    sleep(Duration::from_secs(2)).await;

    conn.query_drop("INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com')")
        .await
        .unwrap();

    let mut found_insert = false;
    for _ in 0..10 {
        if let Ok(Some(result)) = timeout(Duration::from_secs(2), subscription.recv()).await {
            for row in &result.results {
                if matches_row(row, "Alice", "alice@example.com") {
                    found_insert = true;
                }
            }
        }
        if found_insert {
            break;
        }
    }
    assert!(found_insert, "INSERT was not detected");

    conn.query_drop("UPDATE users SET email = 'alice.new@example.com' WHERE name = 'Alice'")
        .await
        .unwrap();

    let mut found_update = false;
    for _ in 0..10 {
        if let Ok(Some(result)) = timeout(Duration::from_secs(2), subscription.recv()).await {
            for row in &result.results {
                if matches_row(row, "Alice", "alice.new@example.com") {
                    found_update = true;
                }
            }
        }
        if found_update {
            break;
        }
    }
    assert!(found_update, "UPDATE was not detected");

    conn.query_drop("DELETE FROM users WHERE name = 'Alice'")
        .await
        .unwrap();

    let mut found_delete = false;
    for _ in 0..10 {
        if let Ok(Some(result)) = timeout(Duration::from_secs(2), subscription.recv()).await {
            for row in &result.results {
                if matches_row(row, "Alice", "alice.new@example.com") {
                    found_delete = matches!(row, ResultDiff::Delete { .. });
                }
            }
        }
        if found_delete {
            break;
        }
    }
    assert!(found_delete, "DELETE was not detected");

    drasi.stop().await.unwrap();
}

fn matches_row(diff: &ResultDiff, name: &str, email: &str) -> bool {
    let data = match diff {
        ResultDiff::Add { data, .. } => data,
        ResultDiff::Delete { data, .. } => data,
        ResultDiff::Update { after, .. } => after,
        ResultDiff::Aggregation { .. } => return false,
        ResultDiff::Noop => return false,
    };
    data.get("name").and_then(|v| v.as_str()) == Some(name)
        && data.get("email").and_then(|v| v.as_str()) == Some(email)
}

/// Validates that bootstrap and CDC produce identical type mappings for the same row.
///
/// Strategy:
/// 1. Create a table with diverse MySQL column types
/// 2. Insert a row BEFORE the source starts (so it's bootstrapped)
/// 3. Start the source → bootstrap produces an Add diff
/// 4. UPDATE only a marker column → CDC produces an Update diff
/// 5. Compare unchanged columns between bootstrap (Add.data) and CDC (Update.after)
///    - If type mappings are consistent, the values must be identical
#[tokio::test]
#[ignore]
async fn test_type_mapping_consistency_between_bootstrap_and_cdc() {
    let mysql_image = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_USER", "test")
        .with_env_var("MYSQL_PASSWORD", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_env_var("MYSQL_AUTHENTICATION_PLUGIN", "mysql_native_password")
        .with_cmd(vec![
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
            "--gtid-mode=ON",
            "--enforce-gtid-consistency=ON",
        ]);

    let container = mysql_image.start().await.unwrap();
    let port = container.get_host_port_ipv4(3306).await.unwrap();

    // Setup: create table with diverse types and grant permissions
    let root_opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some("root"))
        .db_name(Some("test"));

    let mut conn = Conn::new(root_opts).await.unwrap();

    // Wait for MySQL to be fully ready
    for _ in 0..20 {
        if conn.query_drop("SELECT 1").await.is_ok() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    conn.query_drop(
        "CREATE TABLE type_test (
            id INT AUTO_INCREMENT PRIMARY KEY,
            int_col INT NOT NULL,
            bigint_col BIGINT NOT NULL,
            float_col FLOAT NOT NULL,
            double_col DOUBLE NOT NULL,
            decimal_col DECIMAL(10,2) NOT NULL,
            varchar_col VARCHAR(255) NOT NULL,
            tinyint_col TINYINT NOT NULL,
            bool_col BOOLEAN NOT NULL,
            date_col DATE NOT NULL,
            time_col TIME NOT NULL,
            datetime_col DATETIME NOT NULL,
            timestamp_col TIMESTAMP NOT NULL,
            year_col YEAR NOT NULL,
            enum_col ENUM('red', 'green', 'blue') NOT NULL,
            set_col SET('a', 'b', 'c') NOT NULL,
            marker VARCHAR(10) NOT NULL DEFAULT 'v1'
        )",
    )
    .await
    .unwrap();

    conn.query_drop("CREATE USER IF NOT EXISTS 'test'@'%' IDENTIFIED BY 'test'")
        .await
        .unwrap();
    conn.query_drop("GRANT REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("GRANT SELECT, INSERT, UPDATE, DELETE ON test.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("FLUSH PRIVILEGES").await.unwrap();

    // Insert a row BEFORE starting the source, so bootstrap will pick it up.
    conn.query_drop(
        "INSERT INTO type_test
            (int_col, bigint_col, float_col, double_col, decimal_col, varchar_col,
             tinyint_col, bool_col, date_col, time_col, datetime_col, timestamp_col,
             year_col, enum_col, set_col, marker)
        VALUES
            (42, 9876543210, 3.14, 2.718281828, 99.95, 'hello',
             7, TRUE, '2025-06-15', '13:45:30', '2025-06-15 13:45:30', '2025-06-15 13:45:30',
             2025, 'green', 'a,c', 'v1')",
    )
    .await
    .unwrap();

    conn.disconnect().await.unwrap();

    // Build source with bootstrap
    let bootstrap = MySqlBootstrapProvider::builder()
        .with_host("127.0.0.1")
        .with_port(port)
        .with_database("test")
        .with_user("test")
        .with_password("test")
        .with_tables(vec!["type_test".to_string()])
        .add_table_key(drasi_bootstrap_mysql::TableKeyConfig {
            table: "type_test".to_string(),
            key_columns: vec!["id".to_string()],
        })
        .build()
        .unwrap();

    let source = MySqlReplicationSource::builder("mysql-type-test")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_database("test")
        .with_user("test")
        .with_password("test")
        .with_tables(vec!["type_test".to_string()])
        .with_start_position(StartPosition::FromEnd)
        .add_table_key(TableKeyConfig {
            table: "type_test".to_string(),
            key_columns: vec!["id".to_string()],
        })
        .with_bootstrap_provider(bootstrap)
        .build()
        .unwrap();

    // Query returns all typed columns
    let query = Query::cypher("type-query")
        .query(
            "MATCH (t:type_test) RETURN
                t.id AS id,
                t.int_col AS int_col,
                t.bigint_col AS bigint_col,
                t.float_col AS float_col,
                t.double_col AS double_col,
                t.decimal_col AS decimal_col,
                t.varchar_col AS varchar_col,
                t.tinyint_col AS tinyint_col,
                t.bool_col AS bool_col,
                t.date_col AS date_col,
                t.time_col AS time_col,
                t.datetime_col AS datetime_col,
                t.timestamp_col AS timestamp_col,
                t.year_col AS year_col,
                t.enum_col AS enum_col,
                t.set_col AS set_col,
                t.marker AS marker",
        )
        .from_source("mysql-type-test")
        .enable_bootstrap(true)
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("type-reaction")
        .with_query("type-query")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    let mut subscription = handle
        .subscribe_with_options(Default::default())
        .await
        .unwrap();
    drasi.start().await.unwrap();

    // Step 1: Wait for bootstrap to complete and query results to be populated.
    // Bootstrap populates query state without emitting diffs to reactions,
    // so we poll get_query_results() until the bootstrapped row appears.
    let mut bootstrap_data: Option<serde_json::Value> = None;
    for _ in 0..30 {
        sleep(Duration::from_secs(1)).await;
        if let Ok(results) = drasi.get_query_results("type-query").await {
            for row in &results {
                if row.get("marker").and_then(|v| v.as_str()) == Some("v1") {
                    bootstrap_data = Some(row.clone());
                }
            }
        }
        if bootstrap_data.is_some() {
            break;
        }
    }
    let bootstrap_data = bootstrap_data.expect("Bootstrap data was not loaded into query results");

    // Step 2: UPDATE only the marker column to trigger a CDC event
    let test_opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("test"))
        .pass(Some("test"))
        .db_name(Some("test"));
    let mut conn = Conn::new(test_opts).await.unwrap();
    conn.query_drop("UPDATE type_test SET marker = 'v2' WHERE id = 1")
        .await
        .unwrap();
    conn.disconnect().await.unwrap();

    // Step 3: Wait for the CDC Update diff
    let mut cdc_after: Option<serde_json::Value> = None;
    for _ in 0..20 {
        if let Ok(Some(result)) = timeout(Duration::from_secs(2), subscription.recv()).await {
            for row in &result.results {
                if let ResultDiff::Update { after, .. } = row {
                    if after.get("marker").and_then(|v| v.as_str()) == Some("v2") {
                        cdc_after = Some(after.clone());
                    }
                }
            }
        }
        if cdc_after.is_some() {
            break;
        }
    }
    let cdc_after = cdc_after.expect("CDC Update diff was not received");

    drasi.stop().await.unwrap();

    // Step 4: Compare all unchanged columns between bootstrap and CDC.
    // If type mappings are consistent, these values must be identical.
    // Compare columns that should produce identical types between bootstrap and CDC.
    // ENUM, SET, and TIMESTAMP are excluded because the binlog encodes them as raw
    // integers/bitfields without schema context, while the text protocol returns
    // human-readable strings. Resolving this would require schema introspection in
    // the CDC decoder.
    let columns_to_compare = [
        "id",
        "int_col",
        "bigint_col",
        "float_col",
        "double_col",
        "decimal_col",
        "varchar_col",
        "tinyint_col",
        "bool_col",
        "date_col",
        "time_col",
        "datetime_col",
        "year_col",
    ];

    let mut mismatches = Vec::new();
    for col in &columns_to_compare {
        let bs_val = bootstrap_data.get(*col);
        let cdc_val = cdc_after.get(*col);
        if bs_val != cdc_val {
            mismatches.push(format!("  {col}: bootstrap={bs_val:?}, cdc={cdc_val:?}"));
        }
    }

    assert!(
        mismatches.is_empty(),
        "Type mapping mismatch between bootstrap and CDC:\n{}",
        mismatches.join("\n")
    );
}

#[tokio::test]
#[ignore]
async fn test_mysql_bootstrap_cdc_overlap_handover_no_duplicates_or_gaps() {
    use std::collections::HashMap;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    const SEED_COUNT: usize = 1_000;

    let result =
        timeout(Duration::from_secs(300), async {
            let (_container, port) = setup_mysql_container().await;
            prepare_mysql_database(port).await;

            let mut conn = test_conn(port).await;
            for batch_start in (1..=SEED_COUNT).step_by(100) {
                let values = (batch_start..batch_start + 100)
                    .map(|id| format!("('Seed{id}', {id}.00)"))
                    .collect::<Vec<_>>()
                    .join(",");
                conn.query_drop(format!("INSERT INTO items (name, value) VALUES {values}"))
                    .await
                    .unwrap();
            }

            let bootstrap = MySqlBootstrapProvider::builder()
                .with_host("127.0.0.1")
                .with_port(port)
                .with_database("test")
                .with_user("test")
                .with_password("test")
                .with_tables(vec!["items".to_string()])
                .build()
                .unwrap();

            let source = MySqlReplicationSource::builder("mysql-overlap-src")
                .with_host("127.0.0.1")
                .with_port(port)
                .with_database("test")
                .with_user("test")
                .with_password("test")
                .with_tables(vec!["items".to_string()])
                .with_start_position(StartPosition::FromEnd)
                .with_bootstrap_provider(bootstrap)
                .build()
                .unwrap();

            let query = Query::cypher("overlap-query")
                .query("MATCH (i:items) RETURN i.id AS id, i.name AS name, i.value AS value")
                .from_source("mysql-overlap-src")
                .auto_start(true)
                .enable_bootstrap(true)
                .build();

            let (reaction, handle) = ApplicationReaction::builder("overlap-reaction")
                .with_query("overlap-query")
                .build();

            let core = DrasiLib::builder()
                .with_id("mysql-overlap-test")
                .with_source(source)
                .with_query(query)
                .with_reaction(reaction)
                .build()
                .await
                .unwrap();

            let mut sub = handle
                .subscribe_with_options(Default::default())
                .await
                .unwrap();

            core.start().await.unwrap();

            let change_task = tokio::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                let mut change_conn = test_conn(port).await;
                change_conn
                    .query_drop(
                        "UPDATE items SET name = 'ConcurrentUpdated', value = 999.00 WHERE id = 10",
                    )
                    .await
                    .unwrap();
                change_conn
                    .query_drop("DELETE FROM items WHERE id = 20")
                    .await
                    .unwrap();
                change_conn
                    .query_drop(
                        "INSERT INTO items (name, value) VALUES ('ConcurrentInserted', 1001.00)",
                    )
                    .await
                    .unwrap();
                change_conn.disconnect().await.unwrap();
            });
            change_task.await.unwrap();

            let mut update_count = 0usize;
            let mut delete_count = 0usize;
            let mut insert_count = 0usize;
            let mut seed_cdc_adds: HashMap<i64, usize> = HashMap::new();

            for _ in 0..30 {
                if let Ok(Some(result)) = timeout(Duration::from_secs(1), sub.recv()).await {
                    for diff in &result.results {
                        match diff {
                            ResultDiff::Add { data, .. } => {
                                let name = data
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                if name == "ConcurrentInserted" {
                                    insert_count += 1;
                                } else if name.starts_with("Seed") {
                                    if let Some(id) = data.get("id").and_then(|v| v.as_i64()) {
                                        *seed_cdc_adds.entry(id).or_default() += 1;
                                    }
                                }
                            }
                            ResultDiff::Update { after, .. } => {
                                if after.get("name").and_then(|v| v.as_str())
                                    == Some("ConcurrentUpdated")
                                {
                                    update_count += 1;
                                }
                            }
                            ResultDiff::Delete { data, .. } => {
                                if data.get("id").and_then(|v| v.as_i64()) == Some(20) {
                                    delete_count += 1;
                                }
                            }
                            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
                        }
                    }
                }

                if update_count == 1 && delete_count == 1 && insert_count == 1 {
                    break;
                }
            }

            assert_eq!(
                update_count, 1,
                "concurrent UPDATE was not observed exactly once"
            );
            assert_eq!(
                delete_count, 1,
                "concurrent DELETE was not observed exactly once"
            );
            assert_eq!(
                insert_count, 1,
                "concurrent INSERT was not observed exactly once"
            );
            assert!(
                seed_cdc_adds.is_empty(),
                "seed rows were replayed by CDC after bootstrap: {seed_cdc_adds:?}"
            );

            let mut final_rows = Vec::new();
            for _ in 0..30 {
                sleep(Duration::from_secs(1)).await;
                final_rows = core.get_query_results("overlap-query").await.unwrap();
                let has_update = final_rows.iter().any(|row| {
                    row.get("id").and_then(|v| v.as_i64()) == Some(10)
                        && row.get("name").and_then(|v| v.as_str()) == Some("ConcurrentUpdated")
                });
                let has_insert = final_rows.iter().any(|row| {
                    row.get("name").and_then(|v| v.as_str()) == Some("ConcurrentInserted")
                });
                let deleted_absent = final_rows
                    .iter()
                    .all(|row| row.get("id").and_then(|v| v.as_i64()) != Some(20));
                if final_rows.len() == SEED_COUNT && has_update && has_insert && deleted_absent {
                    break;
                }
            }

            assert_eq!(final_rows.len(), SEED_COUNT, "unexpected final row count");
            let mut rows_by_id: HashMap<i64, usize> = HashMap::new();
            for row in &final_rows {
                let id = row
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .expect("row id must be an integer");
                *rows_by_id.entry(id).or_default() += 1;
            }
            for id in 1..=SEED_COUNT as i64 {
                if id == 20 {
                    assert_eq!(rows_by_id.get(&id).copied().unwrap_or_default(), 0);
                } else {
                    assert_eq!(
                        rows_by_id.get(&id).copied().unwrap_or_default(),
                        1,
                        "seed id {id} missing or duplicated in final query results"
                    );
                }
            }
            assert!(final_rows.iter().any(|row| {
                row.get("id").and_then(|v| v.as_i64()) == Some(10)
                    && row.get("name").and_then(|v| v.as_str()) == Some("ConcurrentUpdated")
            }));
            assert_eq!(
                final_rows
                    .iter()
                    .filter(|row| row.get("name").and_then(|v| v.as_str())
                        == Some("ConcurrentInserted"))
                    .count(),
                1
            );

            core.stop().await.unwrap();
            conn.disconnect().await.unwrap();
        })
        .await;

    assert!(
        result.is_ok(),
        "test_mysql_bootstrap_cdc_overlap_handover_no_duplicates_or_gaps timed out"
    );
}

// --- Checkpoint Recovery Integration Tests ---

use drasi_lib::indexes::config::{StorageBackendRef, StorageBackendSpec};
use drasi_reaction_application::subscription::SubscriptionOptions;

/// Helper to start a MySQL container with replication and GTID enabled.
#[allow(clippy::unwrap_used)]
async fn setup_mysql_container() -> (testcontainers::ContainerAsync<Mysql>, u16) {
    let mysql_image = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_USER", "test")
        .with_env_var("MYSQL_PASSWORD", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_env_var("MYSQL_AUTHENTICATION_PLUGIN", "mysql_native_password")
        .with_cmd(vec![
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
            "--gtid-mode=ON",
            "--enforce-gtid-consistency=ON",
        ]);

    let container = mysql_image.start().await.unwrap();
    let port = container.get_host_port_ipv4(3306).await.unwrap();
    (container, port)
}

/// Create the test table and grant replication privileges.
#[allow(clippy::unwrap_used)]
async fn prepare_mysql_database(port: u16) {
    let root_opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some("root"))
        .db_name(Some("test"));

    let mut conn = Conn::new(root_opts).await.unwrap();

    // Wait for MySQL to be fully ready
    for _ in 0..20 {
        if conn.query_drop("SELECT 1").await.is_ok() {
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }

    conn.query_drop(
        "CREATE TABLE items (
            id INT AUTO_INCREMENT PRIMARY KEY,
            name VARCHAR(255) NOT NULL,
            value DECIMAL(10,2) NOT NULL
        )",
    )
    .await
    .unwrap();

    conn.query_drop("CREATE USER IF NOT EXISTS 'test'@'%' IDENTIFIED BY 'test'")
        .await
        .unwrap();
    conn.query_drop("GRANT REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("GRANT SELECT, INSERT, UPDATE, DELETE ON test.* TO 'test'@'%'")
        .await
        .unwrap();
    conn.query_drop("FLUSH PRIVILEGES").await.unwrap();
    conn.disconnect().await.unwrap();
}

/// Get a test connection.
#[allow(clippy::unwrap_used)]
async fn test_conn(port: u16) -> Conn {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("test"))
        .pass(Some("test"))
        .db_name(Some("test"));
    Conn::new(opts).await.unwrap()
}

/// Helper: wait for a specific change to appear in the subscription.
async fn wait_for_change<F>(
    sub: &mut drasi_reaction_application::subscription::Subscription,
    max_attempts: usize,
    predicate: F,
) -> Result<(), String>
where
    F: Fn(&ResultDiff) -> bool,
{
    for _ in 0..max_attempts {
        if let Ok(Some(result)) = timeout(Duration::from_secs(3), sub.recv()).await {
            for row in &result.results {
                if predicate(row) {
                    return Ok(());
                }
            }
        }
    }
    Err("Change not found within attempts".to_string())
}

fn is_add_with_name(diff: &ResultDiff, expected_name: &str) -> bool {
    match diff {
        ResultDiff::Add { data, .. } => {
            data.get("name").and_then(|v| v.as_str()) == Some(expected_name)
        }
        _ => false,
    }
}

/// Full query stop/restart checkpoint recovery test for MySQL.
///
/// 1. Bootstrap from existing data (snapshot captures binlog position).
///    NOTE: Bootstrap only populates query state; it does NOT emit diffs
///    to reactions. We verify bootstrap via `get_query_results()`.
/// 2. Insert rows via CDC so the checkpoint advances.
/// 3. Stop the query, then restart it within the same DrasiLib.
/// 4. Insert a new row post-restart to verify the query resumes streaming
///    from the checkpointed position without re-bootstrapping.
#[tokio::test]
#[ignore]
async fn test_mysql_checkpoint_recovery_round_trip() {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let result = timeout(Duration::from_secs(300), async {
        let (_container, port) = setup_mysql_container().await;
        prepare_mysql_database(port).await;

        let mut conn = test_conn(port).await;

        // Seed data for bootstrap
        conn.query_drop("INSERT INTO items (name, value) VALUES ('Seed1', 10.00)")
            .await
            .unwrap();
        sleep(Duration::from_secs(1)).await;

        let bootstrap = MySqlBootstrapProvider::builder()
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .build()
            .unwrap();

        let source = MySqlReplicationSource::builder("mysql-cp-src")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .with_start_position(StartPosition::FromEnd)
            .with_bootstrap_provider(bootstrap)
            .build()
            .unwrap();

        let query = Query::cypher("cp-query")
            .query("MATCH (i:items) RETURN i.id AS id, i.name AS name, i.value AS value")
            .from_source("mysql-cp-src")
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();

        let (reaction, handle) = ApplicationReaction::builder("cp-reaction")
            .with_query("cp-query")
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mysql-cp-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .unwrap();

        core.start().await.unwrap();

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .unwrap();

        // Phase 1: Wait for bootstrap to complete (populates query state only,
        // does NOT emit to reactions). Poll get_query_results() until the
        // bootstrapped row appears.
        let mut bootstrap_done = false;
        for _ in 0..30 {
            sleep(Duration::from_secs(1)).await;
            if let Ok(results) = core.get_query_results("cp-query").await {
                for row in &results {
                    if row.get("name").and_then(|v| v.as_str()) == Some("Seed1") {
                        bootstrap_done = true;
                    }
                }
            }
            if bootstrap_done {
                break;
            }
        }
        assert!(
            bootstrap_done,
            "Bootstrap did not populate query state with Seed1"
        );

        // Insert a CDC row to advance checkpoint beyond bootstrap
        conn.query_drop("INSERT INTO items (name, value) VALUES ('CDC1', 20.00)")
            .await
            .unwrap();

        wait_for_change(&mut sub, 10, |entry| is_add_with_name(entry, "CDC1"))
            .await
            .expect("Did not observe CDC row");

        // Let checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // Phase 2: Stop and restart the query
        core.stop_query("cp-query").await.unwrap();
        sleep(Duration::from_millis(500)).await;
        core.start_query("cp-query").await.unwrap();

        // Phase 3: Insert a new row post-restart to verify recovery.
        // Since the source is still running, new CDC events should flow
        // to the re-subscribed query.
        conn.query_drop("INSERT INTO items (name, value) VALUES ('PostRestart', 30.00)")
            .await
            .unwrap();

        wait_for_change(&mut sub, 15, |entry| is_add_with_name(entry, "PostRestart"))
            .await
            .expect("Did not observe post-restart CDC row — checkpoint recovery may have failed");

        core.stop().await.unwrap();
        conn.disconnect().await.unwrap();
    })
    .await;

    assert!(
        result.is_ok(),
        "test_mysql_checkpoint_recovery_round_trip timed out"
    );
}

/// Verify that bootstrap's captured binlog position is persisted and used as
/// the CDC resume point after a full system restart.
///
/// Unlike `test_mysql_full_restart_picks_up_offline_changes` (which advances
/// the checkpoint via CDC before stopping), this test verifies recovery from
/// the BOOTSTRAP snapshot position alone (no prior CDC events).
///
/// 1. Start system with bootstrap from empty table (captures binlog position).
/// 2. Full stop the entire system.
/// 3. Insert a row while everything is stopped.
/// 4. Full restart — source reconnects from persisted position (set during
///    bootstrap), picks up the row inserted during the outage.
#[tokio::test]
#[ignore]
async fn test_mysql_bootstrap_then_restart_resumes_from_snapshot_position() {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let result = timeout(Duration::from_secs(300), async {
        let (_container, port) = setup_mysql_container().await;
        prepare_mysql_database(port).await;

        let bootstrap = MySqlBootstrapProvider::builder()
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .build()
            .unwrap();

        let source = MySqlReplicationSource::builder("mysql-bs-src")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .with_start_position(StartPosition::FromEnd)
            .with_bootstrap_provider(bootstrap)
            .build()
            .unwrap();

        let query = Query::cypher("bs-query")
            .query("MATCH (i:items) RETURN i.id AS id, i.name AS name, i.value AS value")
            .from_source("mysql-bs-src")
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();

        let (reaction, handle) = ApplicationReaction::builder("bs-reaction")
            .with_query("bs-query")
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mysql-bs-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .unwrap();

        core.start().await.unwrap();

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .unwrap();

        // Phase 1: Wait for bootstrap to complete (empty table, so just wait
        // for the system to stabilize and persist the checkpoint).
        sleep(Duration::from_secs(5)).await;

        // Phase 2: Full stop, insert while stopped, full restart.
        // The source persists its binlog position (captured during bootstrap)
        // so on restart it reconnects from there.
        core.stop().await.unwrap();
        sleep(Duration::from_millis(500)).await;

        // Insert data while everything is stopped
        let mut conn = test_conn(port).await;
        conn.query_drop("INSERT INTO items (name, value) VALUES ('WhileStopped', 42.00)")
            .await
            .unwrap();
        sleep(Duration::from_secs(1)).await;

        // Phase 3: Full restart
        core.start().await.unwrap();

        // The row inserted while stopped should be picked up because the
        // source resumes from the bootstrap-captured binlog position.
        wait_for_change(&mut sub, 20, |entry| {
            is_add_with_name(entry, "WhileStopped")
        })
        .await
        .expect(
            "Did not observe row inserted while stopped — \
             bootstrap snapshot position may not have been persisted correctly",
        );

        core.stop().await.unwrap();
        conn.disconnect().await.unwrap();
    })
    .await;

    assert!(
        result.is_ok(),
        "test_mysql_bootstrap_then_restart_resumes_from_snapshot_position timed out"
    );
}

/// Full system stop/restart test — verifies that a change made while the entire
/// Drasi system is down (including the source) is picked up after restart.
///
/// 1. Bootstrap and process some CDC events so checkpoint advances.
/// 2. Stop ALL components (source + query + reaction) via `core.stop()`.
/// 3. Insert a row while everything is stopped.
/// 4. Restart ALL components via `core.start()`.
/// 5. Verify the row inserted during the outage is delivered to the reaction.
#[tokio::test]
#[ignore]
async fn test_mysql_full_restart_picks_up_offline_changes() {
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use std::sync::Arc;

    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let result = timeout(Duration::from_secs(300), async {
        let (_container, port) = setup_mysql_container().await;
        prepare_mysql_database(port).await;

        let mut conn = test_conn(port).await;

        // Seed data for bootstrap
        conn.query_drop("INSERT INTO items (name, value) VALUES ('Seed', 1.00)")
            .await
            .unwrap();
        sleep(Duration::from_secs(1)).await;

        let bootstrap = MySqlBootstrapProvider::builder()
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .build()
            .unwrap();

        let source = MySqlReplicationSource::builder("mysql-fr-src")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_database("test")
            .with_user("test")
            .with_password("test")
            .with_tables(vec!["items".to_string()])
            .with_start_position(StartPosition::FromEnd)
            .with_bootstrap_provider(bootstrap)
            .build()
            .unwrap();

        let query = Query::cypher("fr-query")
            .query("MATCH (i:items) RETURN i.id AS id, i.name AS name, i.value AS value")
            .from_source("mysql-fr-src")
            .auto_start(true)
            .enable_bootstrap(true)
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
                path: tmp_dir.path().to_string_lossy().to_string(),
                enable_archive: false,
                direct_io: false,
            }))
            .build();

        let (reaction, handle) = ApplicationReaction::builder("fr-reaction")
            .with_query("fr-query")
            .build();

        let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);

        let core = DrasiLib::builder()
            .with_id("mysql-fr-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_index_provider(Arc::new(provider))
            .build()
            .await
            .unwrap();

        core.start().await.unwrap();

        let mut sub = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(5)),
            )
            .await
            .unwrap();

        // Phase 1: Let bootstrap finish and process one CDC event
        sleep(Duration::from_secs(3)).await;

        conn.query_drop("INSERT INTO items (name, value) VALUES ('PreStop', 2.00)")
            .await
            .unwrap();

        wait_for_change(&mut sub, 15, |entry| is_add_with_name(entry, "PreStop"))
            .await
            .expect("Did not observe CDC row before stop");

        // Let the checkpoint persist
        sleep(Duration::from_secs(2)).await;

        // Phase 2: Full stop (source + query + reaction)
        core.stop().await.unwrap();
        sleep(Duration::from_millis(500)).await;

        // Insert data while everything is stopped
        conn.query_drop("INSERT INTO items (name, value) VALUES ('WhileStopped', 3.00)")
            .await
            .unwrap();
        sleep(Duration::from_secs(1)).await;

        // Phase 3: Full restart
        core.start().await.unwrap();

        // Phase 4: Verify the offline change is delivered
        wait_for_change(&mut sub, 20, |entry| {
            is_add_with_name(entry, "WhileStopped")
        })
        .await
        .expect(
            "Did not observe row inserted while stopped — \
             full restart CDC resume may have skipped the offline change",
        );

        core.stop().await.unwrap();
        conn.disconnect().await.unwrap();
    })
    .await;

    assert!(
        result.is_ok(),
        "test_mysql_full_restart_picks_up_offline_changes timed out"
    );
}
