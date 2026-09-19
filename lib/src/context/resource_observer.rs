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

//! In-process reporting of providers already attached to a component.
//!
//! These contracts are always available so plugins can report their selected
//! providers without depending on the computation runtime. Legacy contexts do
//! not install an observer. Observers are host-local and never cross FFI.
//! Dynamic host proxies report providers supplied through their runtime context
//! and existing setters; providers constructed privately inside a plugin cannot
//! be inspected through the unchanged FFI ABI.
//! A host can also report an explicitly identified plugin and its build version.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::bootstrap::BootstrapProvider;
use crate::identity::IdentityProvider;
use crate::secret_store::SecretStoreProvider;
use crate::state_store::StateStoreProvider;
use crate::wal::WalProvider;

/// An explicitly identified plugin that created a component.
///
/// `id` is assigned by the host, and `version` comes from the plugin's metadata.
/// Neither value is inferred from the component type or configuration version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOrigin {
    pub id: String,
    pub version: String,
}

/// An existing provider instance used by a component.
///
/// Cloning a resource shares the same provider; it never constructs a replacement
/// or clones the provider's underlying state.
#[derive(Clone)]
pub enum ComponentResource {
    Bootstrap(Arc<dyn BootstrapProvider>),
    Identity(Arc<dyn IdentityProvider>),
    StateStore(Arc<dyn StateStoreProvider>),
    Wal(Arc<dyn WalProvider>),
    SecretStore(Arc<dyn SecretStoreProvider>),
}

impl std::fmt::Debug for ComponentResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Bootstrap(_) => "Bootstrap",
            Self::Identity(_) => "Identity",
            Self::StateStore(_) => "StateStore",
            Self::Wal(_) => "Wal",
            Self::SecretStore(_) => "SecretStore",
        };
        f.debug_tuple(kind).field(&"<provider>").finish()
    }
}

/// Receives the complete set of selected providers for one component generation.
///
/// Each successful observation replaces the previous inventory, including when
/// `resources` is empty. Reports contain real, preconstructed provider handles,
/// not configuration or metadata. Implementations own generation validation and
/// publication into their runtime; callers log failures and signal component
/// error status without changing the Source/Reaction initialization contracts.
#[async_trait]
pub trait ComponentResourceObserver: Send + Sync {
    async fn observe(&self, resources: Vec<ComponentResource>) -> Result<()>;

    /// Report the component's plugin when both its identity and version are known.
    ///
    /// The default implementation preserves compatibility with observers that
    /// only track provider resources. This callback is host-local, not an FFI hook.
    async fn observe_plugin(&self, _origin: PluginOrigin) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{mpsc, Mutex};

    use crate::bootstrap::{BootstrapContext, BootstrapRequest, BootstrapResult};
    use crate::channels::{BootstrapEventSender, ComponentStatus};
    use crate::component_graph::ComponentUpdate;
    use crate::context::{ReactionRuntimeContext, SourceRuntimeContext};
    use crate::identity::PasswordIdentityProvider;
    use crate::reactions::common::base::{ReactionBase, ReactionBaseParams};
    use crate::secret_store::MemorySecretStoreProvider;
    use crate::sources::{SourceBase, SourceBaseParams};
    use crate::state_store::MemoryStateStoreProvider;
    use crate::wal::{WalError, WriteAheadLogConfig};
    use drasi_core::models::SourceChange;

    mockall::mock! {
        Wal {}

        #[async_trait]
        impl WalProvider for Wal {
            async fn register(
                &self,
                source_id: &str,
                config: WriteAheadLogConfig,
            ) -> std::result::Result<(), WalError>;
            async fn append(
                &self,
                source_id: &str,
                event: &SourceChange,
            ) -> std::result::Result<u64, WalError>;
            async fn read_from(
                &self,
                source_id: &str,
                sequence: u64,
            ) -> std::result::Result<Vec<(u64, SourceChange)>, WalError>;
            async fn prune_up_to(
                &self,
                source_id: &str,
                sequence: u64,
            ) -> std::result::Result<u64, WalError>;
            async fn head_sequence(&self, source_id: &str) -> std::result::Result<u64, WalError>;
            async fn oldest_sequence(
                &self,
                source_id: &str,
            ) -> std::result::Result<Option<u64>, WalError>;
            async fn event_count(&self, source_id: &str) -> std::result::Result<u64, WalError>;
            async fn delete_wal(&self, source_id: &str) -> std::result::Result<(), WalError>;
        }
    }

    #[derive(Default)]
    struct RecordingObserver {
        reports: Mutex<Vec<Vec<ComponentResource>>>,
        fail: AtomicBool,
    }

    #[async_trait]
    impl ComponentResourceObserver for RecordingObserver {
        async fn observe(&self, resources: Vec<ComponentResource>) -> Result<()> {
            self.reports.lock().await.push(resources);
            anyhow::ensure!(!self.fail.load(Ordering::Acquire), "inventory rejected");
            Ok(())
        }
    }

