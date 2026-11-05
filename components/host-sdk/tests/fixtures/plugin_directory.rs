// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::path::{Path, PathBuf};

pub fn plugin_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root from CARGO_MANIFEST_DIR");
    selected_plugin_dir(
        workspace_root,
        std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        std::env::var_os("DRASI_HOST_TEST_PLUGINS_DIR").map(PathBuf::from),
    )
}

pub fn selected_plugin_dir(
    workspace_root: &Path,
    target: Option<PathBuf>,
    fixtures: Option<PathBuf>,
) -> PathBuf {
    if let Some(directory) = fixtures {
        return directory;
    }
    let target = match target {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace_root.join(path),
        None => workspace_root.join("target"),
    };
    target.join("debug").join("plugins")
}
