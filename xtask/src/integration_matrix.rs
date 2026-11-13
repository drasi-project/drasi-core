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

//! Discover workspace integration tests that are `#[ignore]`d because they
//! start containers (testcontainers or shared Redis/k3s helpers).
//!
//! New crates are picked up automatically when they add an ignored test that
//! uses those helpers. Live-network / credential tests without container
//! setup are left out.

use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IntegrationTestJob {
    pub name: String,
    pub package: String,
    pub test: String,
    pub extra: String,
    pub timeout: u32,
}

pub fn print_matrix(jobs: &[IntegrationTestJob]) {
    let matrix = serde_json::json!({ "include": jobs });
    println!(
        "{}",
        serde_json::to_string(&matrix).expect("failed to serialize integration test matrix")
    );
}

pub fn discover_jobs(
    packages: impl IntoIterator<Item = (String, PathBuf)>,
) -> Vec<IntegrationTestJob> {
    let mut jobs = Vec::new();
    for (package, dir) in packages {
        for (test, root) in integration_test_targets(&dir) {
            let files = reachable_rust_files(&root);
            if !files.iter().any(|path| {
                fs::read_to_string(path)
                    .map(|src| has_ignored_test(&src))
                    .unwrap_or(false)
            }) {
                continue;
            }
            if files.iter().any(|path| {
                fs::read_to_string(path)
                    .map(|src| {
                        src.contains("DRASI_MSSQL_TEST_MODE")
                            || src.contains("DRASI_NETWORK_CONTAINER")
                    })
                    .unwrap_or(false)
            }) {
                // Credentials/ownership and prebuilt cross-OS peers are not
                // supplied by the automatically provisioned container workflow.
                continue;
            }
            if !files.iter().any(|path| {
                fs::read_to_string(path)
                    .map(|src| uses_containers(&src))
                    .unwrap_or(false)
            }) {
                continue;
            }

            let needs_oracle = package.contains("oracle");
            let needs_plugins = files.iter().any(|path| {
                fs::read_to_string(path)
                    .map(|src| needs_cdylib_plugins(&src))
                    .unwrap_or(false)
            });
            let mut extras = Vec::new();
            if needs_oracle {
                extras.push("oracle");
            }
            if needs_plugins {
                extras.push("plugins");
            }
            if files.iter().any(|path| {
                fs::read_to_string(path)
                    .map(|src| src.contains("\"podman\""))
                    .unwrap_or(false)
            }) {
                extras.push("podman");
            }
            let root_source = fs::read_to_string(&root).expect("read discovered test target");
            for feature in ["computation", "garnet-tests"] {
                if root_source.contains(&format!("#![cfg(feature = \"{feature}\")]")) {
                    extras.push(feature);
                }
            }
            let extra = if extras.is_empty() {
                "none".to_string()
            } else {
                extras.join(",")
            };
            let timeout = if needs_oracle {
                90
            } else if needs_plugins {
                120
            } else {
                60
            };
            jobs.push(IntegrationTestJob {
                name: format!("{package} / {test}"),
                package: package.clone(),
                test,
                extra,
                timeout,
            });
        }
    }
    jobs.sort_by(|a, b| (&a.package, &a.test).cmp(&(&b.package, &b.test)));
    jobs
}

fn integration_test_targets(package_dir: &Path) -> Vec<(String, PathBuf)> {
    let tests_dir = package_dir.join("tests");
    let Ok(entries) = fs::read_dir(&tests_dir) else {
        return Vec::new();
    };

    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            if let Some(stem) = path.file_stem() {
                targets.push((stem.to_string_lossy().into_owned(), path));
            }
        } else if path.is_dir() {
            let main = path.join("main.rs");
            if main.is_file() {
                if let Some(name) = path.file_name() {
                    targets.push((name.to_string_lossy().into_owned(), main));
                }
            }
        }
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    targets
}

fn reachable_rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut seen = BTreeSet::new();

    while let Some(path) = stack.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        files.push(path.clone());
        let Ok(src) = fs::read_to_string(&path) else {
            continue;
        };
        let Some(parent) = path.parent() else {
            continue;
        };
        let mut explicit_path = None;
        for line in src.lines() {
            let line = line.trim();
            if let Some(path) = line
                .strip_prefix("#[path = \"")
                .and_then(|path| path.strip_suffix("\"]"))
            {
                explicit_path = Some(path);
                continue;
            }
            let names = parse_mod_names(line);
            let Some(name) = names.first() else {
                if !line.is_empty() && !line.starts_with("#[") && !line.starts_with("//") {
                    explicit_path = None;
                }
                continue;
            };
            if let Some(path) = explicit_path.take() {
                stack.push(parent.join(path));
                continue;
            }
            let sibling = parent.join(format!("{name}.rs"));
            let dir_mod = parent.join(name).join("mod.rs");
            if sibling.is_file() {
                stack.push(sibling);
            } else if dir_mod.is_file() {
                stack.push(dir_mod);
            }
        }
    }
    files
}

fn parse_mod_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        let rest = line
            .strip_prefix("pub mod ")
            .or_else(|| line.strip_prefix("mod "));
        let Some(rest) = rest else {
            continue;
        };
        let Some(name) = rest.strip_suffix(';') else {
            continue;
        };
        let name = name.trim();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            names.push(name.to_string());
        }
    }
    names
}

