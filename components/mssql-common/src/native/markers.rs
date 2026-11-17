// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Snapshot barrier markers owned by one source (decision D-1).
//!
//! A coordinated snapshot inserts one row into the administrator-provisioned
//! barrier table and deletes it after the attempt. Its `token` is a
//! `uniqueidentifier` whose first 64 bits are derived from the graph and the
//! source (`SHA-256("drasi.mssql/marker/v1", graph, source)`), and whose last
//! 64 bits are random. Every marker this source owns therefore starts with
//! the same 18-character text prefix, nothing else does, and erasing removal
//! can delete exactly this source's orphans (left by a crash or a failed
//! cleanup) without a persisted list. Repeating the delete is harmless.

use super::identifiers::TableName;
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};

/// The uppercase text prefix (`XXXXXXXX-XXXX-XXXX`) of every marker token
/// owned by `source` in `graph`.
pub fn marker_prefix(graph: &str, source: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"drasi.mssql/marker/v1\0");
    hash.update(graph.as_bytes());
    hash.update(b"\0");
    hash.update(source.as_bytes());
    let digest = hash.finalize();
    let hex = hex::encode_upper(&digest[..8]);
    format!("{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16])
}

/// A new marker token (canonical uppercase text) with `prefix`.
pub fn new_marker(prefix: &str) -> Result<String> {
    ensure!(
        prefix.len() == 18 && prefix.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
        "invalid SQL Server marker prefix"
    );
    let random = uuid::Uuid::new_v4();
    let tail = hex::encode_upper(&random.as_bytes()[8..]);
    Ok(format!("{prefix}-{}-{}", &tail[..4], &tail[4..]))
}

/// Whether `token` (any case) is owned by `prefix`.
pub fn owns(prefix: &str, token: &str) -> bool {
    token.len() == 36 && token[..18].eq_ignore_ascii_case(prefix)
}

/// The statement deleting every marker of `prefix` from `barrier`; `@P1` is
/// the prefix. A missing barrier table deletes nothing.
pub fn erase_sql(barrier: &TableName) -> String {
    let table = barrier.sql();
    format!(
        "DECLARE @erased BIGINT = 0; \
         IF OBJECT_ID(@P2, N'U') IS NOT NULL \
         BEGIN \
             DELETE FROM {table} WHERE UPPER(LEFT(CONVERT(char(36), token), 18)) = @P1; \
             SET @erased = @@ROWCOUNT; \
         END; \
         SELECT @erased;"
    )
}

/// Deletes this source's markers through a session's raw client. Returns
/// the number of rows removed.
pub async fn erase_markers(
    client: &mut crate::client::SqlClient,
    barrier: &TableName,
    prefix: &str,
) -> Result<u64> {
    let mut query = tiberius::Query::new(erase_sql(barrier));
    query.bind(prefix.to_owned());
    query.bind(barrier.sql());
    let row = query
        .query(client)
        .await?
        .into_row()
        .await?
        .ok_or_else(|| anyhow::anyhow!("SQL Server returned no erase count"))?;
    Ok(u64::try_from(row.try_get::<i64, _>(0)?.unwrap_or(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_owned_by_exactly_one_source_and_unique_per_attempt() {
        let prefix = marker_prefix("graph", "source-a");
        assert_eq!(prefix.len(), 18);
        assert_eq!(prefix, marker_prefix("graph", "source-a"), "stable");
        assert_ne!(prefix, marker_prefix("graph", "source-b"));
        assert_ne!(prefix, marker_prefix("other-graph", "source-a"));
        let first = new_marker(&prefix).unwrap();
        let second = new_marker(&prefix).unwrap();
        assert_ne!(first, second);
        uuid::Uuid::parse_str(&first).expect("a valid uniqueidentifier");
        assert!(owns(&prefix, &first) && owns(&prefix, &first.to_lowercase()));
        assert!(!owns(&marker_prefix("graph", "source-b"), &first));
        assert!(new_marker("not-a-prefix").is_err());
    }

    #[test]
    fn erase_deletes_only_this_prefix_and_tolerates_a_missing_table() {
        let sql = erase_sql(&TableName::parse("[ops].[Drasi Barrier]").unwrap());
        assert!(sql.contains("DELETE FROM [ops].[Drasi Barrier] WHERE UPPER(LEFT(CONVERT(char(36), token), 18)) = @P1"));
        assert!(sql.contains("IF OBJECT_ID(@P2, N'U') IS NOT NULL"));
    }
}
