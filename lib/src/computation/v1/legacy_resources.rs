// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use async_trait::async_trait;

use super::{
    ComponentGeneration, ComponentId, GraphControl, LegacyBootstrapResource,
    LegacyIdentityResource, LegacySecretStoreResource, LegacyStateStoreResource, LegacyWalResource,
    PluginIdentity, ResourceHandle, ResourceId, ResourceOwnership, ResourceRole,
    ResourceSpecification,
};
use crate::context::{ComponentResource, ComponentResourceObserver, PluginOrigin};

/// Adapts host-local plugin reports to generation-fenced native observations.
/// Providers remain borrowed from the component or instance that manages them.
pub struct GraphResourceObserver {
    control: GraphControl,
    component: ComponentId,
    generation: ComponentGeneration,
}

impl GraphResourceObserver {
    pub fn new(
        control: GraphControl,
        component: ComponentId,
        generation: ComponentGeneration,
    ) -> Self {
        Self {
            control,
            component,
            generation,
        }
    }
}

#[async_trait]
impl ComponentResourceObserver for GraphResourceObserver {
    async fn observe_plugin(&self, origin: PluginOrigin) -> anyhow::Result<()> {
        self.control.observe_plugin(
            &self.component,
            self.generation,
            PluginIdentity {
                id: Arc::from(origin.id),
                version: Arc::from(origin.version),
            },
        )?;
        Ok(())
    }

    async fn observe(&self, resources: Vec<ComponentResource>) -> anyhow::Result<()> {
        let registry = self.control.registry_snapshot();
        let existing: Vec<_> = registry
            .desired
            .resources
            .keys()
            .filter_map(|id| registry.resource(id).ok())
            .collect();
        let mut bindings = Vec::with_capacity(resources.len());
        for resource in resources {
            let (name, role, handle) = match resource {
                ComponentResource::Bootstrap(provider) => (
                    "bootstrap",
                    ResourceRole::Bootstrap,
                    ResourceHandle::new(
                        ResourceRole::Bootstrap,
                        Arc::new(LegacyBootstrapResource(provider.clone())),
                    )
                    .with_shared_identity(provider),
                ),
                ComponentResource::Identity(provider) => (
                    "identity",
                    ResourceRole::Identity,
                    ResourceHandle::new(
                        ResourceRole::Identity,
                        Arc::new(LegacyIdentityResource(provider.clone())),
                    )
                    .with_shared_identity(provider),
                ),
                ComponentResource::StateStore(provider) => (
                    "state",
                    ResourceRole::StateStore,
                    ResourceHandle::new(
                        ResourceRole::StateStore,
                        Arc::new(LegacyStateStoreResource(provider.clone())),
                    )
                    .with_shared_identity(provider),
                ),
                ComponentResource::Wal(provider) => (
                    "wal",
                    ResourceRole::Wal,
                    ResourceHandle::new(
                        ResourceRole::Wal,
                        Arc::new(LegacyWalResource(provider.clone())),
                    )
                    .with_shared_identity(provider),
                ),
                ComponentResource::SecretStore(provider) => (
                    "secrets",
                    ResourceRole::SecretStore,
                    ResourceHandle::new(
                        ResourceRole::SecretStore,
                        Arc::new(LegacySecretStoreResource(provider.clone())),
                    )
                    .with_shared_identity(provider),
                ),
            };
            // Older callers may supply wrapper handles without a shared identity.
            // Reuse that binding itself rather than teach the kernel its ABI.
            let handle = existing
                .iter()
                .find(|existing| same_legacy_provider(existing, &handle))
                .cloned()
                .unwrap_or(handle);
            bindings.push((
                ResourceSpecification {
                    id: ResourceId::try_new(name)?,
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(format!("{}.{name}", self.component)),
                },
                handle,
            ));
        }
        self.control
            .observe_resources(&self.component, self.generation, bindings)?;
        Ok(())
    }
}

fn same_legacy_provider(left: &ResourceHandle, right: &ResourceHandle) -> bool {
    if left.role() != right.role() {
        return false;
    }
    macro_rules! same {
        ($wrapper:ty) => {
            match (left.get::<$wrapper>(), right.get::<$wrapper>()) {
                (Ok(left), Ok(right)) => Arc::ptr_eq(&left.0, &right.0),
                _ => false,
            }
        };
    }
    match left.role() {
        ResourceRole::Bootstrap => same!(LegacyBootstrapResource),
        ResourceRole::Identity => same!(LegacyIdentityResource),
        ResourceRole::StateStore => same!(LegacyStateStoreResource),
        ResourceRole::Wal => same!(LegacyWalResource),
        ResourceRole::SecretStore => same!(LegacySecretStoreResource),
        _ => false,
    }
}