fn has_ignored_test(src: &str) -> bool {
    src.contains("#[ignore]") || src.contains("#[ignore =") || src.contains("#[ignore=")
}

fn uses_containers(src: &str) -> bool {
    src.contains("testcontainers")
        || src.contains("testcontainers_modules")
        || src.contains("redis_helpers")
        || src.contains("k3s_helpers")
        || src.contains("\"podman\"")
}

fn needs_cdylib_plugins(src: &str) -> bool {
    src.contains("load_plugin_from_path")
        || src.contains("ffi_plugin_exists")
        || src.contains("computation::load(")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn write_pkg(files: &[(&str, &str)]) -> PathBuf {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "xtask-integration-matrix-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("tests")).unwrap();
        for (rel, contents) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        dir
    }

    #[test]
    fn parse_mod_names_skips_comments_and_inline_mods() {
        let src = r#"
mod helpers;
pub mod more_helpers;
// mod ignored;
mod nested {
    fn x() {}
}
"#;
        assert_eq!(parse_mod_names(src), ["helpers", "more_helpers"]);
    }

    #[test]
    fn discovers_ignored_container_test_and_follows_mod() {
        let dir = write_pkg(&[
            (
                "tests/integration_tests.rs",
                r#"
mod helpers;
#[tokio::test]
#[ignore]
async fn spins_up_db() {}
"#,
            ),
            (
                "tests/helpers.rs",
                "use testcontainers::runners::AsyncRunner;\n",
            ),
            (
                "tests/unitish.rs",
                r#"
#[test]
fn no_ignore() { }
"#,
            ),
        ]);
        let jobs = discover_jobs([("demo-source".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(
            jobs,
            [IntegrationTestJob {
                name: "demo-source / integration_tests".to_string(),
                package: "demo-source".to_string(),
                test: "integration_tests".to_string(),
                extra: "none".to_string(),
                timeout: 60,
            }]
        );
    }

    #[test]
    fn skips_ignored_tests_without_containers() {
        let dir = write_pkg(&[(
            "tests/live.rs",
            r#"
#[tokio::test]
#[ignore = "needs credentials"]
async fn hits_network() {}
"#,
        )]);
        let jobs = discover_jobs([("demo-source".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert!(jobs.is_empty());
    }

    #[test]
    fn marks_oracle_packages() {
        let dir = write_pkg(&[(
            "tests/integration_test.rs",
            r#"
use testcontainers::GenericImage;
#[tokio::test]
#[ignore]
async fn oracle() {}
"#,
        )]);
        let jobs = discover_jobs([("drasi-source-oracle".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(jobs[0].extra, "oracle");
        assert_eq!(jobs[0].timeout, 90);
    }

    #[test]
    fn marks_jobs_that_load_cdylib_plugins() {
        let dir = write_pkg(&[(
            "tests/integration_test.rs",
            r#"
use testcontainers::runners::AsyncRunner;
fn ffi_plugin_exists(crate_name: &str) -> bool { false }
#[tokio::test]
#[ignore]
async fn ffi() {
    let _ = load_plugin_from_path;
}
"#,
        )]);
        let jobs = discover_jobs([("drasi-source-mssql".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(jobs[0].extra, "plugins");
    }

    #[test]
    fn follows_explicit_fixture_paths_and_marks_native_podman_jobs() {
        let dir = write_pkg(&[
            (
                "tests/native.rs",
                "#[path = \"fixtures/database.rs\"]\nmod database;\n",
            ),
            (
                "tests/fixtures/database.rs",
                r#"
#[tokio::test]
#[ignore = "requires Podman"]
async fn database() {
    Command::new("podman");
    computation::load("plugin");
}
"#,
            ),
        ]);
        let jobs = discover_jobs([("drasi-host-sdk".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].test, "native");
        assert_eq!(jobs[0].extra, "plugins,podman");
    }

    #[test]
    fn leaves_explicit_configuration_out_of_automatic_jobs() {
        for marker in ["DRASI_MSSQL_TEST_MODE", "DRASI_NETWORK_CONTAINER"] {
            let source = format!(
                r#"
#[test]
#[ignore = "requires explicit ownership and credentials"]
fn database() {{
    std::env::var("{marker}");
    Command::new("podman");
}}
"#,
            );
            let dir = write_pkg(&[("tests/configured.rs", &source)]);
            let jobs = discover_jobs([("demo".to_string(), dir.clone())]);
            let _ = fs::remove_dir_all(&dir);
            assert!(jobs.is_empty(), "{marker} requires explicit preparation");
        }
    }

    #[test]
    fn keeps_feature_gated_container_tests_enabled() {
        let dir = write_pkg(&[(
            "tests/recovery.rs",
            r#"
#![cfg(feature = "garnet-tests")]
#[tokio::test]
#[ignore = "requires a container"]
async fn recovery() { redis_helpers::setup_redis().await; }
"#,
        )]);
        let jobs = discover_jobs([("lib-integration-tests".to_string(), dir.clone())]);
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].extra, "garnet-tests");
    }
}
