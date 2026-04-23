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

//! Opaque position type for source replay and resume.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;

/// Error returned by [`SequencePosition::try_from_bytes`] when the input
/// exceeds [`MAX_POSITION_BYTES`].
#[derive(Debug, Clone)]
pub struct SequencePositionError {
    /// Length of the slice that was too large.
    pub actual_len: usize,
}

impl fmt::Display for SequencePositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SequencePosition: {} bytes exceeds maximum {MAX_POSITION_BYTES}",
            self.actual_len,
        )
    }
}

impl std::error::Error for SequencePositionError {}

/// Maximum bytes for a source position.
///
/// MSSQL: 20 (start_lsn + seqval), Postgres: 8 (WAL LSN), Kafka: 8.
/// 24 bytes provides headroom for future sources.
pub const MAX_POSITION_BYTES: usize = 24;

/// Opaque, source-defined position marker for replay and resume.
///
/// Uses a fixed-size inline buffer — no heap allocation, [`Copy`]-able.
/// Comparison is lexicographic over the active bytes `data[0..len]`.
///
/// # Source Encodings
///
/// - **Postgres**: 8 bytes (WAL LSN as big-endian u64)
/// - **MSSQL**: 20 bytes (start_lsn ∥ seqval, each 10 bytes)
///
/// # Invariant
///
/// All positions emitted by a single source instance use the same byte length.
/// Cross-source position comparison is undefined.
#[derive(Clone, Copy)]
pub struct SequencePosition {
    data: [u8; MAX_POSITION_BYTES],
    len: u8,
}

impl SequencePosition {
    /// Create from a byte slice. Panics if `bytes.len() > MAX_POSITION_BYTES`.
    ///
    /// For untrusted input (storage, wire data), prefer [`try_from_bytes`](Self::try_from_bytes).
    pub fn from_bytes(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() <= MAX_POSITION_BYTES,
            "SequencePosition: {} bytes exceeds maximum {}",
            bytes.len(),
            MAX_POSITION_BYTES,
        );
        let mut data = [0u8; MAX_POSITION_BYTES];
        data[..bytes.len()].copy_from_slice(bytes);
        Self {
            data,
            len: bytes.len() as u8,
        }
    }

    /// Fallible version of [`from_bytes`](Self::from_bytes) for untrusted input.
    ///
    /// Returns `Err` if `bytes.len() > MAX_POSITION_BYTES`.
    pub fn try_from_bytes(bytes: &[u8]) -> Result<Self, SequencePositionError> {
        if bytes.len() > MAX_POSITION_BYTES {
            return Err(SequencePositionError {
                actual_len: bytes.len(),
            });
        }
        let mut data = [0u8; MAX_POSITION_BYTES];
        data[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            data,
            len: bytes.len() as u8,
        })
    }

    /// Access the active bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    /// Create from a `u64` value (stored as 8 big-endian bytes).
    pub fn from_u64(val: u64) -> Self {
        Self::from_bytes(&val.to_be_bytes())
    }

    /// Try to extract as a `u64`. Returns `None` unless the position is exactly 8 bytes.
    pub fn to_u64(&self) -> Option<u64> {
        if self.len == 8 {
            let arr: [u8; 8] = self.data[..8].try_into().expect("len is 8");
            Some(u64::from_be_bytes(arr))
        } else {
            None
        }
    }

    /// Length of the active position data.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether this position has zero-length data.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Default for SequencePosition {
    fn default() -> Self {
        Self {
            data: [0u8; MAX_POSITION_BYTES],
            len: 0,
        }
    }
}

// -- Comparison (lexicographic on active bytes) --

impl PartialEq for SequencePosition {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for SequencePosition {}

impl PartialOrd for SequencePosition {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SequencePosition {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl std::hash::Hash for SequencePosition {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

// -- Display / Debug --

impl fmt::Display for SequencePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SequencePosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SequencePosition({self})")
    }
}

// -- Serde (serializes only the active bytes as a byte array) --

impl Serialize for SequencePosition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.as_bytes())
    }
}

impl<'de> Deserialize<'de> for SequencePosition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SequencePosition;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "a byte array of at most {MAX_POSITION_BYTES} bytes",
                )
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                if v.len() > MAX_POSITION_BYTES {
                    return Err(E::invalid_length(
                        v.len(),
                        &format!("at most {MAX_POSITION_BYTES} bytes").as_str(),
                    ));
                }
                Ok(SequencePosition::from_bytes(v))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&v)
            }

            // Support sequence deserialization (JSON arrays)
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut bytes = Vec::with_capacity(MAX_POSITION_BYTES);
                while let Some(byte) = seq.next_element::<u8>()? {
                    bytes.push(byte);
                }
                if bytes.len() > MAX_POSITION_BYTES {
                    return Err(serde::de::Error::invalid_length(
                        bytes.len(),
                        &format!("at most {MAX_POSITION_BYTES} bytes").as_str(),
                    ));
                }
                Ok(SequencePosition::from_bytes(&bytes))
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

