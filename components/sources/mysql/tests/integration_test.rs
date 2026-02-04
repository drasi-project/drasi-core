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
use drasi_source_mysql::{MySqlReplicationSource, StartPosition};

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
