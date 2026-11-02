use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::catalog::GraphRecord;
use crate::position::LocalPosition;
use crate::projection::{validate_identifier, validate_org_id};

pub const SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY: &str = "salesforce.bootstrap_ticket.v2";
pub const SNAPSHOT_MANIFEST_VERSION: u8 = 2;
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotTicket {
    pub version: u8,
    pub ticket_id: String,
    pub source_id: String,
    pub source_identity: String,
    pub org_id: String,
    pub objects: Vec<String>,
    #[serde(with = "u128_as_string")]
    pub epoch: u128,
    pub head_ordinal: u64,
    pub manifest_rel_path: String,
    pub expires_at_ms: u64,
    pub checksum_sha256: String,
}

impl SnapshotTicket {
    pub fn local_position(&self) -> Bytes {
        LocalPosition::new(self.epoch, self.head_ordinal).encode()
    }

    pub fn validate_basic(&self, expected_source_id: &str, now_ms: u64) -> Result<()> {
        ensure!(
            !self.ticket_id.is_empty() && self.ticket_id.len() <= 128,
            "snapshot ticket_id must be 1..=128 characters"
        );
        ensure!(
            !self.source_id.is_empty() && self.source_id.len() <= 256,
            "snapshot source_id must be 1..=256 characters"
        );
        ensure!(
            !self.source_identity.is_empty() && self.source_identity.len() <= 256,
            "snapshot source_identity must be 1..=256 characters"
        );
        validate_org_id(&self.org_id)?;
        validate_object_set(&self.objects)?;
        ensure!(
            self.version == SNAPSHOT_MANIFEST_VERSION,
            "unsupported snapshot ticket version {}",
            self.version
        );
        ensure!(
            self.source_id == expected_source_id,
            "snapshot ticket source_id mismatch"
        );
        ensure!(self.expires_at_ms >= now_ms, "snapshot ticket has expired");
        ensure!(
            self.manifest_rel_path.len() <= 4096,
            "snapshot manifest_rel_path exceeds 4096-character limit"
        );
        validate_relative_snapshot_path(&self.manifest_rel_path)?;
        ensure!(
            self.checksum_sha256.len() == 64
                && self
                    .checksum_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "snapshot ticket checksum must be a 64-character hex SHA-256 digest"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotManifest {
    pub version: u8,
    pub source_id: String,
    pub source_identity: String,
    pub org_id: String,
    pub objects: Vec<String>,
    #[serde(with = "u128_as_string")]
    pub epoch: u128,
    pub head_ordinal: u64,
    pub created_at_ms: u64,
    pub rows_rel_path: String,
    pub row_count: u64,
    pub rows_sha256: String,
}

impl SnapshotManifest {
    pub fn local_position(&self) -> Bytes {
        LocalPosition::new(self.epoch, self.head_ordinal).encode()
    }

    pub fn validate_basic(&self) -> Result<()> {
        ensure!(
            self.version == SNAPSHOT_MANIFEST_VERSION,
            "unsupported snapshot manifest version {}",
            self.version
        );
        ensure!(
            !self.source_id.is_empty() && self.source_id.len() <= 256,
            "snapshot manifest source_id must be 1..=256 characters"
        );
        ensure!(
            !self.source_identity.is_empty() && self.source_identity.len() <= 256,
            "snapshot manifest source_identity must be 1..=256 characters"
        );
        validate_org_id(&self.org_id)?;
        validate_object_set(&self.objects)?;
        validate_relative_snapshot_path(&self.rows_rel_path)?;
        ensure!(
            self.rows_sha256.len() == 64
                && self
                    .rows_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()),
            "snapshot rows checksum must be a 64-character hex SHA-256 digest"
        );
        Ok(())
    }
}

pub fn write_snapshot_record<W: Write>(
    writer: &mut W,
    record: &GraphRecord,
    max_record_bytes: usize,
) -> Result<usize> {
    ensure!(
        max_record_bytes > 0,
        "max_record_bytes must be greater than zero"
    );
    let payload =
        rmp_serde::to_vec_named(record).context("failed to encode snapshot graph record")?;
    ensure!(
        payload.len() <= max_record_bytes,
        "snapshot graph record exceeds max_record_bytes limit"
    );
    let payload_len = u32::try_from(payload.len())
        .context("snapshot graph record payload exceeds u32 framing limit")?;
    writer
        .write_all(&payload_len.to_be_bytes())
        .context("failed to write snapshot frame length")?;
    writer
        .write_all(&payload)
        .context("failed to write snapshot frame payload")?;
    Ok(4usize.saturating_add(payload.len()))
}

pub fn read_snapshot_record<R: Read>(
    reader: &mut R,
    max_record_bytes: usize,
) -> Result<Option<GraphRecord>> {
    ensure!(
        max_record_bytes > 0,
        "max_record_bytes must be greater than zero"
    );

    let mut length_bytes = [0u8; 4];
    match reader.read(&mut length_bytes[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!("single-byte read returned more than one byte"),
        Err(error) => return Err(error).context("failed to read snapshot frame length"),
    }
    reader
        .read_exact(&mut length_bytes[1..])
        .context("snapshot frame ended before length header completed")?;

    let payload_len = u32::from_be_bytes(length_bytes) as usize;
    ensure!(
        payload_len > 0,
        "snapshot frame payload length must be greater than zero"
    );
    ensure!(
        payload_len <= max_record_bytes,
        "snapshot graph record exceeds max_record_bytes limit"
    );

    let mut payload = vec![0u8; payload_len];
    reader
        .read_exact(&mut payload)
        .context("snapshot frame ended before payload completed")?;
    let record = rmp_serde::from_slice(&payload)
        .context("failed to decode snapshot graph record payload")?;
    Ok(Some(record))
}

fn validate_object_set(objects: &[String]) -> Result<()> {
    ensure!(
        !objects.is_empty(),
        "snapshot object set must include at least one object"
    );
    let mut seen = BTreeSet::new();
    for object in objects {
        validate_identifier(object)?;
        ensure!(
            seen.insert(object),
            "snapshot object set contains duplicate object '{object}'"
        );
    }
    Ok(())
}

pub fn validate_relative_snapshot_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "snapshot path must not be empty");
    ensure!(
        !path.starts_with('/') && !path.contains('\\'),
        "snapshot path must be a relative POSIX path"
    );
    ensure!(!path.contains(".."), "snapshot path must not contain '..'");
    Ok(())
}

pub fn resolve_snapshot_path(spool_root: &Path, rel_path: &str) -> Result<PathBuf> {
    validate_relative_snapshot_path(rel_path)?;
    let resolved = spool_root.join(rel_path);
    let canonical_root = spool_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize spool root {}", spool_root.display()))?;
    if resolved.exists() {
        let canonical_leaf = resolved.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize snapshot path {}",
                resolved.display()
            )
        })?;
        ensure!(
            canonical_leaf.starts_with(&canonical_root),
            "snapshot path escapes the configured spool root"
        );
        return Ok(canonical_leaf);
    }

    let canonical_parent = resolved
        .parent()
        .unwrap_or(spool_root)
        .canonicalize()
        .with_context(|| {
            format!(
                "failed to canonicalize snapshot parent {}",
                resolved.display()
            )
        })?;
    ensure!(
        canonical_parent.starts_with(&canonical_root),
        "snapshot path escapes the configured spool root"
    );
    Ok(resolved)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

