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

/// Failures against which already committed data may be protected.
/// Power-loss guarantees assume that the filesystem and device honor sync requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureMode {
    ProcessRestart,
    PowerLoss,
    /// Permanent loss of the local machine or storage, not just a process restart.
    StorageLoss,
}

/// Unknown is not evidence of either durability or an explicitly volatile store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureSurvival {
    #[default]
    Unknown,
    NotGuaranteed,
    Guaranteed,
}

impl FailureSurvival {
    const fn intersection(self, other: Self) -> Self {
        match (self, other) {
            (Self::NotGuaranteed, _) | (_, Self::NotGuaranteed) => Self::NotGuaranteed,
            (Self::Guaranteed, Self::Guaranteed) => Self::Guaranteed,
            _ => Self::Unknown,
        }
    }
}

/// A provider's commit boundary, independent of transaction participation,
/// retention policy, processing completion, and external side effects.
/// A backend name or `is_volatile() == false` cannot supply this declaration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageDurability {
    pub process_restart: FailureSurvival,
    pub power_loss: FailureSurvival,
    pub storage_loss: FailureSurvival,
}

impl StorageDurability {
    pub const UNKNOWN: Self = Self {
        process_restart: FailureSurvival::Unknown,
        power_loss: FailureSurvival::Unknown,
        storage_loss: FailureSurvival::Unknown,
    };

    pub const VOLATILE: Self = Self {
        process_restart: FailureSurvival::NotGuaranteed,
        power_loss: FailureSurvival::NotGuaranteed,
        storage_loss: FailureSurvival::NotGuaranteed,
    };

    pub const LOCAL_PROCESS_RESTART: Self = Self {
        process_restart: FailureSurvival::Guaranteed,
        power_loss: FailureSurvival::NotGuaranteed,
        storage_loss: FailureSurvival::NotGuaranteed,
    };

    pub const LOCAL_POWER_LOSS: Self = Self {
        process_restart: FailureSurvival::Guaranteed,
        power_loss: FailureSurvival::Guaranteed,
        storage_loss: FailureSurvival::NotGuaranteed,
    };

    pub const fn survival(self, failure: FailureMode) -> FailureSurvival {
        match failure {
            FailureMode::ProcessRestart => self.process_restart,
            FailureMode::PowerLoss => self.power_loss,
            FailureMode::StorageLoss => self.storage_loss,
        }
    }

    /// Common guarantee when both storage boundaries are required.
    /// Failure modes are checked separately; replication does not imply sync.
    pub const fn intersection(self, other: Self) -> Self {
        Self {
            process_restart: self.process_restart.intersection(other.process_restart),
            power_loss: self.power_loss.intersection(other.power_loss),
            storage_loss: self.storage_loss.intersection(other.storage_loss),
        }
    }

    pub fn require(
        self,
        failure: FailureMode,
    ) -> std::result::Result<(), DurabilityRequirementError> {
        match self.survival(failure) {
            FailureSurvival::Guaranteed => Ok(()),
            actual => Err(DurabilityRequirementError {
                required: failure,
                actual,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("storage survival for {required:?} is {actual:?}, not guaranteed")]
pub struct DurabilityRequirementError {
    pub required: FailureMode,
    pub actual: FailureSurvival,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_failure_mode_requires_affirmative_evidence() {
        use FailureSurvival::*;
        for process_restart in [Unknown, NotGuaranteed, Guaranteed] {
            for power_loss in [Unknown, NotGuaranteed, Guaranteed] {
                for storage_loss in [Unknown, NotGuaranteed, Guaranteed] {
                    let declaration = StorageDurability {
                        process_restart,
                        power_loss,
                        storage_loss,
                    };
                    for (failure, survival) in [
                        (FailureMode::ProcessRestart, process_restart),
                        (FailureMode::PowerLoss, power_loss),
                        (FailureMode::StorageLoss, storage_loss),
                    ] {
                        assert_eq!(declaration.survival(failure), survival);
                        let expected = if survival == Guaranteed {
                            Ok(())
                        } else {
                            Err(DurabilityRequirementError {
                                required: failure,
                                actual: survival,
                            })
                        };
                        assert_eq!(declaration.require(failure), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn combining_stores_never_upgrades_unknown_or_unsupported_survival() {
        use FailureSurvival::*;
        for left in [Unknown, NotGuaranteed, Guaranteed] {
            for right in [Unknown, NotGuaranteed, Guaranteed] {
                let expected = if left == NotGuaranteed || right == NotGuaranteed {
                    NotGuaranteed
                } else if left == Guaranteed && right == Guaranteed {
                    Guaranteed
                } else {
                    Unknown
                };
                let left = StorageDurability {
                    process_restart: left,
                    power_loss: left,
                    storage_loss: left,
                };
                let right = StorageDurability {
                    process_restart: right,
                    power_loss: right,
                    storage_loss: right,
                };
                let result = left.intersection(right);
                assert_eq!(result, right.intersection(left));
                assert_eq!(result.process_restart, expected);
                assert_eq!(result.power_loss, expected);
                assert_eq!(result.storage_loss, expected);
            }
        }
    }

    #[test]
    fn built_in_profiles_do_not_claim_survival_of_local_storage_loss() {
        assert_eq!(StorageDurability::default(), StorageDurability::UNKNOWN);
        for profile in [
            StorageDurability::UNKNOWN,
            StorageDurability::VOLATILE,
            StorageDurability::LOCAL_PROCESS_RESTART,
            StorageDurability::LOCAL_POWER_LOSS,
        ] {
            assert!(profile.require(FailureMode::StorageLoss).is_err());
        }
        assert!(StorageDurability::LOCAL_PROCESS_RESTART
            .require(FailureMode::PowerLoss)
            .is_err());
        assert!(StorageDurability::LOCAL_POWER_LOSS
            .require(FailureMode::PowerLoss)
            .is_ok());
    }

    #[test]
    fn serialized_declarations_preserve_unknown_and_reject_incomplete_or_extra_fields() {
        let value = serde_json::to_value(StorageDurability::UNKNOWN).expect("serialize");
        assert_eq!(
            value,
            serde_json::json!({
                "processRestart": "unknown",
                "powerLoss": "unknown",
                "storageLoss": "unknown"
            })
        );
        assert_eq!(
            serde_json::from_value::<StorageDurability>(value.clone()).expect("round trip"),
            StorageDurability::UNKNOWN
        );
        assert!(serde_json::from_value::<StorageDurability>(serde_json::json!({})).is_err());
        let mut unsupported = value;
        unsupported["exactlyOnce"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StorageDurability>(unsupported).is_err());
    }
}
