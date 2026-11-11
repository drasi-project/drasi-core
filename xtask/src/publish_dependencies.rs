// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{is_publishable, CargoMetadata, Package};
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;

#[derive(Default, Deserialize)]
struct Policy {
    publish: Option<bool>,
    release: Option<bool>,
}

#[derive(Deserialize)]
struct PackagePolicy {
    name: String,
    #[serde(flatten)]
    policy: Policy,
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    workspace: Policy,
    #[serde(default)]
    package: Vec<PackagePolicy>,
}

impl Config {
    fn publishes(&self, package: &Package) -> bool {
        let policy = self.package.iter().find(|p| p.name == package.name);
        is_publishable(package)
            && policy
                .and_then(|p| p.policy.publish)
                .or(self.workspace.publish)
                .unwrap_or(true)
            && policy
                .and_then(|p| p.policy.release)
                .or(self.workspace.release)
                .unwrap_or(true)
    }
}

// Keep this traversal aligned with release-plz v0.3.157's release_order.rs:
// preserve metadata order, visit normal/build dependencies first, and include
// dev-dependencies only when referenced by a feature as "dependency/feature".
fn release_order<'a>(packages: &[&'a Package]) -> Result<Vec<&'a Package>, String> {
    fn visit<'a>(
        package: &'a Package,
        packages: &[&'a Package],
        ordered: &mut Vec<&'a Package>,
        visiting: &mut BTreeSet<&'a str>,
    ) -> Result<(), String> {
        if ordered.iter().any(|p| p.name == package.name) {
            return Ok(());
        }
        if !visiting.insert(&package.name) {
            return Err(format!(
                "Circular release dependency involving {}",
                package.name
            ));
        }
        for dependency in &package.dependencies {
            let in_feature = package.features.values().flatten().any(|feature| {
                feature
                    .split_once('/')
                    .is_some_and(|(name, _)| name == dependency.name)
            });
            if dependency.kind.as_deref() == Some("dev") && !in_feature {
                continue;
            }
            if let Some(next) = packages
                .iter()
                .find(|p| p.name == dependency.name && p.name != package.name)
            {
                visit(next, packages, ordered, visiting)?;
            }
        }
        visiting.remove(package.name.as_str());
        ordered.push(package);
        Ok(())
    }

    let mut ordered = Vec::new();
    let mut visiting = BTreeSet::new();
    for package in packages {
        visit(package, packages, &mut ordered, &mut visiting)?;
    }
    Ok(ordered)
}

fn blockers(
    packages: &[Package],
    config: &Config,
    mut published: impl FnMut(&str, &VersionReq) -> Result<bool, String>,
) -> Result<Vec<String>, String> {
    let releasing: Vec<_> = packages.iter().filter(|p| config.publishes(p)).collect();
    let order = release_order(&releasing)?;
    let positions: BTreeMap<_, _> = order
        .iter()
        .enumerate()
        .map(|(index, package)| (package.name.as_str(), index))
        .collect();
    let mut failures = Vec::new();
    for (index, package) in order.iter().enumerate() {
        for dependency in &package.dependencies {
            if dependency.kind.as_deref() != Some("dev")
                || dependency.path.is_none()
                || dependency.req == "*"
            {
                continue;
            }
            let Some(local) = packages.iter().find(|p| p.name == dependency.name) else {
                continue;
            };
            let requirement = VersionReq::parse(&dependency.req)
                .map_err(|error| format!("Invalid requirement for {}: {error}", dependency.name))?;
            let local_version = Version::parse(&local.version)
                .map_err(|error| format!("Invalid version for {}: {error}", local.name))?;
            if positions
                .get(local.name.as_str())
                .is_some_and(|position| *position < index)
                && requirement.matches(&local_version)
            {
                continue;
            }
            if !published(&dependency.name, &requirement)? {
                let timing = match positions.get(local.name.as_str()) {
                    Some(position) if *position < index => format!(
                        "earlier workspace version {} does not match the requirement",
                        local.version
                    ),
                    Some(_) => "not published earlier in release order".into(),
                    None => "excluded from publication".into(),
                };
                failures.push(format!(
                    "{} --dev {} {}: no matching non-yanked crates.io version; {timing}",
                    package.name, dependency.name, dependency.req
                ));
            }
        }
    }
    Ok(failures)
}

