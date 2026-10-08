// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

/// Explicit construction policy for host-managed journals. Unlike the legacy
/// persistent constructor, omission must not silently restore enabled services.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum QosRecoveryOptions {
    #[default]
    Disabled,
    Admission(AdmissionOptions),
    Replay(ReplayOptions),
}

impl QosRecoveryOptions {
    pub fn validate(&self, definition: &QosChannelDefinition) -> Result<(), PipeError> {
        definition.validate()?;
        match self {
            Self::Disabled => Ok(()),
            Self::Admission(options) => options.validate_channel(definition),
            Self::Replay(options) => options.validate(definition),
        }
    }

    pub(super) fn validate_storage(
        &self,
        definition: &QosChannelDefinition,
        durability: drasi_core::interface::StorageDurability,
    ) -> Result<(), PipeError> {
        self.validate(definition)?;
        let failure_scope = match self {
            Self::Disabled => return Ok(()),
            Self::Admission(options) => options.failure_scope,
            Self::Replay(options) => options.failure_scope,
        };
        durability
            .require(failure_scope)
            .map_err(|error| PipeError::Backend(error.into()))
    }

    /// Validate the persisted mode before changing membership or trimming data.
    /// New services may establish a boundary only on an empty journal.
    pub(super) fn prepare(&self, metadata: &mut Metadata) -> Result<bool, PipeError> {
        match self {
            Self::Disabled => {
                if metadata.admission.is_some() || metadata.replay.is_some() {
                    return Err(backend(
                        "persisted QoS recovery cannot be implicitly disabled",
                    ));
                }
                Ok(false)
            }
            Self::Admission(options) => {
                if metadata.replay.is_some() {
                    return Err(backend(
                        "output replay cannot be replaced by client admission",
                    ));
                }
                if let Some(admission) = &metadata.admission {
                    return if admission.options() == options {
                        Ok(false)
                    } else {
                        Err(backend("admission options differ from persisted settings"))
                    };
                }
                if metadata.head != 0 {
                    return Err(backend(
                        "admission must be configured before the first channel event",
                    ));
                }
                metadata.admission = Some(admission::AdmissionState::new(
                    options.clone(),
                    &metadata.definition,
                )?);
                Ok(true)
            }
            Self::Replay(options) => {
                if metadata.admission.is_some() {
                    return Err(backend(
                        "client admission cannot be replaced by output replay",
                    ));
                }
                if let Some(replay) = &metadata.replay {
                    return if replay.options() == *options {
                        Ok(false)
                    } else {
                        Err(backend(
                            "output replay options differ from persisted settings",
                        ))
                    };
                }
                if metadata.head != 0 {
                    return Err(backend(
                        "output replay tracking must be configured before the first event",
                    ));
                }
                metadata.replay = Some(replay::ReplayState::new(options.clone()));
                Ok(true)
            }
        }
    }
}
