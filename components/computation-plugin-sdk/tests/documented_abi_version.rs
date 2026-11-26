// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native ABI and wire versions quoted in the plugin documentation must be
//! the versions this SDK builds. A vendor following stale text would build a
//! library the Host rejects.

use drasi_computation_plugin_abi::{ABI_MAJOR, ABI_MINOR, ABI_VERSION, WIRE_VERSION};
use std::path::{Path, PathBuf};

/// The ordinary Source/Reaction plugin ABI, which the native documents cite
/// for contrast. It is versioned independently by `FFI_SDK_VERSION`.
const ORDINARY_ABI: &str = "0.19";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_owned()
}

fn markdown_under(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("readable directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            markdown_under(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "md")
            && path.file_name().is_some_and(|name| name != "CHANGELOG.md")
        {
            files.push(path);
        }
    }
}

fn documents() -> Vec<PathBuf> {
    let root = workspace();
    let mut files = [
        "components/AGENTS.md",
        "components/plugin-architecture.md",
        "components/computation-plugin-sdk/README.md",
        "components/host-sdk/README.md",
        "lib/docs/computation-graph-reference.md",
    ]
    .iter()
    .map(|file| root.join(file))
    .collect::<Vec<_>>();
    markdown_under(&root.join("components/computation-plugins"), &mut files);
    files
}

/// The version token that follows `marker` (skipping Markdown emphasis), such
/// as `0.1.0` in `ABI **0.1.0**`.
fn versions_after<'a>(line: &'a str, marker: &str) -> Vec<&'a str> {
    line.match_indices(marker)
        .filter_map(|(index, _)| {
            let rest = line[index + marker.len()..].trim_start_matches(['*', '`']);
            let length = rest
                .find(|character: char| !(character.is_ascii_digit() || character == '.'))
                .unwrap_or(rest.len());
            let version = rest[..length].trim_end_matches('.');
            (!version.is_empty()).then_some(version)
        })
        .collect()
}

#[test]
fn documented_native_abi_and_wire_versions_match_the_sdk() {
    let major = ABI_MAJOR.to_string();
    let current_minor = format!("{ABI_MAJOR}.{ABI_MINOR}");
    let mut stale = Vec::new();
    for file in documents() {
        let text = std::fs::read_to_string(&file).expect("readable document");
        for (number, line) in text.lines().enumerate() {
            let place = || format!("{}:{}: {line}", file.display(), number + 1);
            for version in versions_after(line, "abi-version = \"") {
                if version != ABI_VERSION {
                    stale.push(place());
                }
            }
            let abi = versions_after(line, "ABI ");
            for version in abi.into_iter().chain(versions_after(line, "ABI is ")) {
                let native = version == major || version == current_minor || version == ABI_VERSION;
                if !native
                    && version != ORDINARY_ABI
                    && !version.starts_with(&format!("{ORDINARY_ABI}."))
                {
                    stale.push(place());
                }
            }
            for version in versions_after(line, "wire version ") {
                if version != WIRE_VERSION.to_string() {
                    stale.push(place());
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "documents quote a native ABI other than {ABI_VERSION} or a wire version other than {WIRE_VERSION}:\n{}",
        stale.join("\n")
    );
}

#[test]
fn version_tokens_are_read_through_markdown_emphasis() {
    assert_eq!(
        versions_after("Native ABI **0.1.0**, here", "ABI "),
        ["0.1.0"]
    );
    assert_eq!(versions_after("native ABI 0.1). It", "ABI "), ["0.1"]);
    assert_eq!(versions_after("the ABI 7 SDK", "ABI "), ["7"]);
    assert_eq!(
        versions_after("Native ABI is **0.1.0**.", "ABI is "),
        ["0.1.0"]
    );
    assert!(versions_after("the ABI marker", "ABI ").is_empty());
}
