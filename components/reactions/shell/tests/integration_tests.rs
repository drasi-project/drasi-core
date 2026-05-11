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
use std::sync::Arc;
use std::{thread::sleep, time::Duration};
use tokio::time::Instant;

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

#[tokio::test]
#[serial]
#[ignore]
async fn test_mqtt_publisher_client_connection() -> Result<()> {
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

    wait_for_source_status(&core, "test-source", drasi_lib::ComponentStatus::Running).await?;

    wait_for_reaction_status(&core, &slot_name, drasi_lib::ComponentStatus::Running).await?;

    return Ok(());
}
