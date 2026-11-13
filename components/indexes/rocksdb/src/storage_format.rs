// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{io::Write, path::Path};

use drasi_core::interface::IndexError;
use drasi_core_types::{ComponentId, StorageScope};

use crate::IndexDb;

const ROOT_MARKER: &str = ".drasi-rocksdb-identity-format";
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
            if entry?.file_name() != ROOT_MARKER {
                return match std::fs::read(&marker) {
                    Ok(bytes) => validate(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(incompatible(
                        "unversioned RocksDB storage; use an empty directory or matching binaries",
                    )),
                    Err(error) => Err(error),
                };
            }
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(mut file) => {
                file.write_all(ROOT_FORMAT)?;
                file.sync_all()?;
                std::fs::File::open(root)?.sync_all()
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                validate(std::fs::read(marker)?)
            }
            Err(error) => Err(error),
        }
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
