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
    sleep(Duration::from_secs(2)).await;

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
        ResultDiff::Add { data } => data,
        ResultDiff::Delete { data } => data,
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
    sleep(Duration::from_secs(2)).await;

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

    drasi.start().await.unwrap();
    let mut subscription = handle
        .subscribe_with_options(Default::default())
        .await
        .unwrap();

    // Step 1: Wait for the bootstrap Add diff
    let mut bootstrap_data: Option<serde_json::Value> = None;
    for _ in 0..20 {
        if let Ok(Some(result)) = timeout(Duration::from_secs(2), subscription.recv()).await {
            for row in &result.results {
                if let ResultDiff::Add { data } = row {
                    if data.get("marker").and_then(|v| v.as_str()) == Some("v1") {
                        bootstrap_data = Some(data.clone());
                    }
                }
            }
        }
        if bootstrap_data.is_some() {
            break;
        }
    }
    let bootstrap_data = bootstrap_data.expect("Bootstrap Add diff was not received");

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
        "timestamp_col",
        "year_col",
        "enum_col",
        "set_col",
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
