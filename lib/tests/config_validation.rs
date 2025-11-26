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

//! Config validation tests
//!
//! Validates that all example config files can be loaded successfully

use drasi_lib::config::DrasiLibConfig;
use std::path::{Path, PathBuf};

/// Load and validate a config file
fn validate_config_file(path: &Path) -> Result<(), String> {
    println!("Validating: {}", path.display());

    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;

    let config: DrasiLibConfig = if path.extension().and_then(|s| s.to_str()) == Some("json")
    {
        serde_json::from_str(&content).map_err(|e| format!("JSON parsing error: {}", e))?
    } else {
        serde_yaml::from_str(&content).map_err(|e| format!("YAML parsing error: {}", e))?
    };

    // Validate server ID is present
    if config.server_core.id.is_empty() {
        return Err("Server ID is empty - must be provided".to_string());
    }

    // Validate sources
    for source in &config.sources {
        if source.id.is_empty() {
            return Err(format!("Source has empty ID"));
        }
        if source.source_type().is_empty() {
            return Err(format!("Source '{}' has empty source_type", source.id));
        }
    }

    // Validate queries
    for query in &config.queries {
        if query.id.is_empty() {
            return Err(format!("Query has empty ID"));
        }
        if query.query.is_empty() {
            return Err(format!("Query '{}' has empty query string", query.id));
        }
        // Note: source_subscriptions is now optional (defaults to empty vec)
        // Old config format may not have this field - that's OK for validation
    }

    // Validate reactions
    for reaction in &config.reactions {
        if reaction.id.is_empty() {
            return Err(format!("Reaction has empty ID"));
        }
        if reaction.reaction_type().is_empty() {
            return Err(format!(
                "Reaction '{}' has empty reaction_type",
                reaction.id
            ));
        }
        if reaction.queries.is_empty() {
            return Err(format!("Reaction '{}' has no queries", reaction.id));
        }
    }

    println!("✓ Valid: {}", path.display());
    Ok(())
}

#[test]
fn test_all_example_configs() {
    let examples_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");

    // Find all YAML and JSON config files
    let mut config_files = Vec::new();
    find_config_files(&examples_dir, &mut config_files);

    assert!(
        !config_files.is_empty(),
        "No config files found in examples directory"
    );

    let mut errors = Vec::new();

    for config_file in &config_files {
        if let Err(e) = validate_config_file(config_file) {
            errors.push(format!("{}: {}", config_file.display(), e));
        }
    }

    if !errors.is_empty() {
        eprintln!("\n❌ Config validation failures:\n");
        for error in &errors {
            eprintln!("  - {}", error);
        }
        panic!("\n{} config file(s) failed validation", errors.len());
    }

    println!(
        "\n✓ All {} config files validated successfully",
        config_files.len()
    );
}

fn find_config_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();

            // Skip target directories and hidden files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') || name == "target" {
                    continue;
                }
            }

            if path.is_dir() {
                find_config_files(&path, files);
            } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if ext == "yaml" || ext == "json" {
                    // Skip if it's in a target directory (build artifacts)
                    if !path.to_string_lossy().contains("/target/") {
                        files.push(path);
                    }
                }
            }
        }
    }
}

#[test]
fn test_basic_yaml_loads() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/basic_integration/config/basic.yaml");

    if config_path.exists() {
        validate_config_file(&config_path).expect("basic.yaml should be valid");
    }
}

#[test]
fn test_basic_json_loads() {
    let config_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/basic_integration/config/basic.json");

    if config_path.exists() {
        validate_config_file(&config_path).expect("basic.json should be valid");
    }
}
