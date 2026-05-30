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

mod shell_helpers;

use anyhow::Result;
use shell_helpers::*;

use drasi_reaction_shell::{ShellCommand, ShellReactionConfig};
use serial_test::serial;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::{thread::sleep, time::Duration};

macro_rules! wait_for_startup {
    ($core:expr, $reaction_id:expr, $source_id:expr) => {
        $core.start().await?;

        wait_for_source_status(&$core, $source_id, drasi_lib::ComponentStatus::Running).await?;

        wait_for_reaction_status(&$core, $reaction_id, drasi_lib::ComponentStatus::Running).await?;
    };
}

#[tokio::test]
#[serial]
#[ignore]
/// Validates that the shell reaction starts up correctly.
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
/// Validates that the shell reaction processes events correctly.
async fn test_shell_reaction_processing() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test1");
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
    send_device_insert_event(9000, "test-source", "device-1", 72.5, "room-1").await?;

    // get the invocation details
    tokio::time::sleep(Duration::from_secs(2)).await;
    let invocation = get_invocation_details(&core, &shell_reaction_slot_name).await?;

    assert_eq!(
        invocation.len(),
        1,
        "there should be one invocation of the script, got {}",
        invocation.len()
    );

    let exit_status = invocation[0]["exit_status"].as_i64().unwrap_or(-1);
    assert_eq!(exit_status, 0, "operations.sh should exit with 0");

    let stdout = invocation[0]["stdout"]
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
// validate that the shell reaction fails on unexecutable script.
async fn test_shell_reaction_unexecutable_script() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();
    let script_path = operations_script_path("test2");

    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o644))?; // unexecutable

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        ..Default::default()
    };

    let result = build_core(reaction_config, shell_reaction_slot_name.clone()).await;

    // check the error message
    assert!(
        result.is_err()
            && result
                .err()
                .unwrap_or_else(|| anyhow::anyhow!("Unknown error"))
                .to_string()
                .contains("test2.sh' is not executable"),
    );
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// validate that the shell reaction can read multiple writes from the same script
async fn test_shell_reaction_script_multiple_writes() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test3");
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([
            ("RUN_TIMEOUT_TEST".to_string(), "true".to_string()),
            ("SENSOR_ID".to_string(), "{{after.id}}".to_string()),
            ("SENSOR_TEMP".to_string(), "{{after.temp}}".to_string()),
            ("SENSOR_LOCATION".to_string(), "{{after.loc}}".to_string()),
            ("SENSOR_STDIN".to_string(), "true".to_string()),
        ]),
        timeout_s: 20,
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_insert_event(9000, "test-source", "device-1", 30.0, "room-34").await?;

    // 13 secs to wait for the test3 to finish
    tokio::time::sleep(Duration::from_secs(13)).await;
    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;

    let stdout = invocations[0]["stdout"].as_str().unwrap_or("").to_string();

    let exit_status = invocations[0]["exit_status"].as_i64().unwrap_or(-1);

    assert_eq!(
        stdout, "device-1\n30.0\ntrue\nroom-34\n",
        "stdout should match the expected output"
    );

    assert_eq!(exit_status, 0, "operations.sh should exit with 0");

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// validate that the shell reaction respects the max_recent_invocations limit and only keeps the most recent invocations
async fn test_shell_reaction_limited_invocations_size() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test3");
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([
            ("RUN_TIMEOUT_TEST".to_string(), "true".to_string()),
            ("SENSOR_ID".to_string(), "{{after.id}}".to_string()),
            ("SENSOR_TEMP".to_string(), "{{after.temp}}".to_string()),
            ("SENSOR_LOCATION".to_string(), "{{after.loc}}".to_string()),
            ("SENSOR_STDIN".to_string(), "true".to_string()),
        ]),
        timeout_s: 20,
        max_recent_invocations: 2,
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_insert_event(9000, "test-source", "device-1", 30.0, "room-34").await?;
    send_device_insert_event(9000, "test-source", "device-2", 31.0, "room-35").await?;
    send_device_insert_event(9000, "test-source", "device-3", 32.0, "room-36").await?;
    send_device_insert_event(9000, "test-source", "device-4", 33.0, "room-37").await?;

    tokio::time::sleep(Duration::from_secs(13)).await;
    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;

    let length_of_invocations = invocations.len();
    assert_eq!(
        length_of_invocations, 2,
        "there should be 2 invocations of the script, got {length_of_invocations}"
    );

    let stdout_1 = invocations[0]["stdout"].as_str().unwrap_or("").to_string();
    let stdout_2 = invocations[1]["stdout"].as_str().unwrap_or("").to_string();

    assert_eq!(
        stdout_1, "device-4\n33.0\ntrue\nroom-37\n",
        "stdout of invocation 1 should match the expected output"
    );

    assert_eq!(
        stdout_2, "device-3\n32.0\ntrue\nroom-36\n",
        "stdout of invocation 2 should match the expected output"
    );

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// validate that the shell reaction kills executions that exceed the timeout duration.
async fn test_shell_reaction_execution_timeout() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test3");
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([
            ("RUN_TIMEOUT_TEST".to_string(), "false".to_string()),
            ("SENSOR_ID".to_string(), "{{after.id}}".to_string()),
            ("SENSOR_TEMP".to_string(), "{{after.temp}}".to_string()),
            ("SENSOR_LOCATION".to_string(), "{{after.loc}}".to_string()),
            ("SENSOR_STDIN".to_string(), "true".to_string()),
        ]),
        timeout_s: 20,
        max_recent_invocations: 2,
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_insert_event(9000, "test-source", "device-1", 30.0, "room-34").await?;
    send_device_insert_event(9000, "test-source", "device-2", 31.0, "room-35").await?;
    send_device_insert_event(9000, "test-source", "device-3", 32.0, "room-36").await?;
    send_device_insert_event(9000, "test-source", "device-4", 33.0, "room-37").await?;

    tokio::time::sleep(Duration::from_secs(25)).await;
    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;

    let len = invocations.len();
    assert_eq!(
        len, 0,
        "there should be 0 invocations of the script, got {len}"
    );

    let properties = get_reaction_properties(&core, &shell_reaction_slot_name).await?;
    let number_of_timeouts = properties
        .get("timeout_firings")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    assert_eq!(
        number_of_timeouts, 4,
        "there should be 4 timeouts for the 4 script executions, got {number_of_timeouts}"
    );

    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// validate that the shell reaction respects the concurrent execution limit.
