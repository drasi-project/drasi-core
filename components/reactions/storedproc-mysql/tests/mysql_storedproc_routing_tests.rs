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

//! End-to-end routing tests for the MySQL Stored Procedure reaction.
//!
//! These tests exercise per-query route resolution (route override vs. default
//! template, and last-dotted-segment matching) as well as positional parameter
//! binding for different JSON value types, driven through the real reaction
//! against a MySQL container.

mod mysql_helpers;

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::Reaction;
use drasi_reaction_storedproc_mysql::config::{
    MySqlStoredProcReactionConfig, QueryConfig, TemplateSpec,
};
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;
use mysql_helpers::{
    create_test_stored_procedure, get_procedure_log_count, get_procedure_log_entries, setup_mysql,
    MysqlConfig, ProcedureLogEntry,
};
use serde_json::json;
use serial_test::serial;
use tokio::time::sleep;

async fn setup_procedures(config: &MysqlConfig) {
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("connect for proc setup");
    create_test_stored_procedure(&mut conn)
        .await
        .expect("create procedures");
    drop(conn);
}

async fn entries(config: &MysqlConfig) -> Vec<ProcedureLogEntry> {
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("connect for read");
    let result = get_procedure_log_entries(&mut conn)
        .await
        .expect("read log");
    drop(conn);
    result
}

async fn wait_for_count(config: &MysqlConfig, expected: i64, timeout_ms: u64) {
    let step = 100;
    let mut waited = 0;
    while waited < timeout_ms {
        let opts = config.connection_opts();
        let mut conn = mysql_async::Conn::new(opts)
            .await
            .expect("connect for count");
        let count = get_procedure_log_count(&mut conn).await.unwrap_or(0);
        drop(conn);
        if count >= expected {
            return;
        }
        sleep(Duration::from_millis(step)).await;
        waited += step;
    }
}

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(query_id.to_string(), 0, Utc::now(), diffs, HashMap::new())
}

fn add_user_template(proc: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(format!(
            "CALL {proc}({{{{param after.id}}}}, {{{{param after.name}}}}, {{{{param after.email}}}})"
        ))),
        updated: None,
        deleted: None,
    }
}

#[tokio::test]
#[serial]
async fn test_route_overrides_default_template() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_procedures(&config).await;

    // Route for `routed-query` calls update_user; default calls add_user.
    let mut routes = HashMap::new();
    routes.insert("routed-query".to_string(), add_user_template("update_user"));

    let reaction_config = MySqlStoredProcReactionConfig {
        hostname: config.host.clone(),
        port: Some(config.port),
        user: config.user.clone(),
        password: config.password.clone(),
        database: config.database.clone(),
        routes,
        default_template: Some(add_user_template("add_user")),
        ..Default::default()
    };

    let reaction = MySqlStoredProcReaction::builder("routing-e2e")
        .with_query("routed-query")
        .with_query("default-query")
        .with_config(reaction_config)
        .build()
        .await
        .expect("build");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "routed-query",
            vec![ResultDiff::Add {
                data: json!({"id": 1, "name": "Alice", "email": "alice@example.com"}),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue routed");

    reaction
        .enqueue_query_result(make_query_result(
            "default-query",
            vec![ResultDiff::Add {
                data: json!({"id": 2, "name": "Bob", "email": "bob@example.com"}),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue default");

    wait_for_count(&config, 2, 5000).await;
    reaction.stop().await.expect("stop");

    let log = entries(&config).await;
    assert_eq!(log.len(), 2);
    // routed-query used the update_user procedure (operation logged as 'update').
    let routed = log.iter().find(|e| e.user_id == Some(1)).unwrap();
    assert_eq!(routed.operation, "update");
    assert_eq!(routed.user_name.as_deref(), Some("Alice"));
    // default-query used add_user (operation logged as 'add').
    let defaulted = log.iter().find(|e| e.user_id == Some(2)).unwrap();
    assert_eq!(defaulted.operation, "add");
    assert_eq!(defaulted.user_name.as_deref(), Some("Bob"));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_route_matches_last_dotted_segment() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_procedures(&config).await;

    // Route keyed by the last segment `my_query` matches `source.my_query`.
    let mut routes = HashMap::new();
    routes.insert("my_query".to_string(), add_user_template("add_user"));

    let reaction_config = MySqlStoredProcReactionConfig {
        hostname: config.host.clone(),
        port: Some(config.port),
        user: config.user.clone(),
        password: config.password.clone(),
        database: config.database.clone(),
        routes,
        ..Default::default()
    };

    let reaction = MySqlStoredProcReaction::builder("routing-segment-e2e")
        .with_query("source.my_query")
        .with_config(reaction_config)
        .build()
        .await
        .expect("build");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "source.my_query",
            vec![ResultDiff::Add {
                data: json!({"id": 7, "name": "Carol", "email": "carol@example.com"}),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue");

    wait_for_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let log = entries(&config).await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].user_id, Some(7));
    assert_eq!(log[0].user_name.as_deref(), Some("Carol"));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_parameter_binding_preserves_value_types() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_procedures(&config).await;

    let reaction_config = MySqlStoredProcReactionConfig {
        hostname: config.host.clone(),
        port: Some(config.port),
        user: config.user.clone(),
        password: config.password.clone(),
        database: config.database.clone(),
        default_template: Some(add_user_template("add_user")),
        ..Default::default()
    };

    let reaction = MySqlStoredProcReaction::builder("binding-e2e")
        .with_query("q")
        .with_config(reaction_config)
        .build()
        .await
        .expect("build");
    reaction.start().await.expect("start");

    // A numeric id and a name containing SQL metacharacters must be bound
    // positionally, not substituted into the SQL string.
    reaction
        .enqueue_query_result(make_query_result(
            "q",
            vec![ResultDiff::Add {
                data: json!({
                    "id": 12345,
                    "name": "Robert'); DROP TABLE users;--",
                    "email": "injection@example.com"
                }),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue");

    wait_for_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let log = entries(&config).await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].user_id, Some(12345));
    // The literal string is stored verbatim — proof it was bound, not interpolated.
    assert_eq!(
        log[0].user_name.as_deref(),
        Some("Robert'); DROP TABLE users;--")
    );

    mysql.cleanup().await;
}
