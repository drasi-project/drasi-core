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

use super::*;
use crate::{
    identity::IdentityProvider,
    secret_store::SecretStoreProvider,
    state_store::{StateStoreProvider, StateStoreResult},
    wal::{WalError, WalProvider, WriteAheadLogConfig},
};
use async_trait::async_trait;
use drasi_core::models::SourceChange;
use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

pub struct LegacyStateStoreResource(pub Arc<dyn StateStoreProvider>);
pub struct LegacyIdentityResource(pub Arc<dyn IdentityProvider>);
pub struct LegacyWalResource(pub Arc<dyn WalProvider>);
pub struct LegacySecretStoreResource(pub Arc<dyn SecretStoreProvider>);
pub struct LegacyBootstrapResource(pub Arc<dyn crate::bootstrap::BootstrapProvider>);

pub(crate) struct PluginObservations {
    id: String,
    updates: tokio::sync::Mutex<
        Option<tokio::sync::mpsc::Receiver<crate::component_graph::ComponentUpdate>>,
    >,
    status: tokio::sync::watch::Sender<(crate::ComponentStatus, Option<String>)>,
}
impl PluginObservations {
    pub(crate) fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            updates: tokio::sync::Mutex::new(None),
            status: tokio::sync::watch::channel((crate::ComponentStatus::Stopped, None)).0,
        }
    }
    pub(crate) async fn channel(&self) -> crate::component_graph::ComponentUpdateSender {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        *self.updates.lock().await = Some(receiver);
        sender
    }
    pub(crate) async fn reset(&self) {
        self.status
            .send_replace((crate::ComponentStatus::Stopped, None));
        if let Some(receiver) = self.updates.lock().await.as_mut() {
            while let Ok(update) = receiver.try_recv() {
                log::trace!("Discarding prior plugin operation observation: {update:?}");
            }
        }
    }
    pub(crate) async fn close(&self) {
        *self.updates.lock().await = None;
    }
    pub(crate) async fn read(&self) -> anyhow::Result<()> {
        let mut updates = self.updates.lock().await;
        if updates.is_none() {
            drop(updates);
            return std::future::pending().await;
        }
        match updates.as_mut().expect("status receiver").recv().await {
            Some(crate::component_graph::ComponentUpdate::Status {
                component_id,
                status,
                message,
            }) => {
                if component_id != self.id {
                    anyhow::bail!("plugin reported another component's status");
                }
                self.status.send_replace((status, message.clone()));
                if status == crate::ComponentStatus::Error {
                    anyhow::bail!(
                        "plugin {} failed: {}",
                        self.id,
                        message.as_deref().unwrap_or("unspecified failure")
                    );
                }
            }
            None => {
                *updates = None;
            }
        }
        Ok(())
    }
    pub(crate) async fn failure(&self) -> anyhow::Result<()> {
        let mut status = self.status.subscribe();
        let status = status
            .wait_for(|(status, _)| *status == crate::ComponentStatus::Error)
            .await?;
        anyhow::bail!(
            "plugin {} failed: {}",
            self.id,
            status.1.as_deref().unwrap_or("unspecified failure")
        );
    }
    pub(crate) async fn drive<T>(
        &self,
        future: impl std::future::Future<Output = anyhow::Result<T>>,
        cleanup: bool,
    ) -> anyhow::Result<T> {
        tokio::pin!(future);
        let mut observation = None;
        loop {
            tokio::select! {
                result = &mut future => return match (result, observation) {
                    (Ok(_), Some(error)) => Err(error),
                    (Err(error), Some(observation)) => Err(error.context(format!("plugin status also failed: {observation:#}"))),
                    (result, None) => result,
                },
                update = self.read() => if let Err(error) = update {
                    if !cleanup { return Err(error); }
                    observation = Some(error);
                }
            }
        }
    }
}

/// Actual instance services for compatibility plugins. State/WAL access is
/// namespace-scoped; the adapter never shuts down the shared instance provider.
#[derive(Clone)]
pub struct LegacyPluginServices {
    pub scope: Arc<str>,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub identity: Option<Arc<dyn IdentityProvider>>,
    pub wal: Option<Arc<dyn WalProvider>>,
    pub secrets: Option<Arc<dyn SecretStoreProvider>>,
}

fn encode(value: &str) -> String {
    value.bytes().map(|value| format!("{value:02x}")).collect()
}