// -- Conversion helpers --

impl From<u64> for SequencePosition {
    fn from(val: u64) -> Self {
        Self::from_u64(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_u64_round_trip() {
        let pos = SequencePosition::from_u64(42);
        assert_eq!(pos.to_u64(), Some(42));
        assert_eq!(pos.len(), 8);
    }

    #[test]
    fn test_from_u64_zero() {
        let pos = SequencePosition::from_u64(0);
        assert_eq!(pos.to_u64(), Some(0));
    }

    #[test]
    fn test_from_u64_max() {
        let pos = SequencePosition::from_u64(u64::MAX);
        assert_eq!(pos.to_u64(), Some(u64::MAX));
    }

    #[test]
    fn test_from_bytes_20() {
        let bytes: Vec<u8> = (0..20).collect();
        let pos = SequencePosition::from_bytes(&bytes);
        assert_eq!(pos.as_bytes(), &bytes[..]);
        assert_eq!(pos.len(), 20);
        assert_eq!(pos.to_u64(), None); // not 8 bytes
    }

    #[test]
    fn test_ordering_u64() {
        let a = SequencePosition::from_u64(10);
        let b = SequencePosition::from_u64(20);
        assert!(a < b);
    }

    #[test]
    fn test_ordering_same_length() {
        let a = SequencePosition::from_bytes(&[0x00, 0x01]);
        let b = SequencePosition::from_bytes(&[0x00, 0x02]);
        assert!(a < b);
    }

    #[test]
    fn test_ordering_different_length() {
        // Lexicographic: shorter prefix is less
        let a = SequencePosition::from_bytes(&[0x00, 0x01]);
        let b = SequencePosition::from_bytes(&[0x00, 0x01, 0x00]);
        assert!(a < b);
    }

    #[test]
    fn test_equality() {
        let a = SequencePosition::from_u64(42);
        let b = SequencePosition::from_u64(42);
        assert_eq!(a, b);
    }

    #[test]
    fn test_display_hex() {
        let pos = SequencePosition::from_bytes(&[0xAB, 0xCD, 0xEF]);
        assert_eq!(format!("{pos}"), "abcdef");
    }

    #[test]
    fn test_debug() {
        let pos = SequencePosition::from_u64(1);
        let debug_str = format!("{pos:?}");
        assert!(debug_str.starts_with("SequencePosition("));
    }

    #[test]
    fn test_default_is_empty() {
        let pos = SequencePosition::default();
        assert!(pos.is_empty());
        assert_eq!(pos.len(), 0);
        assert_eq!(pos.as_bytes(), &[] as &[u8]);
    }

    #[test]
    fn test_copy_semantics() {
        let a = SequencePosition::from_u64(100);
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_serde_json_round_trip() {
        let pos = SequencePosition::from_u64(42);
        let json = serde_json::to_string(&pos).unwrap();
        let deserialized: SequencePosition = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, deserialized);
    }

    #[test]
    fn test_serde_json_20_bytes() {
        let bytes: Vec<u8> = (0..20).collect();
        let pos = SequencePosition::from_bytes(&bytes);
        let json = serde_json::to_string(&pos).unwrap();
        let deserialized: SequencePosition = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, deserialized);
    }

    #[test]
    fn test_from_u64_trait() {
        let pos: SequencePosition = 42u64.into();
        assert_eq!(pos.to_u64(), Some(42));
    }

    #[test]
    #[should_panic(expected = "exceeds maximum")]
    fn test_from_bytes_too_large() {
        SequencePosition::from_bytes(&[0u8; 25]);
    }

    #[test]
    fn test_hash_consistency() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SequencePosition::from_u64(42));
        assert!(set.contains(&SequencePosition::from_u64(42)));
        assert!(!set.contains(&SequencePosition::from_u64(43)));
    }

    #[test]
    fn test_try_from_bytes_ok() {
        let bytes: Vec<u8> = (0..20).collect();
        let pos = SequencePosition::try_from_bytes(&bytes).unwrap();
        assert_eq!(pos.as_bytes(), &bytes[..]);
    }

    #[test]
    fn test_try_from_bytes_too_large() {
        let err = SequencePosition::try_from_bytes(&[0u8; 25]).unwrap_err();
        assert_eq!(err.actual_len, 25);
        assert!(err.to_string().contains("25 bytes exceeds maximum"));
    }

    #[test]
    fn test_try_from_bytes_at_max() {
        let pos = SequencePosition::try_from_bytes(&[0xFFu8; MAX_POSITION_BYTES]).unwrap();
        assert_eq!(pos.len(), MAX_POSITION_BYTES);
    }
}