    struct TestBootstrap(usize);

    #[async_trait]
    impl BootstrapProvider for TestBootstrap {
        async fn bootstrap(
            &self,
            _request: BootstrapRequest,
            _context: &BootstrapContext,
            _event_tx: BootstrapEventSender,
            _settings: Option<&crate::config::SourceSubscriptionSettings>,
        ) -> Result<BootstrapResult> {
            Ok(BootstrapResult {
                event_count: self.0,
                source_position: None,
            })
        }
    }

    fn identity(name: &str) -> Arc<dyn IdentityProvider> {
        Arc::new(PasswordIdentityProvider::new(name, "test-password"))
    }

    fn assert_identity(resource: &ComponentResource, expected: &Arc<dyn IdentityProvider>) {
        let ComponentResource::Identity(provider) = resource else {
            panic!("expected identity, got {resource:?}");
        };
        assert!(Arc::ptr_eq(provider, expected));
    }

    fn assert_state_store(resource: &ComponentResource, expected: &Arc<dyn StateStoreProvider>) {
        let ComponentResource::StateStore(provider) = resource else {
            panic!("expected state store, got {resource:?}");
        };
        assert!(Arc::ptr_eq(provider, expected));
    }

    #[tokio::test]
    async fn provider_only_observers_accept_plugin_origins_without_new_implementation() {
        let observer: Arc<dyn ComponentResourceObserver> = Arc::new(RecordingObserver::default());
        observer
            .observe_plugin(PluginOrigin {
                id: "explicit-plugin".into(),
                version: "1.2.3".into(),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn contexts_default_to_none_and_clone_the_same_observer() {
        let (update_tx, _rx) = mpsc::channel(8);
        let mut source =
            SourceRuntimeContext::new("instance", "source", None, update_tx.clone(), None);
        let mut reaction =
            ReactionRuntimeContext::new("instance", "reaction", None, update_tx, None);
        assert!(source.resource_observer.is_none());
        assert!(reaction.resource_observer.is_none());

        let observer: Arc<dyn ComponentResourceObserver> = Arc::new(RecordingObserver::default());
        source.resource_observer = Some(observer.clone());
        reaction.resource_observer = Some(observer.clone());
        assert!(Arc::ptr_eq(
            source.clone().resource_observer.as_ref().unwrap(),
            &observer
        ));
        assert!(Arc::ptr_eq(
            reaction.clone().resource_observer.as_ref().unwrap(),
            &observer
        ));
        assert!(format!("{source:?}").contains("<ComponentResourceObserver>"));
        assert!(format!("{reaction:?}").contains("<ComponentResourceObserver>"));
    }

    #[tokio::test]
    async fn source_reports_selected_handles_and_live_provider_replacements() {
        let selected_store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let default_store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let selected_identity = identity("selected");
        let base = SourceBase::new(
            SourceBaseParams::new("source")
                .with_state_store(selected_store.clone())
                .with_bootstrap_provider(TestBootstrap(1)),
        )
        .unwrap();
        base.set_identity_provider(selected_identity.clone()).await;

        let wal: Arc<dyn WalProvider> = Arc::new(MockWal::new());
        let observer = Arc::new(RecordingObserver::default());
        let (update_tx, mut rx) = mpsc::channel(8);
        let mut context = SourceRuntimeContext::new(
            "instance",
            "source",
            Some(default_store),
            update_tx,
            Some(identity("default")),
        );
        context.wal_provider = Some(wal.clone());
        context.resource_observer = Some(observer.clone());
        base.initialize(context).await;

        let replacement_identity = identity("replacement");
        base.clone_shared()
            .set_identity_provider(replacement_identity.clone())
            .await;
        base.set_bootstrap_provider(TestBootstrap(2)).await;

        let reports = observer.reports.lock().await;
        assert_eq!(reports.len(), 3);
        for (index, report) in reports.iter().enumerate() {
            assert_eq!(report.len(), 4);
            assert_identity(
                &report[1],
                if index == 0 {
                    &selected_identity
                } else {
                    &replacement_identity
                },
            );
            assert_state_store(&report[2], &selected_store);
            let ComponentResource::Wal(provider) = &report[3] else {
                panic!("expected WAL");
            };
            assert!(Arc::ptr_eq(provider, &wal));
        }
        let (
            ComponentResource::Bootstrap(initial),
            ComponentResource::Bootstrap(unchanged),
            ComponentResource::Bootstrap(replaced),
        ) = (&reports[0][0], &reports[1][0], &reports[2][0])
        else {
            panic!("expected bootstrap providers");
        };
        assert!(Arc::ptr_eq(initial, unchanged));
        assert!(!Arc::ptr_eq(initial, replaced));
        for (provider, count) in [(initial, 1), (replaced, 2)] {
            let (tx, _rx) = mpsc::channel(1);
            let result = provider
                .bootstrap(
                    BootstrapRequest {
                        query_id: "query".into(),
                        node_labels: vec![],
                        relation_labels: vec![],
                        request_id: "request".into(),
                    },
                    &BootstrapContext::new_minimal("instance".into(), "source".into()),
                    tx,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.event_count, count);
        }
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reaction_reports_selected_identity_and_shared_state_store() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let selected_identity = identity("selected");
        let base = ReactionBase::new(ReactionBaseParams::new("reaction", vec![]));
        base.set_identity_provider(selected_identity.clone()).await;
        let observer = Arc::new(RecordingObserver::default());
        let (update_tx, _rx) = mpsc::channel(8);
        let mut context = ReactionRuntimeContext::new(
            "instance",
            "reaction",
            Some(store.clone()),
            update_tx,
            Some(identity("default")),
        );
        context.resource_observer = Some(observer.clone());
        base.initialize(context).await;
        let replacement_identity = identity("replacement");
        base.clone_shared()
            .set_identity_provider(replacement_identity.clone())
            .await;

        let reports = observer.reports.lock().await;
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].len(), 2);
        assert_eq!(reports[1].len(), 2);
        assert_identity(&reports[0][0], &selected_identity);
        assert_identity(&reports[1][0], &replacement_identity);
        assert_state_store(&reports[0][1], &store);
        assert_state_store(&reports[1][1], &store);
    }

    #[tokio::test]
    async fn empty_inventories_are_reported_and_legacy_contexts_are_noops() {
        for observed in [false, true] {
            let observer = Arc::new(RecordingObserver::default());
            let (update_tx, mut rx) = mpsc::channel(8);
            let mut source_context =
                SourceRuntimeContext::new("instance", "source", None, update_tx.clone(), None);
            let mut reaction_context =
                ReactionRuntimeContext::new("instance", "reaction", None, update_tx, None);
            if observed {
                source_context.resource_observer = Some(observer.clone());
                reaction_context.resource_observer = Some(observer.clone());
            }
            let source = SourceBase::new(SourceBaseParams::new("source")).unwrap();
            let reaction = ReactionBase::new(ReactionBaseParams::new("reaction", vec![]));
            source.initialize(source_context).await;
            reaction.initialize(reaction_context).await;
            let reports = observer.reports.lock().await;
            assert_eq!(reports.len(), if observed { 2 } else { 0 });
            assert!(reports.iter().all(Vec::is_empty));
            assert_eq!(source.get_status().await, ComponentStatus::Stopped);
            assert_eq!(reaction.get_status().await, ComponentStatus::Stopped);
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn observer_failures_signal_errors_on_initialize_and_live_setters() {
        for fail_initialize in [true, false] {
            let observer = Arc::new(RecordingObserver::default());
            observer.fail.store(fail_initialize, Ordering::Release);
            let (update_tx, mut rx) = mpsc::channel(8);
            let mut source_context =
                SourceRuntimeContext::new("instance", "source", None, update_tx.clone(), None);
            let mut reaction_context =
                ReactionRuntimeContext::new("instance", "reaction", None, update_tx, None);
            source_context.resource_observer = Some(observer.clone());
            reaction_context.resource_observer = Some(observer.clone());
            let source = SourceBase::new(SourceBaseParams::new("source")).unwrap();
            let reaction = ReactionBase::new(ReactionBaseParams::new("reaction", vec![]));
            source.initialize(source_context).await;
            reaction.initialize(reaction_context).await;
            if !fail_initialize {
                observer.fail.store(true, Ordering::Release);
                source.set_bootstrap_provider(TestBootstrap(1)).await;
                reaction
                    .set_identity_provider(identity("replacement"))
                    .await;
            }
            assert_eq!(source.get_status().await, ComponentStatus::Error);
            assert_eq!(reaction.get_status().await, ComponentStatus::Error);
            for expected_id in ["source", "reaction"] {
                let ComponentUpdate::Status {
                    component_id,
                    status,
                    message,
                } = rx.try_recv().unwrap();
                assert_eq!(component_id, expected_id);
                assert_eq!(status, ComponentStatus::Error);
                assert!(message.unwrap().contains("inventory rejected"));
            }
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn resource_clones_share_providers_without_exposing_credentials() {
        let provider = identity("private-username");
        let resource = ComponentResource::Identity(provider.clone());
        assert_identity(&resource.clone(), &provider);
        assert_eq!(format!("{resource:?}"), "Identity(\"<provider>\")");

        let store: Arc<dyn SecretStoreProvider> =
            Arc::new(MemorySecretStoreProvider::new().with_secret("key", "private-value"));
        let resource = ComponentResource::SecretStore(store.clone());
        let ComponentResource::SecretStore(cloned) = resource.clone() else {
            panic!("expected secret store");
        };
        assert!(Arc::ptr_eq(&cloned, &store));
        assert_eq!(cloned.get_secret("key").await.unwrap(), "private-value");
        assert_eq!(format!("{resource:?}"), "SecretStore(\"<provider>\")");
    }
}
