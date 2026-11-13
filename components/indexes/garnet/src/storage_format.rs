// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_core::interface::IndexError;
use redis::{aio::MultiplexedConnection, AsyncCommands};

const FORMAT_KEY: &str = "drasi:storage-identity-format";
const FORMAT_VERSION: u32 = 2;

fn incompatible() -> IndexError {
    IndexError::other(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "unsupported Garnet identity storage format; use an empty database or matching binaries",
    ))
}

pub(crate) async fn ensure_current(connection: &MultiplexedConnection) -> Result<(), IndexError> {
    let mut connection = connection.clone();
    let version: Option<u32> = connection
        .get(FORMAT_KEY)
        .await
        .map_err(IndexError::other)?;
    if let Some(version) = version {
        return if version == FORMAT_VERSION {
            Ok(())
        } else {
            Err(incompatible())
        };
    }
    let mut cursor = 0u64;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("COUNT")
            .arg(256)
            .query_async(&mut connection)
            .await
            .map_err(IndexError::other)?;
        if keys.iter().any(|key| {
            [
                "ei:{",
                "eai:{",
                "archive:{",
                "ari:{",
                "fqi:{",
                "ss:{",
                "outbox:{",
                "outbox_data:{",
                "live:{",
                "metadata:{",
            ]
            .iter()
            .any(|prefix| key.starts_with(prefix))
        }) {
            let version: Option<u32> = connection
                .get(FORMAT_KEY)
                .await
                .map_err(IndexError::other)?;
            return if version == Some(FORMAT_VERSION) {
                Ok(())
            } else {
                Err(incompatible())
            };
        }
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    let _: bool = connection
        .set_nx(FORMAT_KEY, FORMAT_VERSION)
        .await
        .map_err(IndexError::other)?;
    let version: u32 = connection
        .get(FORMAT_KEY)
        .await
        .map_err(IndexError::other)?;
    if version != FORMAT_VERSION {
        return Err(incompatible());
    }
    Ok(())
}
