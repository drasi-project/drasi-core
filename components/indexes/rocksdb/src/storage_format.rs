// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use drasi_core::interface::IndexError;
use drasi_core_types::{ComponentId, StorageScope};

use crate::IndexDb;

const ROOT_MARKER: &str = ".drasi-rocksdb-identity-format";
const STAGED_PREFIX: &str = ".drasi-rocksdb-identity-format.staged-";
const ROOT_FORMAT: &[u8] = b"drasi-rocksdb-identity-v2\n";
const DATABASE_MARKER: &[u8] = b"\0drasi:storage-identity-format";
const VERSION: u32 = 2;

fn incompatible(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

pub(crate) fn prepare_root(root: &Path) -> Result<(), IndexError> {
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(root)?;
        let marker = root.join(ROOT_MARKER);
        let validate = |bytes: Vec<u8>| {
            if bytes == ROOT_FORMAT {
                Ok(())
            } else {
                Err(incompatible("unsupported RocksDB root identity format"))
            }
        };
        match std::fs::read(&marker) {
            Ok(bytes) => return validate(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for entry in std::fs::read_dir(root)? {
            let name = entry?.file_name();
            // A concurrent preparer's staged marker is not existing storage.
            let staged = name
                .to_str()
                .is_some_and(|name| name.starts_with(STAGED_PREFIX));
            if name != ROOT_MARKER && !staged {
                return match std::fs::read(&marker) {
                    Ok(bytes) => validate(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(incompatible(
                        "unversioned RocksDB storage; use an empty directory or matching binaries",
                    )),
                    Err(error) => Err(error),
                };
            }
        }
        // Publish the marker atomically: a concurrent preparer either finds
        // no marker or the complete one, never a created-but-unwritten file.
        // Every preparer stages the same constant, so a replacing rename is
        // harmless.
        static STAGED: AtomicU64 = AtomicU64::new(0);
        let staged = root.join(format!(
            "{STAGED_PREFIX}{}-{}",
            std::process::id(),
            STAGED.fetch_add(1, Ordering::Relaxed)
        ));
        let published = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(ROOT_FORMAT)?;
            file.sync_all()?;
            std::fs::rename(&staged, &marker)?;
            std::fs::File::open(root)?.sync_all()
        })();
        if published.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        published?;
        validate(std::fs::read(marker)?)
    })();
    result.map_err(IndexError::other)
}

pub(crate) fn verify_database(
    db: &IndexDb,
    scope: &StorageScope,
    component: &ComponentId,
    columns: &[String],
) -> Result<(), IndexError> {
    let expected = serde_json::to_vec(&(VERSION, scope, component)).map_err(IndexError::other)?;
    match db.get(DATABASE_MARKER).map_err(IndexError::other)? {
        Some(stored) if stored == expected => return Ok(()),
        Some(_) => {
            return Err(IndexError::other(incompatible(
                "RocksDB identity format or owner does not match the requested scope and component",
            )))
        }
        None => {}
    }
    for column in columns {
        let cf = db.cf_handle(column).ok_or(IndexError::CorruptedData)?;
        if db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .next()
            .transpose()
            .map_err(IndexError::other)?
            .is_some()
        {
            return Err(IndexError::other(incompatible(
                "nonempty RocksDB database has no current identity format",
            )));
        }
    }
    let mut options = rocksdb::WriteOptions::default();
    options.set_sync(true);
    db.put_opt(DATABASE_MARKER, expected, &options)
        .map_err(IndexError::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queries sharing a RocksDB root are initialized concurrently; none may
    /// observe the marker between its creation and its contents.
    #[test]
    fn concurrent_preparers_never_observe_a_partial_root_marker() {
        for _ in 0..50 {
            let root = tempfile::tempdir().unwrap();
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let workers: Vec<_> = (0..8)
                    .map(|_| {
                        scope.spawn(|| {
                            barrier.wait();
                            prepare_root(root.path())
                        })
                    })
                    .collect();
                for worker in workers {
                    worker.join().unwrap().expect("prepare shared root");
                }
            });
            assert_eq!(
                std::fs::read(root.path().join(ROOT_MARKER)).unwrap(),
                ROOT_FORMAT
            );
            let entries: Vec<_> = std::fs::read_dir(root.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(
                entries,
                [std::ffi::OsString::from(ROOT_MARKER)],
                "no staged file remains"
            );
        }
    }

    #[test]
    fn a_different_root_format_is_still_refused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(ROOT_MARKER),
            b"drasi-rocksdb-identity-v1\n",
        )
        .unwrap();
        assert!(prepare_root(root.path()).is_err());
    }
}