mod u128_as_string {
    use super::*;

    pub fn serialize<S>(value: &u128, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> std::result::Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum U128Repr {
            Number(u128),
            String(String),
        }

        match U128Repr::deserialize(deserializer)? {
            U128Repr::Number(value) => Ok(value),
            U128Repr::String(value) => value
                .parse::<u128>()
                .map_err(|_| D::Error::custom("invalid u128 string")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn sample_record() -> GraphRecord {
        GraphRecord {
            object: "Contact".to_string(),
            id: "003000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Ada".into()))]),
            relationships: vec![crate::catalog::LookupRelationship {
                field: "AccountId".to_string(),
                label: "Account".to_string(),
                target_object: "Account".to_string(),
                target_id: "001000000000001AAA".to_string(),
            }],
            observed_at_ms: 1_700_000_000_000,
            schema_revision: "schema-v2".to_string(),
        }
    }

    use drasi_core::models::ElementValue;

    #[test]
    fn ticket_validation_accepts_high_bit_epoch() {
        let ticket = SnapshotTicket {
            version: SNAPSHOT_MANIFEST_VERSION,
            ticket_id: "ticket-1".to_string(),
            source_id: "source-1".to_string(),
            source_identity: "identity-1".to_string(),
            org_id: "00D000000000001AAA".to_string(),
            objects: vec!["Account".to_string(), "Contact".to_string()],
            epoch: (1u128 << 127) + 7,
            head_ordinal: 42,
            manifest_rel_path: "snapshots/ticket-1/manifest.msgpack".to_string(),
            expires_at_ms: u64::MAX,
            checksum_sha256: "a".repeat(64),
        };

        ticket
            .validate_basic("source-1", 1)
            .expect("high-bit epoch ticket should validate");
    }

    #[test]
    fn framed_snapshot_round_trip() -> Result<()> {
        let mut bytes = Vec::new();
        let first = sample_record();
        let second = GraphRecord {
            object: "Account".to_string(),
            id: "001000000000001AAA".to_string(),
            properties: BTreeMap::from([("Name".to_string(), ElementValue::String("Acme".into()))]),
            relationships: Vec::new(),
            observed_at_ms: 1_700_000_000_100,
            schema_revision: "schema-v2".to_string(),
        };

        write_snapshot_record(&mut bytes, &first, 1024)?;
        write_snapshot_record(&mut bytes, &second, 1024)?;

        let mut cursor = std::io::Cursor::new(bytes);
        assert_eq!(read_snapshot_record(&mut cursor, 1024)?, Some(first));
        assert_eq!(read_snapshot_record(&mut cursor, 1024)?, Some(second));
        assert!(read_snapshot_record(&mut cursor, 1024)?.is_none());
        Ok(())
    }

    #[test]
    fn framed_snapshot_writer_returns_payload_plus_header_size() -> Result<()> {
        let record = sample_record();
        let payload = rmp_serde::to_vec_named(&record)?;
        let mut bytes = Vec::new();
        let written = write_snapshot_record(&mut bytes, &record, 4096)?;
        assert_eq!(written, payload.len() + 4);
        assert_eq!(bytes.len(), written);
        Ok(())
    }

    #[test]
    fn framed_snapshot_rejects_partial_body() -> Result<()> {
        let record = sample_record();
        let mut bytes = Vec::new();
        write_snapshot_record(&mut bytes, &record, 1024)?;
        bytes.pop();
        let mut cursor = std::io::Cursor::new(bytes);
        let error = read_snapshot_record(&mut cursor, 1024).expect_err("partial frame should fail");
        assert!(
            format!("{error:#}").contains("ended before payload completed"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn framed_snapshot_rejects_partial_header() -> Result<()> {
        let mut cursor = std::io::Cursor::new(vec![0x00, 0x00]);
        let error =
            read_snapshot_record(&mut cursor, 1024).expect_err("partial header should fail");
        assert!(
            format!("{error:#}").contains("length header completed"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resolve_snapshot_path_rejects_leaf_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp dir");
        let root = temp.path().join("spool");
        let snapshots = root.join("snapshots");
        std::fs::create_dir_all(&snapshots).expect("create snapshots");

        let outside = temp.path().join("outside.msgpack");
        std::fs::write(&outside, b"outside").expect("write outside");
        let symlink_leaf = snapshots.join("escaped.msgpack");
        symlink(&outside, &symlink_leaf).expect("create symlink");

        let error = resolve_snapshot_path(&root, "snapshots/escaped.msgpack")
            .expect_err("symlink escape should be rejected");
        assert!(
            format!("{error:#}").contains("escapes the configured spool root"),
            "unexpected error: {error:#}"
        );
    }
}
