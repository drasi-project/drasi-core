// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{config::MAX_MANIFEST_BYTES, platform};

pub(crate) struct StateDirectory {
    path: PathBuf,
    _lock: File,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u32,
    instance_id: String,
}

impl StateDirectory {
    pub(crate) fn acquire(path: &Path, instance: &str) -> Result<Self> {
        private_directory(path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.join("owner.lock"))?;
        platform::lock(&lock)?;
        let identity_path = path.join("identity.json");
        if identity_path.exists() {
            let identity: Identity = read_json(&identity_path, 4096)?;
            ensure!(
                identity.version == 1 && identity.instance_id == instance,
                "state namespace belongs to a different stable instance"
            );
        } else {
            atomic_json(
                &identity_path,
                &Identity {
                    version: 1,
                    instance_id: instance.to_owned(),
                },
            )?;
        }
        Ok(Self {
            path: path.to_owned(),
            _lock: lock,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "state directory must be a real directory"
    );
    ensure!(
        metadata.uid() == platform::owner_uid() && metadata.mode() & 0o077 == 0,
        "state directory must be owner-only (0700)"
    );
    ensure!(
        std::fs::canonicalize(path)? == path,
        "state path must not traverse symlinks"
    );
    Ok(())
}

pub(crate) fn read_bytes(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "file exceeds byte budget"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "file exceeds byte budget");
    Ok(bytes)
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path, limit: usize) -> Result<T> {
    serde_json::from_slice(&read_bytes(path, limit)?).context("invalid bounded JSON file")
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_MANIFEST_BYTES * 2,
        "state record exceeds byte budget"
    );
    let next = path.with_extension("next");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&next)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&next, path)?;
    File::open(path.parent().context("state record has no directory")?)?.sync_all()?;
    Ok(())
}

pub(crate) fn read_key(path: &Path) -> Result<[u8; 32]> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.uid() == platform::owner_uid() && metadata.permissions().mode() & 0o077 == 0,
        "configuration key must be owner-only"
    );
    read_bytes(path, 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("configuration key must contain exactly 32 bytes"))
}
