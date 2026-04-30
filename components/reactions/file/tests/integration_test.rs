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

async fn wait_for_content(path: &Path, needle: &str, timeout: Duration) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await?;
            if content.contains(needle) {
                return Ok(content);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(anyhow!(
        "Timed out waiting for file '{}' to contain '{}'",
        path.display(),
        needle,
    ))
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
async fn test_file_reaction_fallback_raw_json() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let (drasi, app_handle) =
        setup_drasi_for_mode(&output_dir, WriteMode::Append, "{{query_name}}.log", None).await?;

    drasi.start().await?;

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

#[tokio::test]
async fn test_file_reaction_aggregation_uses_updated_template() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let (app_source, app_handle) = ApplicationSource::new(
        "test-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
        },
    )?;

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(
            r#"{"op":"add","count":{{after.item_count}}}"#,
        )),
        updated: Some(TemplateSpec::new(
            r#"{"op":"update","before_count":{{before.item_count}},"after_count":{{after.item_count}}}"#,
        )),
        deleted: Some(TemplateSpec::new(
            r#"{"op":"delete","count":{{before.item_count}}}"#,
        )),
    };

    let file_reaction = FileReaction::builder("file-reaction")
        .with_query("agg-query")
        .with_output_path(&output_dir)
        .with_write_mode(WriteMode::Append)
        .with_filename_template("{{query_name}}.ndjson")
        .with_default_template(default_template)
        .build()?;

    let query = Query::cypher("agg-query")
        .query("MATCH (n:Item) RETURN n.category AS category, count(n) AS item_count")
        .from_source("test-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let drasi = DrasiLib::builder()
        .with_id("file-reaction-agg-test")
        .with_source(app_source)
        .with_query(query)
        .with_reaction(file_reaction)
        .build()
        .await?;

    drasi.start().await?;

    // First insert produces an aggregation diff (before=None, after={category:"A", item_count:1})
    app_handle
        .send_node_insert(
            "item-1",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-1")
                .with_string("category", "A")
                .build(),
        )
        .await?;

    let output_file = temp.path().join("agg-query.ndjson");
    let content = wait_for_file(&output_file, Duration::from_secs(10)).await?;
    // The first aggregation has no before, so the updated template renders before.item_count as empty.
    // But the after count should be present.
    assert!(
        content.contains("after_count") || content.contains("\"count\""),
        "Expected aggregation output with count, got: {content}"
    );

    // Second insert to same category changes count from 1 to 2
    app_handle
        .send_node_insert(
            "item-2",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("id", "item-2")
                .with_string("category", "A")
                .build(),
        )
        .await?;

    let content = wait_for_line_count(&output_file, 2, Duration::from_secs(10)).await?;
    let lines: Vec<&str> = content.lines().collect();
    let last_line = lines.last().expect("should have at least 2 lines");
    assert!(
        last_line.contains("after_count"),
        "Expected aggregation with updated template, got: {last_line}"
    );

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_file_reaction_append_recovers_from_partial_line() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();
    let output_file = temp.path().join("test-query.ndjson");

    // Pre-seed the output file with a complete line followed by a simulated
    // crash (partial trailing line without a newline terminator).
    tokio::fs::write(
        &output_file,
        b"{\"op\":\"add\",\"id\":\"item-0\"}\npartial crash",
    )
    .await?;

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(
            r#"{"op":"add","id":"{{after.id}}","name":"{{after.name}}"}"#,
        )),
        updated: None,
        deleted: None,
    };

    let (drasi, app_handle) = setup_drasi_for_mode(
        &output_dir,
        WriteMode::Append,
        "{{query_name}}.ndjson",
        Some(default_template),
    )
    .await?;

    drasi.start().await?;

    // Send a new event — the reaction should repair the file first, then append.
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

    let content = wait_for_content(&output_file, "item-1", Duration::from_secs(10)).await?;
    let lines: Vec<&str> = content.lines().collect();

    // The partial "partial crash" line should have been truncated.
    // Line 1 should be the original complete line, line 2 the newly appended line.
    assert_eq!(lines.len(), 2, "Expected exactly 2 lines, got: {content}");
    assert!(
        lines[0].contains("\"item-0\""),
        "First line should be the pre-existing complete line"
    );
    assert!(
        lines[1].contains("\"item-1\""),
        "Second line should be the newly appended event, got: '{}'",
        lines[1]
    );
    assert!(
        !content.contains("partial crash"),
        "Partial line should have been truncated"
    );

    drasi.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_file_reaction_append_fsync_produces_valid_ndjson() -> Result<()> {
    let temp = TempDir::new()?;
    let output_dir = temp.path().to_string_lossy().to_string();

    let default_template = QueryConfig {
        added: Some(TemplateSpec::new(r#"{"op":"add","id":"{{after.id}}"}"#)),
        updated: None,
        deleted: None,
    };

    let (drasi, app_handle) = setup_drasi_for_mode(
        &output_dir,
        WriteMode::Append,
        "{{query_name}}.ndjson",
        Some(default_template),
    )
    .await?;

    drasi.start().await?;

    // Send multiple events.
    for i in 1..=5 {
        let id = format!("item-{i}");
        let name = format!("Widget {i}");
        app_handle
            .send_node_insert(
                id.as_str(),
                vec!["Item"],
                PropertyMapBuilder::new()
                    .with_string("id", &id)
                    .with_string("name", &name)
                    .build(),
            )
            .await?;
    }

    let output_file = temp.path().join("test-query.ndjson");
    let content = wait_for_line_count(&output_file, 5, Duration::from_secs(10)).await?;

    // Verify every line is valid JSON (valid NDJSON).
    for (i, line) in content.lines().enumerate() {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_ok(),
            "Line {i} is not valid JSON: {line}"
        );
    }

    // Verify the file ends with a newline (proper NDJSON termination).
    assert!(
        content.ends_with('\n'),
        "File should end with a newline character"
    );

    drasi.stop().await?;
    Ok(())
}
