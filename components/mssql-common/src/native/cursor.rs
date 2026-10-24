// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use drasi_computation_contracts::InputPositionComparator;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// A real CDC position. `row = None` covers the complete commit LSN; a row
/// position includes the CDC sequence, table object and emitted image ordinal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub version: u8,
    pub binding: [u8; 32],
    pub lsn: [u8; 10],
    pub row: Option<([u8; 10], i32, u8)>,
}

impl Cursor {
    pub fn boundary(binding: [u8; 32], lsn: [u8; 10]) -> Self {
        Self {
            version: 1,
            binding,
            lsn,
            row: None,
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 2048, "SQL Server cursor exceeds limit");
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported SQL Server cursor");
        if let Some((_, object, ordinal)) = self.row {
            ensure!(
                object > 0 && ordinal <= 1,
                "invalid SQL Server CDC row position"
            );
        }
        Ok(())
    }
    pub fn compare(&self, other: &Self) -> Result<Ordering> {
        self.validate()?;
        other.validate()?;
        ensure!(
            self.binding == other.binding,
            "SQL Server database/catalog/key binding changed; explicit query reset required"
        );
        Ok(self
            .lsn
            .cmp(&other.lsn)
            .then_with(|| match (&self.row, &other.row) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            }))
    }
}

pub struct CursorComparator;
impl InputPositionComparator for CursorComparator {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        Ok(Cursor::decode(event)?.compare(&Cursor::decode(handled)?)? == Ordering::Greater)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn row_order_covers_partial_transactions_and_snapshot_boundaries() {
        let boundary = Cursor::boundary([7; 32], [1; 10]);
        let mut first = boundary.clone();
        first.row = Some(([2; 10], 4, 0));
        let mut second = first.clone();
        second.row.as_mut().unwrap().2 = 1;
        assert_eq!(first.compare(&second).unwrap(), Ordering::Less);
        assert_eq!(second.compare(&boundary).unwrap(), Ordering::Less);
        assert!(!CursorComparator
            .is_after(&first.encode().unwrap(), &first.encode().unwrap())
            .unwrap());
        assert_eq!(Cursor::decode(&second.encode().unwrap()).unwrap(), second);
        let mut incompatible = second.clone();
        incompatible.binding = [8; 32];
        assert!(second.compare(&incompatible).is_err());
        incompatible.version = 2;
        assert!(incompatible.encode().is_err());
        assert!(Cursor::decode(b"{\"version\":1}").is_err());
    }
}
