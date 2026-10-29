// Copyright 2026 The Drasi Authors.
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

use drasi_core::interface::{FailureSurvival, StorageDurability};

/// Fixed-size, versioned storage evidence. Codes are 0 = unknown,
/// 1 = not guaranteed, 2 = guaranteed. This is not transaction participation.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfiStorageDurability {
    pub version: u32,
    pub process_restart: u32,
    pub power_loss: u32,
    pub storage_loss: u32,
}

impl From<StorageDurability> for FfiStorageDurability {
    fn from(value: StorageDurability) -> Self {
        fn encode(value: FailureSurvival) -> u32 {
            match value {
                FailureSurvival::Unknown => 0,
                FailureSurvival::NotGuaranteed => 1,
                FailureSurvival::Guaranteed => 2,
            }
        }
        Self {
            version: 1,
            process_restart: encode(value.process_restart),
            power_loss: encode(value.power_loss),
            storage_loss: encode(value.storage_loss),
        }
    }
}

impl TryFrom<FfiStorageDurability> for StorageDurability {
    type Error = String;

    fn try_from(value: FfiStorageDurability) -> Result<Self, Self::Error> {
        fn decode(value: u32, field: &str) -> Result<FailureSurvival, String> {
            match value {
                0 => Ok(FailureSurvival::Unknown),
                1 => Ok(FailureSurvival::NotGuaranteed),
                2 => Ok(FailureSurvival::Guaranteed),
                _ => Err(format!("invalid storage durability {field} code {value}")),
            }
        }
        if value.version != 1 {
            return Err(format!(
                "unsupported storage durability version {}",
                value.version
            ));
        }
        Ok(Self {
            process_restart: decode(value.process_restart, "process_restart")?,
            power_loss: decode(value.power_loss, "power_loss")?,
            storage_loss: decode(value.storage_loss, "storage_loss")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_storage_declaration_round_trips_without_strengthening() {
        use FailureSurvival::*;
        assert_eq!(std::mem::size_of::<FfiStorageDurability>(), 16);
        for process_restart in [Unknown, NotGuaranteed, Guaranteed] {
            for power_loss in [Unknown, NotGuaranteed, Guaranteed] {
                for storage_loss in [Unknown, NotGuaranteed, Guaranteed] {
                    let expected = StorageDurability {
                        process_restart,
                        power_loss,
                        storage_loss,
                    };
                    assert_eq!(
                        StorageDurability::try_from(FfiStorageDurability::from(expected)),
                        Ok(expected)
                    );
                }
            }
        }
    }

    #[test]
    fn unsupported_versions_and_invalid_codes_are_errors() {
        for field in 0..4 {
            let mut value = FfiStorageDurability::from(StorageDurability::UNKNOWN);
            match field {
                0 => value.version = 2,
                1 => value.process_restart = 3,
                2 => value.power_loss = u32::MAX,
                _ => value.storage_loss = 3,
            }
            assert!(StorageDurability::try_from(value).is_err());
        }
    }
}
