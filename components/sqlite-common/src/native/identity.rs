// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The capture identity a SQLite source binds: for each captured table, the
//! columns its element IDs are derived from (configured keys, the primary
//! key, or the rowid).
//!
//! As PostgreSQL's binding covers only key columns (decision D-7), nothing
//! else is bound. Adding a table, adding, dropping or renaming a non-key
//! column, and creating or dropping indexes, views and triggers keep every
//! element ID and are allowed, from the source's own SQL or from DDL applied
//! while it is stopped. Dropping or renaming a captured table, or changing
//! its key columns, would orphan the element IDs queries already hold, so it
//! is refused with [`SourceFailure::SchemaChanged`].

use anyhow::Result;
use drasi_computation_contracts::SourceFailure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureIdentity {
    version: u32,
    /// Captured table name to its sorted key columns; empty means rowid
    /// identity.
    tables: BTreeMap<String, Vec<String>>,
}

impl CaptureIdentity {
    pub fn new(tables: &BTreeMap<String, crate::TableSchema>) -> Self {
        Self {
            version: VERSION,
            tables: tables
                .iter()
                .map(|(name, table)| {
                    let mut keys = if table.rowid_alias.is_some() {
                        Vec::new()
                    } else {
                        table.keys.clone()
                    };
                    keys.sort();
                    (name.clone(), keys)
                })
                .collect(),
        }
    }

    pub fn encode(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// The identity recorded in a journal. One this version cannot read
    /// binds element IDs it cannot check.
    pub fn decode(recorded: &str) -> Result<Self> {
        serde_json::from_str::<Self>(recorded)
            .ok()
            .filter(|identity| identity.version == VERSION)
            .ok_or_else(|| {
                SourceFailure::SchemaChanged(
                    "the journal records a capture identity this version cannot read".into(),
                )
                .into()
            })
    }

    /// Checks that this (current) identity keeps every element ID `bound`
    /// produced: each bound table still exists with the same key columns.
    /// New tables are additive.
    pub fn check_extends(&self, bound: &Self) -> Result<()> {
        for (table, keys) in &bound.tables {
            let detail = match self.tables.get(table) {
                None => format!("captured table '{table}' was dropped or renamed"),
                Some(current) if current != keys => format!(
                    "the identity columns of captured table '{table}' changed from {} to {}",
                    describe(keys),
                    describe(current)
                ),
                Some(_) => continue,
            };
            return Err(SourceFailure::SchemaChanged(detail).into());
        }
        Ok(())
    }
}

fn describe(keys: &[String]) -> String {
    if keys.is_empty() {
        "the rowid".into()
    } else {
        format!("({})", keys.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::source_failure;

    fn identity(tables: &[(&str, &[&str])]) -> CaptureIdentity {
        CaptureIdentity::new(
            &tables
                .iter()
                .map(|(name, keys)| {
                    (
                        (*name).to_owned(),
                        crate::TableSchema {
                            columns: Vec::new(),
                            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
                            rowid_alias: keys.is_empty().then_some("rowid"),
                        },
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn only_dropped_tables_and_changed_keys_break_the_identity() -> Result<()> {
        let bound = identity(&[("orders", &["region", "id"]), ("log", &[])]);
        // A new table is additive; key order is irrelevant.
        identity(&[
            ("orders", &["id", "region"]),
            ("log", &[]),
            ("audit", &["id"]),
        ])
        .check_extends(&bound)?;
        for (changed, expected) in [
            (identity(&[("orders", &["id"]), ("log", &[])]), "changed"),
            (
                identity(&[("orders", &["id", "region"]), ("log", &["id"])]),
                "rowid",
            ),
            (identity(&[("orders", &["id", "region"])]), "dropped"),
        ] {
            let error = changed.check_extends(&bound).unwrap_err();
            assert!(
                matches!(
                    source_failure(&error),
                    Some(SourceFailure::SchemaChanged(_))
                ),
                "{error:#}"
            );
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        assert_eq!(CaptureIdentity::decode(&bound.encode()?)?, bound);
        assert!(CaptureIdentity::decode("[[\"table\",\"orders\"]]").is_err());
        Ok(())
    }
}