impl LegacyPluginServices {
    pub fn empty(scope: impl Into<Arc<str>>) -> Self {
        Self {
            scope: scope.into(),
            state_store: None,
            identity: None,
            wal: None,
            secrets: None,
        }
    }
    pub fn scoped(
        instance: &str,
        graph: &str,
        state_store: Option<Arc<dyn StateStoreProvider>>,
        identity: Option<Arc<dyn IdentityProvider>>,
        wal: Option<Arc<dyn WalProvider>>,
    ) -> Result<Self> {
        super::data::validate_identifier("graph", graph)?;
        let prefix = format!("computation_{}_{}", encode(instance), encode(graph));
        Ok(Self {
            scope: Arc::from(format!("{instance}::computation::{graph}")),
            state_store: state_store.map(|inner| {
                Arc::new(ScopedStateStore {
                    inner,
                    prefix: prefix.clone(),
                }) as Arc<dyn StateStoreProvider>
            }),
            identity,
            wal: wal.map(|inner| Arc::new(ScopedWal { inner, prefix }) as Arc<dyn WalProvider>),
            secrets: None,
        })
    }
    pub fn with_secret_store(mut self, secrets: Option<Arc<dyn SecretStoreProvider>>) -> Self {
        self.secrets = secrets;
        self
    }
    pub fn dependencies(&self) -> BTreeMap<Arc<str>, Vec<ResourceId>> {
        [
            ("state", self.state_store.is_some(), "instance.state"),
            ("identity", self.identity.is_some(), "instance.identity"),
            ("wal", self.wal.is_some(), "instance.wal"),
            ("secrets", self.secrets.is_some(), "instance.secrets"),
        ]
        .into_iter()
        .filter(|(_, present, _)| *present)
        .map(|(name, _, id)| {
            (
                Arc::from(name),
                vec![ResourceId::try_new(id).expect("constant resource")],
            )
        })
        .collect()
    }
    pub(super) fn requirements() -> BTreeMap<Arc<str>, ResourceRequirement> {
        [
            (
                "state",
                ResourceRequirement::exactly_one::<LegacyStateStoreResource>(
                    ResourceRole::StateStore,
                ),
            ),
            (
                "identity",
                ResourceRequirement::exactly_one::<LegacyIdentityResource>(ResourceRole::Identity),
            ),
            (
                "wal",
                ResourceRequirement::exactly_one::<LegacyWalResource>(ResourceRole::Wal),
            ),
            (
                "secrets",
                ResourceRequirement::exactly_one::<LegacySecretStoreResource>(
                    ResourceRole::SecretStore,
                ),
            ),
        ]
        .into_iter()
        .map(|(name, requirement)| {
            (
                Arc::from(name),
                ResourceRequirement {
                    minimum: 0,
                    ..requirement
                },
            )
        })
        .collect()
    }
    pub(super) fn validate_bindings(
        &self,
        spec: &ComponentSpecification,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        validate_service(
            spec,
            resources,
            "state",
            self.state_store.as_ref(),
            |value: &LegacyStateStoreResource| &value.0,
        )?;
        validate_service(
            spec,
            resources,
            "identity",
            self.identity.as_ref(),
            |value: &LegacyIdentityResource| &value.0,
        )?;
        validate_service(
            spec,
            resources,
            "wal",
            self.wal.as_ref(),
            |value: &LegacyWalResource| &value.0,
        )?;
        validate_service(
            spec,
            resources,
            "secrets",
            self.secrets.as_ref(),
            |value: &LegacySecretStoreResource| &value.0,
        )
    }
    pub(super) fn same_services(&self, other: &Self) -> bool {
        fn same<T: ?Sized>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
        }
        self.scope == other.scope
            && same(&self.state_store, &other.state_store)
            && same(&self.identity, &other.identity)
            && same(&self.wal, &other.wal)
            && same(&self.secrets, &other.secrets)
    }
    pub fn declare(
        &self,
        mut builder: ComputationGraphBuilder,
    ) -> GraphResult<ComputationGraphBuilder> {
        let mut resources = Vec::new();
        if let Some(store) = &self.state_store {
            resources.push((
                "instance.state",
                ResourceRole::StateStore,
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(LegacyStateStoreResource(store.clone())),
                ),
            ));
        }
        if let Some(identity) = &self.identity {
            resources.push((
                "instance.identity",
                ResourceRole::Identity,
                ResourceHandle::new(
                    ResourceRole::Identity,
                    Arc::new(LegacyIdentityResource(identity.clone())),
                ),
            ));
        }
        if let Some(wal) = &self.wal {
            resources.push((
                "instance.wal",
                ResourceRole::Wal,
                ResourceHandle::new(ResourceRole::Wal, Arc::new(LegacyWalResource(wal.clone()))),
            ));
        }
        if let Some(secrets) = &self.secrets {
            resources.push((
                "instance.secrets",
                ResourceRole::SecretStore,
                ResourceHandle::new(
                    ResourceRole::SecretStore,
                    Arc::new(LegacySecretStoreResource(secrets.clone())),
                ),
            ));
        }
        for (id, role, handle) in resources {
            let id = ResourceId::try_new(id)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(id.as_str()),
                })?
                .provide_resource(id, handle)?;
        }
        Ok(builder)
    }
}