async fn test_shell_reaction_concurrent_execution_limit() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test3");
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([
            ("RUN_TIMEOUT_TEST".to_string(), "true".to_string()),
            ("SENSOR_ID".to_string(), "{{after.id}}".to_string()),
            ("SENSOR_TEMP".to_string(), "{{after.temp}}".to_string()),
            ("SENSOR_LOCATION".to_string(), "{{after.loc}}".to_string()),
            ("SENSOR_STDIN".to_string(), "true".to_string()),
        ]),
        timeout_s: 20,
        max_recent_invocations: 3,
        max_concurrent: 2,
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_insert_event(9000, "test-source", "device-1", 30.0, "room-34").await?;
    send_device_insert_event(9000, "test-source", "device-2", 31.0, "room-35").await?;
    send_device_insert_event(9000, "test-source", "device-3", 32.0, "room-36").await?;
    send_device_insert_event(9000, "test-source", "device-4", 33.0, "room-37").await?;

    tokio::time::sleep(Duration::from_secs(9)).await; // wait for the first two executions to start

    let properties = get_reaction_properties(&core, &shell_reaction_slot_name).await?;
    let number_of_active_processes = properties
        .get("active_processes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    assert_eq!(
        number_of_active_processes, 2,
        "there should be 2 active processes for the 4 script executions, got {number_of_active_processes}"
    );

    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;
    let len = invocations.len();
    assert_eq!(
        len, 0,
        "there should be 0 invocations of the script, got {len}"
    );

    tokio::time::sleep(Duration::from_secs(12)).await; // wait for the first two executions to finish and the next two to start
    let number_of_active_processes_after =
        get_reaction_properties(&core, &shell_reaction_slot_name)
            .await?
            .get("active_processes")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    assert_eq!(
        number_of_active_processes_after, 2,
        "there should be 2 active processes for the 4 script executions after the first two finish, got {number_of_active_processes_after}"
    );

    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;
    let len = invocations.len();
    assert_eq!(
        len, 2,
        "there should be 2 invocations of the script, got {len}"
    );

    let first_stdout = invocations[0]["stdout"].as_str().unwrap_or("").to_string();
    let second_stdout = invocations[1]["stdout"].as_str().unwrap_or("").to_string();

    assert_eq!(
        first_stdout, "device-2\n31.0\ntrue\nroom-35\n",
        "stdout of invocation 1 should match the expected output"
    );

    assert_eq!(
        second_stdout, "device-1\n30.0\ntrue\nroom-34\n",
        "stdout of invocation 2 should match the expected output"
    );

    // wait for all executions to finish
    tokio::time::sleep(Duration::from_secs(10)).await;
    let number_of_active_processes_end = get_reaction_properties(&core, &shell_reaction_slot_name)
        .await?
        .get("active_processes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(
        number_of_active_processes_end, 0,
        "there should be 0 active processes after all executions finish, got {number_of_active_processes_end}"
    );

    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;
    let len = invocations.len();
    assert_eq!(
        len, 3,
        "there should be 3 invocations of the script, got {len}"
    );

    let stdout_1 = invocations[0]["stdout"].as_str().unwrap_or("").to_string();
    let stdout_2 = invocations[1]["stdout"].as_str().unwrap_or("").to_string();
    let stdout_3 = invocations[2]["stdout"].as_str().unwrap_or("").to_string();

    assert_eq!(
        stdout_1, "device-4\n33.0\ntrue\nroom-37\n",
        "stdout of invocation 1 should match the expected output"
    );
    assert_eq!(
        stdout_2, "device-3\n32.0\ntrue\nroom-36\n",
        "stdout of invocation 2 should match the expected output"
    );
    assert_eq!(
        stdout_3, "device-2\n31.0\ntrue\nroom-35\n",
        "stdout of invocation 3 should match the expected output"
    );
    Ok(())
}

