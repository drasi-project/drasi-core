// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    RootCertStore,
};
use serde::{Deserialize, Serialize};

use crate::TlsResource;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsRecipeKind {
    #[serde(rename = "bridgeTls")]
    BridgeTls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsRole {
    Client,
    Server,
}

/// Shared by normal hosts and Server. Configuration contains paths, never
/// inline private-key bytes. DER roots/chains and unencrypted PKCS8 DER keys only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsResourceRecipe {
    pub kind: TlsRecipeKind,
    pub version: u32,
    pub role: TlsRole,
    pub trust_roots: Vec<PathBuf>,
    pub certificate_chain: Vec<PathBuf>,
    pub private_key: PathBuf,
    pub peer_certificate_sha256: String,
}

impl TlsResourceRecipe {
    /// Pure validation, suitable for validating child definitions without reading
    /// their keys or loading any child-only code in the parent.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "unsupported bridge TLS recipe version");
        anyhow::ensure!(
            (1..=16).contains(&self.trust_roots.len())
                && (1..=8).contains(&self.certificate_chain.len()),
            "TLS roots/chains exceed supported counts"
        );
        anyhow::ensure!(
            self.peer_certificate_sha256.len() == 64
                && self
                    .peer_certificate_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "peer pin must be exactly 64 lowercase hexadecimal characters"
        );
        for path in self
            .trust_roots
            .iter()
            .chain(&self.certificate_chain)
            .chain(std::iter::once(&self.private_key))
        {
            let text = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("TLS paths must be UTF-8"))?;
            anyhow::ensure!(
                !text.is_empty()
                    && text.len() <= 4096
                    && !text.chars().any(char::is_control)
                    && !path
                        .components()
                        .any(|part| matches!(part, Component::ParentDir)),
                "TLS paths must be bounded and cannot contain parent traversal"
            );
        }
        Ok(())
    }

    /// The owning host resolves this resource locally, never while merely
    /// inspecting a child manifest. Relative paths are relative to `base`.
    pub fn resolve(&self, base: &Path) -> anyhow::Result<Arc<TlsResource>> {
        self.validate()?;
        let mut total = 0usize;
        let path = |path: &Path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            }
        };
        let mut roots = RootCertStore::empty();
        for root in &self.trust_roots {
            roots.add(CertificateDer::from(read_bounded(
                &path(root),
                false,
                &mut total,
            )?))?;
        }
        let chain = self
            .certificate_chain
            .iter()
            .map(|file| read_bounded(&path(file), false, &mut total).map(CertificateDer::from))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut pin = [0; 32];
        for (index, value) in pin.iter_mut().enumerate() {
            *value =
                u8::from_str_radix(&self.peer_certificate_sha256[index * 2..index * 2 + 2], 16)?;
        }
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(read_bounded(
            &path(&self.private_key),
            true,
            &mut total,
        )?));
        Ok(Arc::new(match self.role {
            TlsRole::Client => TlsResource::client(roots, chain, key, pin)?,
            TlsRole::Server => TlsResource::server(roots, chain, key, pin)?,
        }))
    }
}

fn read_bounded(path: &Path, private: bool, total: &mut usize) -> anyhow::Result<Vec<u8>> {
    let before = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "TLS material must be a regular non-symlink file"
    );
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "TLS material must be a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        anyhow::ensure!(
            before.dev() == metadata.dev() && before.ino() == metadata.ino(),
            "TLS file changed while opening"
        );
        if private {
            anyhow::ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "private-key file must grant no group/other permissions"
            );
        }
    }
    #[cfg(not(unix))]
    anyhow::ensure!(!private, "private-key ACL verification for TLS recipes is not implemented on this platform; use an explicitly secured programmatic identity provider");
    let limit = if private { 32 * 1024 } else { 64 * 1024 };
    anyhow::ensure!(
        metadata.len() > 0 && metadata.len() <= limit as u64,
        "TLS file exceeds its bounded DER input limit"
    );
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= limit,
        "TLS file grew beyond its input limit"
    );
    *total = total
        .checked_add(bytes.len())
        .ok_or_else(|| anyhow::anyhow!("TLS input length overflow"))?;
    anyhow::ensure!(
        *total <= 1024 * 1024,
        "combined TLS material exceeds one MiB"
    );
    Ok(bytes)
}
