// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::BTreeMap;

use async_trait::async_trait;
use drasi_lib::computation::v1::*;

/// Metadata remains registerable on other platforms. Only the requested
/// unsupported hosting capability fails; unrelated Server graphs still work.
pub struct SupervisorFactory {
    descriptor: FactoryDescriptor,
}

impl Default for SupervisorFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    crate::SUPERVISOR_IMPLEMENTATION,
                    "1",
                )
                .expect("constant identity"),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        "manifest".into(),
                        ConfigurationField {
                            value_type: ConfigurationType::Object,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for SupervisorFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::bail!("owned local instance hosting is unsupported on this platform; Unix lifetime pipes and process groups are required")
    }

    async fn create(
        &self,
        _: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        Err(ComponentCreationError::terminal(anyhow::anyhow!(
            "owned local instance hosting is unsupported on this platform"
        )))
    }
}