#[tokio::test]
#[serial]
#[ignore]
// validate that the shell reaction accepts the failure of the script and adds it to the invocation details without crashing.
async fn test_shell_reaction_script_failure() -> Result<()> {
    init_logging();

    let shell_reaction_slot_name = slot_name();

    // get the script path
    let script_path = operations_script_path("test2");
    // make the script executable
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;

    let reaction_config = ShellReactionConfig {
        commands: HashMap::from([(
            "test-query".to_string(),
            ShellCommand {
                executable: script_path.clone(),
                args: vec![],
            },
        )]),
        env: HashMap::from([("FAIL_EXIT".to_string(), "true".to_string())]),
        timeout_s: 20,
        ..Default::default()
    };

    let core = build_core(reaction_config, shell_reaction_slot_name.clone()).await?;
    wait_for_startup!(core, &shell_reaction_slot_name, "test-source");

    send_device_insert_event(9000, "test-source", "device-1", 30.0, "room-34").await?;

    // 13 secs to wait for the test2 to finish
    tokio::time::sleep(Duration::from_secs(1)).await;
    let invocations = get_invocation_details(&core, &shell_reaction_slot_name).await?;

    let exit_status = invocations[0]["exit_status"].as_i64().unwrap_or(0);
    assert_eq!(exit_status, 1, "operations.sh should exit with 1");

    let stdout = invocations[0]["stdout"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(
        stdout.contains("Failing with status 1 as FAIL_EXIT is set to true"),
        "stdout should contain the failure message from the script, got: {stdout}"
    );

    let properties = get_reaction_properties(&core, &shell_reaction_slot_name).await?;
    let non_zero_exits = properties
        .get("non_zero_exits")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    assert_eq!(
        non_zero_exits, 1,
        "there should be 1 non-zero exit for the script execution, got {non_zero_exits}"
    );

    Ok(())
}
