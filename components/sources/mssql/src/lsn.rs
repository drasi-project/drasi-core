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

//! LSN (Log Sequence Number) handling for MS SQL CDC
//!
//! MS SQL LSN is a binary(10) value that needs special handling for:
//! - Serialization to/from StateStore
//! - Conversion to/from hex strings (for config and SQL queries)
//! - Ordering comparison

use anyhow::{anyhow, Result};
use std::cmp::Ordering;

/// MS SQL Log Sequence Number (LSN)
///
/// LSN is a 10-byte binary value that represents a position in the transaction log.
/// It's used to track CDC progress and coordinate between bootstrap and streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lsn([u8; 10]);

impl Lsn {
    /// Create a zero LSN (minimum possible value)
    pub fn zero() -> Self {
        Lsn([0u8; 10])
    }

    /// Create LSN from hex string
    ///
    /// # Arguments
    /// * `hex` - Hex string like "0x00000027000000680004" or "00000027000000680004"
    ///
    /// # Examples
    /// ```
    /// # use drasi_source_mssql::Lsn;
    /// let lsn = Lsn::from_hex("0x00000027000000680004").unwrap();
    /// ```
    pub fn from_hex(hex: &str) -> Result<Self> {
        // Remove 0x prefix if present
        let hex_clean = hex.strip_prefix("0x").unwrap_or(hex);

        // Validate length (20 hex chars = 10 bytes)
        if hex_clean.len() != 20 {
            return Err(anyhow!(
                "LSN hex string must be 20 characters (10 bytes), got {}",
                hex_clean.len()
            ));
        }

        // Decode hex to bytes
        let mut bytes = [0u8; 10];
        for (i, chunk) in hex_clean.as_bytes().chunks(2).enumerate() {
            let hex_byte = std::str::from_utf8(chunk)
                .map_err(|e| anyhow!("Invalid UTF-8 in hex string: {}", e))?;
            bytes[i] = u8::from_str_radix(hex_byte, 16)
                .map_err(|e| anyhow!("Invalid hex character: {}", e))?;
        }

        Ok(Lsn(bytes))
    }

    /// Convert LSN to hex string
    ///
    /// Returns a hex string with "0x" prefix, suitable for SQL queries and logging.
    ///
    /// # Examples
    /// ```
    /// # use drasi_source_mssql::Lsn;
    /// let lsn = Lsn::zero();
    /// assert_eq!(lsn.to_hex(), "0x00000000000000000000");
    /// ```
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }

    /// Create LSN from raw bytes (from StateStore or SQL query result)
    ///
    /// # Arguments
    /// * `bytes` - Exactly 10 bytes
    ///
    /// # Errors
    /// Returns error if bytes length is not exactly 10
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 10 {
            return Err(anyhow!("LSN must be exactly 10 bytes, got {}", bytes.len()));
        }

        let mut arr = [0u8; 10];
        arr.copy_from_slice(bytes);
        Ok(Lsn(arr))
    }

    /// Convert LSN to bytes for StateStore persistence
    ///
    /// Returns a Vec<u8> of length 10
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    /// Get raw byte array
    pub fn as_bytes(&self) -> &[u8; 10] {
        &self.0
    }
}

impl PartialOrd for Lsn {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Lsn {
    fn cmp(&self, other: &Self) -> Ordering {
        // LSN is a big-endian unsigned integer
        self.0.cmp(&other.0)
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsn_zero() {
        let lsn = Lsn::zero();
        assert_eq!(lsn.to_hex(), "0x00000000000000000000");
        assert_eq!(lsn.to_bytes(), vec![0u8; 10]);
    }

    #[test]
    fn test_lsn_from_hex_with_prefix() {
        let lsn = Lsn::from_hex("0x00000027000000680004").unwrap();
        assert_eq!(lsn.to_hex(), "0x00000027000000680004");
    }

    #[test]
    fn test_lsn_from_hex_without_prefix() {
        let lsn = Lsn::from_hex("00000027000000680004").unwrap();
        assert_eq!(lsn.to_hex(), "0x00000027000000680004");
    }

    #[test]
    fn test_lsn_from_hex_invalid_length() {
        let result = Lsn::from_hex("0x123");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must be 20 characters"));
    }

    #[test]
    fn test_lsn_from_hex_invalid_chars() {
        let result = Lsn::from_hex("0xGGGGGGGGGGGGGGGGGGGG");
        assert!(result.is_err());
    }

    #[test]
    fn test_lsn_from_bytes() {
        let bytes = vec![0x00, 0x00, 0x00, 0x27, 0x00, 0x00, 0x00, 0x68, 0x00, 0x04];
        let lsn = Lsn::from_bytes(&bytes).unwrap();
        assert_eq!(lsn.to_hex(), "0x00000027000000680004");
    }

    #[test]
    fn test_lsn_from_bytes_invalid_length() {
        let bytes = vec![0x00, 0x01, 0x02];
        let result = Lsn::from_bytes(&bytes);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must be exactly 10 bytes"));
    }

    #[test]
    fn test_lsn_roundtrip() {
        let original = Lsn::from_hex("0x00000027000000680004").unwrap();
        let bytes = original.to_bytes();
        let restored = Lsn::from_bytes(&bytes).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn test_lsn_ordering() {
        let lsn1 = Lsn::from_hex("0x00000027000000680004").unwrap();
        let lsn2 = Lsn::from_hex("0x00000027000000680005").unwrap();
        let lsn3 = Lsn::from_hex("0x00000028000000000000").unwrap();

        assert!(lsn1 < lsn2);
        assert!(lsn2 < lsn3);
        assert!(lsn1 < lsn3);
    }

    #[test]
    fn test_lsn_equality() {
        let lsn1 = Lsn::from_hex("0x00000027000000680004").unwrap();
        let lsn2 = Lsn::from_hex("0x00000027000000680004").unwrap();
        let lsn3 = Lsn::from_hex("0x00000027000000680005").unwrap();

        assert_eq!(lsn1, lsn2);
        assert_ne!(lsn1, lsn3);
    }

    #[test]
    fn test_lsn_display() {
        let lsn = Lsn::from_hex("0x00000027000000680004").unwrap();
        assert_eq!(format!("{lsn}"), "0x00000027000000680004");
    }
}
