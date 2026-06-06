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

#![cfg(target_os = "linux")]

use drasi_lib::{DrasiLib, Query, Source};

use anyhow::Result;
use drasi_reaction_shell::{ShellReactionBuilder, ShellReactionConfig};
use drasi_source_http::HttpSource;
use log::info;
use std::collections::HashMap;
use std::sync::Arc;
use std::{thread::sleep, time::Duration};
use tokio::time::Instant;

pub fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

pub fn slot_name() -> String {
    format!("drasi_slot_{}", uuid::Uuid::new_v4().simple())
}

pub async fn wait_for_reaction_status(
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

pub async fn wait_for_source_status(
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
                   d.temperature AS temp,
                   d.location AS loc
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

pub fn operations_script_path(test_name: &str) -> String {
    let path = format!(
        "{}/tests/scripts/{}.sh",
        env!("CARGO_MANIFEST_DIR"),
        test_name
    );
    path
}

pub async fn send_device_insert_event(
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

pub async fn get_reaction_properties(
    core: &Arc<DrasiLib>,
    reaction_id: &str,
) -> Result<HashMap<String, serde_json::Value>> {
    let info = core.get_reaction_info(reaction_id).await?;
    Ok(info.properties)
}

pub async fn get_invocation_details(
    core: &Arc<DrasiLib>,
    reaction_id: &str,
) -> Result<Vec<serde_json::Value>> {
    let info = core.get_reaction_info(reaction_id).await?;
    if let Some(arr) = info
        .properties
        .get("recent_invocations")
        .and_then(|v| v.as_array())
    {
        return Ok(arr.clone());
    }
    Ok(vec![])
}
