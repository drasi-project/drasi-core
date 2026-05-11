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

//! Integration tests for Shell reaction
//!
//! These tests validate that the Shell reaction correctly processes and reacts to results from
//! the continuous queries.
//!
//! # Running tests
//!
//! ```bash
//! cargo test -p drasi-reaction-shell --test integration_tests -- --ignored --nocapture
//! ```
//!
//! The tests are ignored by default.

use anyhow::Result;
use drasi_lib::channels::ResultDiff;
use drasi_lib::identity::{self, IdentityProvider};
use drasi_lib::{identity::PasswordIdentityProvider, DrasiLib, Query, Source};

use drasi_reaction_shell::{ShellCommand, ShellReactionBuilder, ShellReactionConfig};
use drasi_source_http::{HttpSource, HttpSourceConfig};
use log::info;
use serial_test::serial;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::{thread::sleep, time::Duration};
use tokio::time::Instant;

macro_rules! wait_for_startup {
    ($core:expr, $reaction_id:expr, $source_id:expr) => {
        $core.start().await?;

        wait_for_source_status(&$core, $source_id, drasi_lib::ComponentStatus::Running).await?;

        wait_for_reaction_status(&$core, $reaction_id, drasi_lib::ComponentStatus::Running).await?;
    };
}

fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

fn slot_name() -> String {
    format!("drasi_slot_{}", uuid::Uuid::new_v4().simple())
}

async fn wait_for_reaction_status(
    core: &Arc<DrasiLib>,
    reaction_id: &str,
    expected_status: drasi_lib::ComponentStatus,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        let status = core.get_reaction_status(reaction_id).await?;
        if status == expected_status {
            info!("Reaction {reaction_id} reached expected status: {status:?}");
            return Ok(());
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for reaction `{reaction_id}` to reach status {expected_status:?}; current status: {status:?}"
            )
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_for_source_status(
    core: &Arc<DrasiLib>,
    source_id: &str,
    expected_status: drasi_lib::ComponentStatus,
) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        let status = core.get_source_status(source_id).await?;
        if status == expected_status {
            info!("Source {source_id} reached expected status: {status:?}");
            return Ok(());
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Timed out waiting for source `{source_id}` to reach status {expected_status:?}; current status: {status:?}"
            )
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn build_core(
    reaction_config: ShellReactionConfig,
    slot_name: String,
) -> Result<Arc<DrasiLib>> {
    let http_source = HttpSource::builder("test-source")
        .with_host("0.0.0.0")
        .with_port(9000)
        .build()?;

    let query = Query::cypher("test-query")
        .query(
            r#"
            MATCH (d:devices)
            RETURN d.id AS id,
                   d.temperature AS temperature,
                   d.location AS location
        "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();

    let shell_reaction = ShellReactionBuilder::new(slot_name)
        .with_queries(vec!["test-query".to_string()])
        .with_config(reaction_config)
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("mqtt-test-core")
            .with_query(query)
            .with_source(http_source)
            .with_reaction(shell_reaction)
            .build()
            .await?,
    );
    Ok(core)
}

fn operations_script_path() -> String {
    format!("{}/tests/operations.sh", env!("CARGO_MANIFEST_DIR"))
}

async fn send_device_event(
    port: u16,
    source_id: &str,
    device_id: &str,
    temperature: f64,
    location: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "operation": "insert",
        "element": {
            "type": "node",
            "id": device_id,
            "labels": ["devices"],
            "properties": {
                "id": device_id,
                "temperature": temperature,
                "location": location
            }
        }
    });

    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/sources/{source_id}/events"
        ))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await?;

    anyhow::ensure!(
        response.status().is_success(),
        "HTTP source rejected event: status {}",
        response.status()
    );
    Ok(())
}

async fn wait_for_invocation(
    core: &Arc<DrasiLib>,
    reaction_id: &str,
    timeout_duration: Duration,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    loop {
        let info = core.get_reaction_info(reaction_id).await?;
        if let Some(arr) = info
            .properties
            .get("recent_invocations")
            .and_then(|v| v.as_array())
        {
            if !arr.is_empty() {
                return Ok(arr[0].clone());
            }
        }

        if start.elapsed() > timeout_duration {
            anyhow::bail!("Timed out waiting for an invocation on reaction `{reaction_id}`");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_shell_reaction_startup() -> Result<()> {
    init_logging();

    let slot_name = slot_name();
    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: "/bin/sh".to_string(),
                args: vec!["-c".to_string(), "cat".to_string()],
            },
        )]),
        ..Default::default()
    };

    let core = build_core(reaction_config, slot_name.clone()).await?;

    wait_for_startup!(core, &slot_name, "test-source");

    return Ok(());
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_shell_reaction_processing() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path();
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    // generate the reaction config
    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([("STDIN_ENV_VAR".to_string(), "true".to_string())]),
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    // send a device event to drasi via the http source.
    send_device_event(9000, "test-source", "device-1", 72.5, "room-1").await?;

    // get the invocation details
    let invocation =
        wait_for_invocation(&core, &shell_reaction_slot_name, Duration::from_secs(10)).await?;

    let exit_status = invocation["exit_status"].as_i64().unwrap_or(-1);
    assert_eq!(exit_status, 0, "operations.sh should exit with 0");

    let stdout = invocation["stdout"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(
        !stdout.is_empty(),
        "operations.sh should produce output on stdout"
    );
    assert!(
        stdout.contains("device-1"),
        "stdout should contain the device id from the query result, got: {stdout}"
    );

    core.stop().await?;
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
async fn test_shell_script_without_stdin_env() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    let script_path = operations_script_path();
    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: "/bin/sh".to_string(),
                args: vec![script_path],
            },
        )]),
        env: HashMap::from([("STDIN_ENV_VAR".to_string(), "false".to_string())]),
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_event(9000, "test-source", "device-2", 20.0, "garage").await?;

    let invocation =
        wait_for_invocation(&core, &shell_reaction_slot_name, Duration::from_secs(10)).await?;

    let exit_status = invocation["exit_status"].as_i64().unwrap_or(-1);
    assert_eq!(exit_status, 0, "operations.sh should exit with 0");

    let stdout = invocation["stdout"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(
        stdout.contains("No input received"),
        "stdout should contain the fallback message, got: {stdout}"
    );

    core.stop().await?;
    Ok(())
}