#[derive(Deserialize)]
struct IndexVersion {
    vers: String,
    yanked: bool,
}

fn parse_index(body: &str) -> Result<Vec<Version>, String> {
    if body.trim().is_empty() {
        return Err("Empty crates.io index response".into());
    }
    body.lines()
        .map(|line| {
            let entry: IndexVersion = serde_json::from_str(line)
                .map_err(|error| format!("Invalid crates.io index entry: {error}"))?;
            let version = Version::parse(&entry.vers)
                .map_err(|error| format!("Invalid crates.io version: {error}"))?;
            Ok((!entry.yanked).then_some(version))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

fn registry_versions(name: &str) -> Result<Vec<Version>, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("Invalid crates.io package name: {name}"));
    }
    let name = name.to_ascii_lowercase();
    let path = match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    };
    let response = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "60",
            "--retry",
            "3",
            "--write-out",
            "\n%{http_code}",
            &format!("https://index.crates.io/{path}"),
        ])
        .output()
        .map_err(|error| format!("Cannot query crates.io for {name}: {error}"))?;
    if !response.status.success() {
        return Err(format!(
            "Cannot query crates.io for {name}: {}",
            String::from_utf8_lossy(&response.stderr)
        ));
    }
    let text = String::from_utf8(response.stdout)
        .map_err(|error| format!("Invalid crates.io response for {name}: {error}"))?;
    parse_registry_response(&name, &text)
}

fn parse_registry_response(name: &str, text: &str) -> Result<Vec<Version>, String> {
    let (body, status) = text
        .rsplit_once('\n')
        .ok_or_else(|| format!("Missing crates.io HTTP status for {name}"))?;
    match status {
        "200" => parse_index(body).map_err(|error| format!("{name}: {error}")),
        "404" => Ok(Vec::new()),
        _ => Err(format!("Cannot query crates.io for {name}: HTTP {status}")),
    }
}

