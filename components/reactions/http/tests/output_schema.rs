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

//! Regression test that keeps `schema/output.schema.json` in lock-step
//! with the Rust types it is generated from. Re-renders the schema from
//! [`HttpReactionOutputSchemas`] and asserts equality with the committed
//! file.
//!
//! When the env var `DRASI_UPDATE_SCHEMA=1` is set, the test instead
//! **rewrites** the committed file with the freshly generated schema
//! and succeeds. This is the supported regeneration path; the
//! `make update-schema` target wraps it.

use std::path::PathBuf;

use drasi_reaction_http::descriptor::HttpReactionOutputSchemas;

fn schema_path() -> PathBuf {
    // CARGO_MANIFEST_DIR points at components/reactions/http during test runs.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join("output.schema.json")
}

fn pretty_json(value: &serde_json::Value) -> String {
    // Trailing newline so the file matches typical editor / formatter output.
    let mut s = serde_json::to_string_pretty(value).expect("schema must serialize");
    s.push('\n');
    s
}

#[test]
fn output_schema_file_is_in_sync_with_types() {
    let generated = HttpReactionOutputSchemas::as_json_schema();
    let path = schema_path();

    if std::env::var("DRASI_UPDATE_SCHEMA").ok().as_deref() == Some("1") {
        std::fs::create_dir_all(path.parent().unwrap()).expect("could not create schema directory");
        std::fs::write(&path, pretty_json(&generated))
            .expect("could not write regenerated schema file");
        eprintln!("regenerated {}", path.display());
        return;
    }

    let committed_text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {} ({e}). Run `make update-schema` (or set \
             DRASI_UPDATE_SCHEMA=1) to generate the file.",
            path.display()
        )
    });
    let committed: serde_json::Value =
        serde_json::from_str(&committed_text).expect("committed schema must be valid JSON");

    assert_eq!(
        committed,
        generated,
        "{} is out of sync with the Rust types in src/output.rs. \
         Run `make update-schema` to regenerate it.",
        path.display(),
    );
}
