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

//! Oracle System Change Number (SCN) utilities.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Scn(pub u64);

impl Scn {
    pub fn zero() -> Self {
        Self(0)
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn to_bytes(self) -> Vec<u8> {
        self.0.to_be_bytes().to_vec()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 8 {
            return Err(anyhow!(
                "SCN bytes must be exactly 8 bytes, got {}",
                bytes.len()
            ));
        }

        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(Self(u64::from_be_bytes(array)))
    }
}

impl From<u64> for Scn {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Scn> for u64 {
    fn from(value: Scn) -> Self {
        value.0
    }
}

impl std::fmt::Display for Scn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_round_trip() {
        let scn = Scn(123_456_789);
        let parsed = Scn::from_bytes(&scn.to_bytes()).unwrap();
        assert_eq!(parsed, scn);
    }

    #[test]
    fn test_zero() {
        assert!(Scn::zero().is_zero());
        assert!(!Scn(1).is_zero());
    }
}
