use anyhow::{ensure, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub const POSITION_VERSION_V1: u8 = 1;
pub const POSITION_ENCODED_BYTES: usize = 1 + 16 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPosition {
    pub epoch: u128,
    pub ordinal: u64,
}

impl LocalPosition {
    pub fn new(epoch: u128, ordinal: u64) -> Self {
        Self { epoch, ordinal }
    }

    pub fn encode(self) -> Bytes {
        let mut bytes = Vec::with_capacity(POSITION_ENCODED_BYTES);
        bytes.push(POSITION_VERSION_V1);
        bytes.extend_from_slice(&self.epoch.to_be_bytes());
        bytes.extend_from_slice(&self.ordinal.to_be_bytes());
        Bytes::from(bytes)
    }

    pub fn decode(bytes: &Bytes) -> Result<Self> {
        ensure!(
            bytes.len() == POSITION_ENCODED_BYTES,
            "invalid source position byte length"
        );
        ensure!(
            bytes[0] == POSITION_VERSION_V1,
            "unsupported source position version {}",
            bytes[0]
        );

        let mut epoch = [0u8; 16];
        epoch.copy_from_slice(&bytes[1..17]);

        let mut ordinal = [0u8; 8];
        ordinal.copy_from_slice(&bytes[17..25]);

        Ok(Self {
            epoch: u128::from_be_bytes(epoch),
            ordinal: u64::from_be_bytes(ordinal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_round_trip_and_lexicographic_order() {
        let first = LocalPosition::new(42, 7).encode();
        let second = LocalPosition::new(42, 8).encode();
        let third = LocalPosition::new(43, 1).encode();

        assert_eq!(
            LocalPosition::decode(&first).unwrap(),
            LocalPosition::new(42, 7)
        );
        assert!(first < second);
        assert!(second < third);
    }
}
