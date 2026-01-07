// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! CDC operation decoder

use anyhow::{anyhow, Result};

/// CDC Operation types from MS SQL CDC
///
/// These are the values found in the __$operation column of CDC change tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum CdcOperation {
    /// Delete operation (before image)
    Delete = 1,
    /// Insert operation (after image)
    Insert = 2,
    /// Update operation - before image (rarely used)
    UpdateBefore = 3,
    /// Update operation - after image
    UpdateAfter = 4,
}

impl CdcOperation {
    /// Parse operation from MS SQL CDC __$operation value
    pub fn from_i32(value: i32) -> Result<Self> {
        match value {
            1 => Ok(Self::Delete),
            2 => Ok(Self::Insert),
            3 => Ok(Self::UpdateBefore),
            4 => Ok(Self::UpdateAfter),
            _ => Err(anyhow!("Invalid CDC operation value: {}", value)),
        }
    }

    /// Check if this is an update operation (before or after)
    pub fn is_update(&self) -> bool {
        matches!(self, Self::UpdateBefore | Self::UpdateAfter)
    }

    /// Check if this is the after image of an update
    pub fn is_update_after(&self) -> bool {
        matches!(self, Self::UpdateAfter)
    }

    /// Get human-readable name
    pub fn name(&self) -> &'static str {
        match self {
            Self::Delete => "DELETE",
            Self::Insert => "INSERT",
            Self::UpdateBefore => "UPDATE_BEFORE",
            Self::UpdateAfter => "UPDATE_AFTER",
        }
    }
}

impl std::fmt::Display for CdcOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// CDC column names (prefixed with __$)
pub mod cdc_columns {
    /// LSN of the change
    pub const START_LSN: &str = "__$start_lsn";
    
    /// End LSN (always NULL for captured changes)
    pub const END_LSN: &str = "__$end_lsn";
    
    /// Sequence value within transaction
    pub const SEQVAL: &str = "__$seqval";
    
    /// Operation type (1=delete, 2=insert, 3=update before, 4=update after)
    pub const OPERATION: &str = "__$operation";
    
    /// Bit mask showing which columns were updated
    pub const UPDATE_MASK: &str = "__$update_mask";

    /// Check if a column name is a CDC metadata column
    pub fn is_metadata_column(name: &str) -> bool {
        name.starts_with("__$")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_from_i32() {
        assert_eq!(CdcOperation::from_i32(1).unwrap(), CdcOperation::Delete);
        assert_eq!(CdcOperation::from_i32(2).unwrap(), CdcOperation::Insert);
        assert_eq!(
            CdcOperation::from_i32(3).unwrap(),
            CdcOperation::UpdateBefore
        );
        assert_eq!(
            CdcOperation::from_i32(4).unwrap(),
            CdcOperation::UpdateAfter
        );

        assert!(CdcOperation::from_i32(5).is_err());
        assert!(CdcOperation::from_i32(0).is_err());
    }

    #[test]
    fn test_is_update() {
        assert!(!CdcOperation::Delete.is_update());
        assert!(!CdcOperation::Insert.is_update());
        assert!(CdcOperation::UpdateBefore.is_update());
        assert!(CdcOperation::UpdateAfter.is_update());
    }

    #[test]
    fn test_is_update_after() {
        assert!(!CdcOperation::Delete.is_update_after());
        assert!(!CdcOperation::Insert.is_update_after());
        assert!(!CdcOperation::UpdateBefore.is_update_after());
        assert!(CdcOperation::UpdateAfter.is_update_after());
    }

    #[test]
    fn test_operation_name() {
        assert_eq!(CdcOperation::Delete.name(), "DELETE");
        assert_eq!(CdcOperation::Insert.name(), "INSERT");
        assert_eq!(CdcOperation::UpdateBefore.name(), "UPDATE_BEFORE");
        assert_eq!(CdcOperation::UpdateAfter.name(), "UPDATE_AFTER");
    }

    #[test]
    fn test_is_metadata_column() {
        assert!(cdc_columns::is_metadata_column("__$operation"));
        assert!(cdc_columns::is_metadata_column("__$start_lsn"));
        assert!(cdc_columns::is_metadata_column("__$seqval"));
        assert!(!cdc_columns::is_metadata_column("customer_id"));
        assert!(!cdc_columns::is_metadata_column("name"));
    }
}
