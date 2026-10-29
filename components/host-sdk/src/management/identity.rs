// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::{NativeIdentityProviders, NativeIdentityResource};
use drasi_plugin_sdk::computation::{PluginConfiguration, PluginResolution};

/// Canonical recipe:
/// `{"kind":"nativeIdentity","provider":{"kind":"...","config_version":"...",
/// "config":{...}}}`. Keep references in `config`; resolution happens on START.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeIdentityConfig {
    pub provider: PluginConfiguration,
}

impl std::fmt::Debug for NativeIdentityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeIdentityConfig")
            .field("provider", &"[REDACTED]")
            .finish()
    }
}

impl NativeIdentityConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::Identity && dependencies.is_empty(),
            "native identity requires an Identity resource without dependencies"
        );
        anyhow::ensure!(
            !self.provider.kind.is_empty()
                && !self.provider.config_version.is_empty()
                && self.provider.config.is_object(),
            "native identity requires a provider kind, configuration version and object"
        );
        Ok(())
    }

    pub fn resolve(
        &self,
        instance: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
        providers: &NativeIdentityProviders,
        secrets: Option<Arc<dyn SecretStoreProvider>>,
    ) -> Result<ResourceHandle> {
        self.validate(
            specification,
            &dependencies
                .iter()
                .map(|(id, resource)| (id.clone(), resource.role()))
                .collect(),
        )?;
        let descriptor = providers
            .get(&self.provider.kind)
            .ok_or_else(|| anyhow::anyhow!("configured native identity provider is not loaded"))?;
        let services = LegacyPluginServices::empty(instance).with_secret_store(secrets);
        Ok(Arc::new(NativeIdentityResource::configured(
            descriptor.clone(),
            self.provider.clone(),
            PluginResolution::from_services(&services),
        )?)
        .resource())
    }
}
