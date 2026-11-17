// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Context, Result};
use std::{io::ErrorKind, path::Path};

pub fn load() -> Result<()> {
    load_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(".env"),
        |key| std::env::var_os(key).is_some(),
        |key, value| std::env::set_var(key, value),
    )
}

fn load_file(
    path: &Path,
    mut already_set: impl FnMut(&str) -> bool,
    mut apply: impl FnMut(&str, &str),
) -> Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    let entries =
        dotenvy::from_read_iter(content.as_bytes()).collect::<std::result::Result<Vec<_>, _>>();
    let Ok(entries) = entries else {
        // dotenv parse errors include the raw line, which may contain a password.
        bail!("Invalid configuration in {}. Use KEY=value assignments and quote values containing spaces.", path.display());
    };
    for (key, value) in entries {
        if !already_set(&key) {
            apply(&key, &value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn loads_local_settings_and_preserves_existing_environment() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(".env");
        std::fs::write(&path, "# Local setup\r\nFOUNDRY_PROJECT_ENDPOINT=\r\nFOUNDRY_AGENT_NAME=\"local agent\"\r\nFOUNDRY_DEMO_MYSQL_PASSWORD=local-fixture-only\r\n")?;
        let existing =
            BTreeMap::from([("FOUNDRY_AGENT_NAME".to_owned(), "shell-agent".to_owned())]);
        let mut applied = BTreeMap::new();
        load_file(
            &path,
            |key| existing.contains_key(key),
            |key, value| {
                applied.insert(key.to_owned(), value.to_owned());
            },
        )?;
        assert!(!applied.contains_key("FOUNDRY_AGENT_NAME"));
        assert_eq!(applied["FOUNDRY_PROJECT_ENDPOINT"], "");
        assert_eq!(applied["FOUNDRY_DEMO_MYSQL_PASSWORD"], "local-fixture-only");
        Ok(())
    }

    #[test]
    fn malformed_files_do_not_apply_partial_settings_or_expose_values() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(".env");
        std::fs::write(
            &path,
            "FOUNDRY_AGENT_NAME=agent\nPASSWORD='private-fixture-value",
        )?;
        let mut applied = false;
        let error = load_file(&path, |_| false, |_, _| applied = true)
            .expect_err("Invalid dotenv must fail");
        assert!(!applied);
        assert!(!error.to_string().contains("private-fixture-value"));
        assert!(error.to_string().contains("Invalid configuration"));
        Ok(())
    }
}
