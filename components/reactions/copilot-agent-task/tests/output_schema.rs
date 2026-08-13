// Copyright 2026 The Drasi Authors.
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

//! Regression test keeping `schema/*.schema.json` in lock-step with the
//! Rust functions they are generated from (`src/prompt.rs`).
//!
//! Set `DRASI_UPDATE_SCHEMA=1` to regenerate the committed files instead of
//! asserting equality; `make update-schema` wraps this.

use std::path::PathBuf;

use drasi_reaction_copilot_agent_task::prompt::{
    work_graph_event_v1_schema, workgraph_execution_v1_schema,
};
use drasi_reaction_copilot_agent_task::state::workgraph_execution_state_v1_schema;

fn schema_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join(name)
}

fn fixture(name: &str) -> serde_json::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()))
}

fn assert_exact_required_fields(schema: &serde_json::Value, value: &serde_json::Value) {
    let required: std::collections::BTreeSet<&str> = schema["required"]
        .as_array()
        .expect("schema required is an array")
        .iter()
        .map(|field| field.as_str().expect("required field is a string"))
        .collect();
    let actual: std::collections::BTreeSet<&str> = value
        .as_object()
        .expect("fixture is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(actual, required);
}

fn pretty_json(value: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(value).expect("schema must serialize");
    s.push('\n');
    s
}

fn assert_in_sync(name: &str, generated: &serde_json::Value) {
    let path = schema_path(name);
    let rendered = pretty_json(generated);

    if std::env::var("DRASI_UPDATE_SCHEMA").ok().as_deref() == Some("1") {
        std::fs::create_dir_all(path.parent().expect("schema path has a parent"))
            .expect("create schema dir");
        std::fs::write(&path, &rendered).expect("write regenerated schema file");
        eprintln!("regenerated {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {} ({e}). Run `make update-schema` (or set DRASI_UPDATE_SCHEMA=1) to generate the file.",
            path.display()
        )
    });
    assert_eq!(
        committed,
        rendered,
        "{} is out of sync with src/prompt.rs — run `make update-schema`",
        path.display()
    );
}

#[test]
fn work_graph_event_v1_schema_file_is_in_sync() {
    assert_in_sync(
        "workgraph-event-v1.schema.json",
        &work_graph_event_v1_schema(),
    );
}

#[test]
fn workgraph_execution_v1_schema_file_is_in_sync() {
    assert_in_sync(
        "workgraph-execution-v1.schema.json",
        &workgraph_execution_v1_schema(),
    );
}

#[test]
fn workgraph_execution_state_v1_schema_file_is_in_sync() {
    assert_in_sync(
        "workgraph-execution-state-v1.schema.json",
        &workgraph_execution_state_v1_schema(),
    );
}

#[test]
fn canonical_workgraph_event_fixture_matches_schema_contract() {
    let schema = work_graph_event_v1_schema();
    let event = fixture("workgraph-event-completed.json");
    assert_exact_required_fields(&schema, &event);
    assert_eq!(event["schemaVersion"], "workgraph.event/v1");
    assert_eq!(event["eventType"], "CompletedIssueValidation");
    assert_eq!(event["subjectType"], "Issue");
    assert_eq!(event["result"]["outcome"], "passed");
    assert_eq!(
        event["result"]["evidence"]["requiredMarker"],
        "WorkGraph-Validation: pass"
    );
}

#[test]
fn canonical_execution_fixture_matches_schema_contract() {
    let schema = workgraph_execution_v1_schema();
    let execution = fixture("workgraph-execution-started.json");
    assert_exact_required_fields(&schema, &execution);
    assert_eq!(execution["schemaVersion"], "workgraph.execution/v1");
    assert_eq!(execution["messageType"], "execution");
    assert_eq!(execution["requiredEventType"], "CompletedIssueValidation");
    assert_eq!(execution["state"], "started");
    assert_eq!(execution["requestedModel"], "gpt-5.6-sol");
    assert_eq!(execution["actualModel"], "gpt-5.4");
}
