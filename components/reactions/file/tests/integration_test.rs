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

use anyhow::{anyhow, Result};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_file::{FileReaction, QueryConfig, TemplateSpec, WriteMode};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

async fn wait_for_file(path: &Path, timeout: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await?;
            return Ok(content);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!("Timed out waiting for file '{}'", path.display()))
}

async fn wait_for_line_count(
    path: &Path,
    expected_lines: usize,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await?;
            if content.lines().count() >= expected_lines {
                return Ok(content);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!(
        "Timed out waiting for file '{}' to reach {} lines",
        path.display(),
        expected_lines
    ))
}

async fn setup_drasi_for_mode(
    output_dir: &str,
    write_mode: WriteMode,
    filename_template: &str,
    default_template: Option<QueryConfig>,
) -> Result<(DrasiLib, drasi_source_application::ApplicationSourceHandle)> {
    let (app_source, app_handle) = ApplicationSource::new(
        "test-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;

    let mut builder = FileReaction::builder("file-reaction")
        .with_query("test-query")
        .with_output_path(output_dir)
        .with_write_mode(write_mode)
        .with_filename_template(filename_template);

    if let Some(template) = default_template {
        builder = builder.with_default_template(template);
    }

    let file_reaction = builder.build()?;

    let query = Query::cypher("test-query")
        .query("MATCH (n:Item) RETURN n.id AS id, n.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let drasi = DrasiLib::builder()
        .with_id("file-reaction-integration")
        .with_source(app_source)
        .with_query(query)
        .with_reaction(file_reaction)
        .build()
        .await?;

    Ok((drasi, app_handle))
}

#[tokio::test]
#[ignore]
async fn test_file_reaction_append_insert_update_delete() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(
            r#"{"op":"add","id":"{{after.id}}","name":"{{after.name}}"}"#,
        )),
        updated: Some(TemplateSpec::new(
            r#"{"op":"update","before":"{{before.name}}","after":"{{after.name}}"}"#,
        )),
        deleted: Some(TemplateSpec::new(
            r#"{"op":"delete","id":"{{before.id}}","name":"{{before.name}}"}"#,
        )),
    };

    let (drasi, app_handle) = setup_drasi_for_mode(
        &output_dir,
        WriteMode::Append,
        "{{query_name}}.ndjson",
        Some(default_template),
    )
    .await?;

    drasi.start().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    app_handle
        .send_node_insert(
            "item-1",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-1")
                .with_string("name", "Widget")
                .build(),
        )
        .await?;

    app_handle
        .send_node_update(
            "item-1",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-1")
                .with_string("name", "Widget Pro")
                .build(),
        )
        .await?;

    app_handle.send_delete("item-1", vec!["Item"]).await?;

    let output_file = temp.path().join("test-query.ndjson");
    let content = wait_for_line_count(&output_file, 3, Duration::from_secs(10)).await?;
    let lines: Vec<&str> = content.lines().collect();
    assert!(lines.iter().any(|l| l.contains(r#""op":"add""#)));
    assert!(lines.iter().any(|l| l.contains(r#""op":"update""#)));
    assert!(lines.iter().any(|l| l.contains(r#""op":"delete""#)));

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_file_reaction_overwrite_mode_keeps_latest() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(r#"{"op":"add","name":"{{after.name}}"}"#)),
        updated: Some(TemplateSpec::new(
            r#"{"op":"update","name":"{{after.name}}"}"#,
        )),
        deleted: Some(TemplateSpec::new(
            r#"{"op":"delete","name":"{{before.name}}"}"#,
        )),
    };

    let (drasi, app_handle) = setup_drasi_for_mode(
        &output_dir,
        WriteMode::Overwrite,
        "snapshot.json",
        Some(default_template),
    )
    .await?;

    drasi.start().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    app_handle
        .send_node_insert(
            "item-2",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-2")
                .with_string("name", "Start")
                .build(),
        )
        .await?;

    app_handle
        .send_node_update(
            "item-2",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-2")
                .with_string("name", "Latest")
                .build(),
        )
        .await?;

    let output_file = temp.path().join("snapshot.json");
    let content = wait_for_file(&output_file, Duration::from_secs(10)).await?;
    assert!(content.contains(r#""name":"Latest""#));

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_file_reaction_per_change_and_payload_filename() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(r#"{"id":"{{after.id}}"}"#)),
        updated: None,
        deleted: None,
    };

    let (drasi, app_handle) = setup_drasi_for_mode(
        &output_dir,
        WriteMode::PerChange,
        "item_{{after.id}}_{{operation}}.json",
        Some(default_template),
    )
    .await?;

    drasi.start().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    app_handle
        .send_node_insert(
            "item-3",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "a/b")
                .with_string("name", "Slash Name")
                .build(),
        )
        .await?;

    let output_file = temp.path().join("item_a_b_ADD.json");
    let content = wait_for_file(&output_file, Duration::from_secs(10)).await?;
    assert!(content.contains(r#""id":"a/b""#));

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_file_reaction_fallback_raw_json() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let (drasi, app_handle) =
        setup_drasi_for_mode(&output_dir, WriteMode::Append, "{{query_name}}.log", None).await?;

    drasi.start().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    app_handle
        .send_node_insert(
            "item-4",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-4")
                .with_string("name", "Raw")
                .build(),
        )
        .await?;

    let output_file = temp.path().join("test-query.log");
    let content = wait_for_file(&output_file, Duration::from_secs(10)).await?;
    assert!(content.contains("\"ADD\"") && content.contains("\"id\""));

    drasi.stop().await?;
    Ok(())
}