pub(super) fn validate_service<T: Any + Send + Sync, S: ?Sized>(
    spec: &ComponentSpecification,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
    name: &str,
    expected: Option<&Arc<S>>,
    inner: impl Fn(&T) -> &Arc<S>,
) -> anyhow::Result<()> {
    let ids = spec
        .dependencies
        .get(name)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    match (expected, ids) {
        (Some(expected), [id]) => {
            if let Some(handle) = resources.get(id) {
                let resource = handle.get::<T>()?;
                if !Arc::ptr_eq(expected, inner(resource.as_ref())) {
                    anyhow::bail!("plugin service dependency {name} differs from its host");
                }
            }
        }
        (None, []) => {}
        _ => anyhow::bail!("plugin service dependency {name} must match its host"),
    }
    Ok(())
}

struct ScopedStateStore {
    inner: Arc<dyn StateStoreProvider>,
    prefix: String,
}
impl ScopedStateStore {
    fn partition(&self, id: &str) -> String {
        format!("{}_{}", self.prefix, encode(id))
    }
}

#[async_trait]
impl StateStoreProvider for ScopedStateStore {
    async fn get(&self, id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(&self.partition(id), key).await
    }
    async fn set(&self, id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        self.inner.set(&self.partition(id), key, value).await
    }
    async fn delete(&self, id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(&self.partition(id), key).await
    }
    async fn contains_key(&self, id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(&self.partition(id), key).await
    }
    async fn get_many(
        &self,
        id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(&self.partition(id), keys).await
    }
    async fn set_many(&self, id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        self.inner.set_many(&self.partition(id), entries).await
    }
    async fn delete_many(&self, id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(&self.partition(id), keys).await
    }
    async fn clear_store(&self, id: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(&self.partition(id)).await
    }
    async fn list_keys(&self, id: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(&self.partition(id)).await
    }
    async fn store_exists(&self, id: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(&self.partition(id)).await
    }
    async fn key_count(&self, id: &str) -> StateStoreResult<usize> {
        self.inner.key_count(&self.partition(id)).await
    }
    async fn sync(&self) -> StateStoreResult<()> {
        self.inner.sync().await
    }
    fn is_durable(&self) -> bool {
        self.inner.is_durable()
    }
}

struct ScopedWal {
    inner: Arc<dyn WalProvider>,
    prefix: String,
}
impl ScopedWal {
    fn partition(&self, id: &str) -> String {
        format!("{}_{}", self.prefix, encode(id))
    }
}

#[async_trait]
impl WalProvider for ScopedWal {
    async fn register(
        &self,
        id: &str,
        config: WriteAheadLogConfig,
    ) -> std::result::Result<(), WalError> {
        self.inner.register(&self.partition(id), config).await
    }
    async fn append(&self, id: &str, event: &SourceChange) -> std::result::Result<u64, WalError> {
        self.inner.append(&self.partition(id), event).await
    }
    async fn read_from(
        &self,
        id: &str,
        sequence: u64,
    ) -> std::result::Result<Vec<(u64, SourceChange)>, WalError> {
        self.inner.read_from(&self.partition(id), sequence).await
    }
    async fn prune_up_to(&self, id: &str, sequence: u64) -> std::result::Result<u64, WalError> {
        self.inner.prune_up_to(&self.partition(id), sequence).await
    }
    async fn head_sequence(&self, id: &str) -> std::result::Result<u64, WalError> {
        self.inner.head_sequence(&self.partition(id)).await
    }
    async fn oldest_sequence(&self, id: &str) -> std::result::Result<Option<u64>, WalError> {
        self.inner.oldest_sequence(&self.partition(id)).await
    }
    async fn event_count(&self, id: &str) -> std::result::Result<u64, WalError> {
        self.inner.event_count(&self.partition(id)).await
    }
    async fn delete_wal(&self, id: &str) -> std::result::Result<(), WalError> {
        self.inner.delete_wal(&self.partition(id)).await
    }
}