pub(super) fn check(metadata: &CargoMetadata) -> Result<(), String> {
    let path = metadata.workspace_root.join("release-plz.toml");
    let config = fs::read_to_string(&path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let config: Config =
        toml::from_str(&config).map_err(|error| format!("Invalid {}: {error}", path.display()))?;
    let mut cache = BTreeMap::new();
    let failures = blockers(&metadata.packages, &config, |name, requirement| {
        if !cache.contains_key(name) {
            cache.insert(name.to_string(), registry_versions(name)?);
        }
        Ok(cache[name]
            .iter()
            .any(|version| requirement.matches(version)))
    })?;
    if failures.is_empty() {
        println!("Release dev-dependencies are published or ordered before their consumers.");
        Ok(())
    } else {
        Err(format!(
            "Release dev-dependency preflight failed:\n  {}\n\
             Move cross-crate tests to a publish-false test crate or use path-only dev-dependencies. \
             publish_no_verify does not skip Cargo's registry dependency resolution.",
            failures.join("\n  ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(name: &str, dependencies: serde_json::Value) -> Package {
        serde_json::from_value(json!({
            "name": name, "version": "0.3.14", "manifest_path": "/fixture/Cargo.toml",
            "features": {}, "dependencies": dependencies
        }))
        .unwrap()
    }

    fn dependency(name: &str, kind: Option<&str>, req: &str) -> serde_json::Value {
        json!({"name": name, "kind": kind, "req": req, "path": format!("/fixture/{name}")})
    }

    #[test]
    fn rejects_unpublished_later_dev_dependency_without_a_cycle() {
        let packages = vec![
            package(
                "salesforce-common",
                json!([dependency("application", Some("dev"), "^0.3.14")]),
            ),
            package("application", json!([])),
        ];
        let failures = blockers(&packages, &Config::default(), |_, _| Ok(false)).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("salesforce-common --dev application ^0.3.14"));
        assert!(failures[0].contains("not published earlier"));
    }

    #[test]
    fn accepts_earlier_transitive_normal_and_build_dependencies_without_registry_access() {
        let packages = vec![
            package(
                "consumer",
                json!([
                    dependency("application", Some("dev"), "^0.3.14"),
                    dependency("bridge", Some("build"), "^0.3.14")
                ]),
            ),
            package(
                "bridge",
                json!([dependency("application", None, "^0.3.14")]),
            ),
            package("application", json!([])),
        ];
        assert!(blockers(&packages, &Config::default(), |_, _| {
            panic!("earlier dependencies must not need a registry query")
        })
        .unwrap()
        .is_empty());
    }

    #[test]
    fn accepts_matching_published_versions_but_not_older_yanked_or_prerelease_versions() {
        let packages = vec![
            package(
                "consumer",
                json!([dependency("application", Some("dev"), "^0.3.14")]),
            ),
            package("application", json!([])),
        ];
        for (version, yanked, allowed) in [
            ("0.3.13", false, false),
            ("0.3.14", false, true),
            ("0.3.15", false, true),
            ("0.4.0", false, false),
            ("0.3.14", true, false),
            ("0.3.15-beta.1", false, false),
        ] {
            let versions =
                parse_index(&json!({"vers": version, "yanked": yanked}).to_string()).unwrap();
            let failures = blockers(&packages, &Config::default(), |_, req| {
                Ok(versions.iter().any(|version| req.matches(version)))
            })
            .unwrap();
            assert_eq!(failures.is_empty(), allowed, "{version}, yanked={yanked}");
        }
    }

    #[test]
    fn respects_exclusions_and_path_only_dev_dependencies() {
        let packages = vec![
            package(
                "consumer",
                json!([dependency("application", Some("dev"), "^0.3.14")]),
            ),
            package("application", json!([])),
        ];
        let config: Config =
            toml::from_str("[[package]]\nname='consumer'\npublish=false\n").unwrap();
        assert!(
            blockers(&packages, &config, |_, _| panic!("excluded consumer"))
                .unwrap()
                .is_empty()
        );
        let mut packages = packages;
        packages[0].publish = Some(Vec::new());
        assert!(blockers(&packages, &Config::default(), |_, _| panic!(
            "publish=false"
        ))
        .unwrap()
        .is_empty());
        packages[0].publish = None;
        packages[0].dependencies[0].req = "*".into();
        assert!(blockers(&packages, &Config::default(), |_, _| panic!(
            "path-only dependency"
        ))
        .unwrap()
        .is_empty());
    }

    #[test]
    fn excluded_dependency_cannot_satisfy_order_and_registry_errors_fail_closed() {
        let packages = vec![
            package("application", json!([])),
            package(
                "consumer",
                json!([dependency("application", Some("dev"), "^0.3.14")]),
            ),
        ];
        let config: Config =
            toml::from_str("[[package]]\nname='application'\nrelease=false\n").unwrap();
        let failures = blockers(&packages, &config, |_, _| Ok(false)).unwrap();
        assert!(failures[0].contains("excluded from publication"));
        assert_eq!(
            blockers(
                &packages,
                &config,
                |_, _| Err("registry unavailable".into())
            )
            .unwrap_err(),
            "registry unavailable"
        );
        assert!(parse_index("invalid JSON").is_err());
    }

    #[test]
    fn feature_referenced_dev_dependency_is_ordered_first() {
        let mut packages = vec![
            package(
                "consumer",
                json!([dependency("application", Some("dev"), "^0.3.14")]),
            ),
            package("application", json!([])),
        ];
        packages[0]
            .features
            .insert("default".into(), vec!["application/feature".into()]);
        assert!(blockers(&packages, &Config::default(), |_, _| {
            panic!("feature dependency should be released first")
        })
        .unwrap()
        .is_empty());
    }

    #[test]
    fn earlier_workspace_version_must_match_the_dev_requirement() {
        let packages = vec![
            package("application", json!([])),
            package(
                "consumer",
                json!([dependency("application", Some("dev"), "^0.4.0")]),
            ),
        ];
        let failures = blockers(&packages, &Config::default(), |_, _| Ok(false)).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("earlier workspace version 0.3.14 does not match"));
    }

    #[test]
    fn registry_not_found_is_distinct_from_http_and_malformed_response_errors() {
        assert!(parse_registry_response("new-crate", "not found\n404")
            .unwrap()
            .is_empty());
        for response in [
            "error\n401",
            "error\n429",
            "error\n500",
            "\n200",
            "{}\n200",
            "",
        ] {
            assert!(
                parse_registry_response("application", response).is_err(),
                "{response}"
            );
        }
        assert_eq!(
            parse_registry_response("application", "{\"vers\":\"0.3.14\",\"yanked\":false}\n200",)
                .unwrap(),
            vec![Version::parse("0.3.14").unwrap()]
        );
    }
}
