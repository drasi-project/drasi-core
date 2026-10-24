// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{ActivationCredentialContext, ActivationCredentials};
use drasi_lib::{
    computation::v1::{ResourceHandle, ResourceRole},
    identity::{CredentialContext, Credentials, IdentityProvider},
};
use drasi_plugin_sdk::{
    computation::{create_identity_provider, PluginConfiguration, PluginResolution},
    IdentityProviderPluginDescriptor,
};
use std::{fmt, sync::Arc};

/// Snapshot of the already loaded ordinary identity-provider catalogue.
pub type NativeIdentityProviders =
    std::collections::BTreeMap<String, Arc<dyn IdentityProviderPluginDescriptor>>;

struct ConfiguredIdentity {
    descriptor: Arc<dyn IdentityProviderPluginDescriptor>,
    recipe: PluginConfiguration,
    resolution: PluginResolution,
}

/// Typed, optional `identity` dependency. Provider objects never cross the native
/// boundary. Configured providers and their secret references are resolved anew
/// for each activation; an injected provider always takes precedence.
pub struct NativeIdentityResource {
    injected: Option<Arc<dyn IdentityProvider>>,
    configured: Option<ConfiguredIdentity>,
}

impl NativeIdentityResource {
    pub fn new(provider: Arc<dyn IdentityProvider>) -> Self {
        Self {
            injected: Some(provider),
            configured: None,
        }
    }

    pub fn configured(
        descriptor: Arc<dyn IdentityProviderPluginDescriptor>,
        recipe: PluginConfiguration,
        resolution: PluginResolution,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            descriptor.kind() == recipe.kind
                && descriptor.config_version() == recipe.config_version,
            "identity provider kind/configuration version mismatch"
        );
        Ok(Self {
            injected: None,
            configured: Some(ConfiguredIdentity {
                descriptor,
                recipe,
                resolution,
            }),
        })
    }

    pub fn with_injected_provider(mut self, provider: Arc<dyn IdentityProvider>) -> Self {
        self.injected = Some(provider);
        self
    }

    pub fn resource(self: Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::Identity, self)
    }

    pub(crate) async fn resolve(
        &self,
        endpoint: &ActivationCredentialContext,
    ) -> anyhow::Result<ActivationCredentials> {
        endpoint.validate()?;
        let context = CredentialContext::new()
            .with_property("hostname", &endpoint.hostname)
            .with_property("port", endpoint.port.to_string());
        let credentials = match &self.injected {
            Some(provider) => provider.get_credentials(&context).await,
            None => {
                let configured = self
                    .configured
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("identity provider is not bound"))?;
                configured
                    .resolution
                    .run(async {
                        let provider = create_identity_provider(
                            configured.descriptor.clone(),
                            &configured.recipe,
                        )
                        .await?;
                        provider.get_credentials(&context).await
                    })
                    .await
            }
        }
        .map_err(|_| {
            anyhow::anyhow!("activation identity provider credential resolution failed")
        })?;
        let credentials = match credentials {
            Credentials::UsernamePassword { username, password } => {
                ActivationCredentials::UsernamePassword { username, password }
            }
            Credentials::Token { username, token } => {
                ActivationCredentials::Token { username, token }
            }
            Credentials::Certificate { .. } => {
                anyhow::bail!("certificate credentials are not supported for native activation")
            }
        };
        credentials.validate()?;
        Ok(credentials)
    }
}

impl fmt::Debug for NativeIdentityResource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeIdentityResource")
            .field("injected", &self.injected.is_some())
            .field("configured", &self.configured.is_some())
            .finish()
    }
}
