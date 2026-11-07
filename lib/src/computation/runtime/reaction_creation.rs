// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::config::ComponentReference;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Mutex as SyncMutex;

pub(crate) enum ReactionConstructor {
    Sync(
        Box<dyn FnOnce(ComponentId, Vec<ComponentId>) -> anyhow::Result<Box<dyn Reaction>> + Send>,
    ),
    Async(
        Box<
            dyn FnOnce(
                    ComponentId,
                    Vec<ComponentId>,
                ) -> BoxFuture<
                    'static,
                    anyhow::Result<(Box<dyn Reaction>, HashMap<String, String>)>,
                > + Send,
        >,
    ),
}

/// A graph-owned, uninstantiated ordinary declaration. Its constructor receives
/// UUID bindings, never the editable names used to select them.
pub(super) struct DeferredReaction {
    id: ComponentId,
    references: Vec<ComponentReference>,
    bound: Vec<OnceLock<ComponentId>>,
    constructor: SyncMutex<Option<ReactionConstructor>>,
    instance: OnceLock<Arc<dyn Reaction>>,
    auto_start: bool,
    state: super::construction::ConstructionState,
}

impl DeferredReaction {
    pub(super) fn configuration_available(&self) -> bool {
        self.state.outcome().is_some_and(|result| result.is_ok())
    }

    pub(super) fn new_committed(
        id: ComponentId,
        references: Vec<ComponentReference>,
        auto_start: bool,
        constructor: ReactionConstructor,
    ) -> Arc<Self> {
        let bound = references.iter().map(|_| OnceLock::new()).collect();
        Arc::new(Self {
            id,
            references,
            auto_start,
            bound,
            constructor: SyncMutex::new(Some(constructor)),
            instance: OnceLock::new(),
            state: super::construction::ConstructionState::new(),
        })
    }

    pub(super) fn references(&self) -> Vec<ComponentReference> {
        self.references
            .iter()
            .zip(&self.bound)
            .map(|(reference, bound)| {
                bound
                    .get()
                    .cloned()
                    .map(ComponentReference::by_id)
                    .unwrap_or_else(|| reference.clone())
            })
            .collect()
    }

    pub(super) fn metadata(&self) -> Option<&HashMap<String, String>> {
        self.state.metadata.get()
    }

    pub(super) fn bind_committed(&self, inputs: &[ComponentReference]) -> anyhow::Result<()> {
        anyhow::ensure!(
            inputs.len() == self.references.len(),
            "reaction input count changed"
        );
        for ((reference, bound), input) in self.references.iter().zip(&self.bound).zip(inputs) {
            if let Some(id) = input.as_id() {
                anyhow::ensure!(
                    reference.as_id().is_none_or(|original| original == id)
                        && bound.get().is_none_or(|original| original == id),
                    "committed reaction input would retarget an existing binding"
                );
            } else {
                anyhow::ensure!(
                    reference == input && bound.get().is_none(),
                    "committed reaction input would discard an existing binding"
                );
            }
        }
        for (bound, input) in self.bound.iter().zip(inputs) {
            if let Some(id) = input.as_id() {
                let _ = bound.set(id.clone());
            }
        }
        Ok(())
    }

    fn resolve(&self) -> anyhow::Result<Vec<ComponentId>> {
        self.references()
            .iter()
            .map(|reference| match reference {
                ComponentReference::Id { id } => Ok(id.clone()),
                ComponentReference::Name(name) => Err(anyhow::anyhow!(
                    "component name '{name}' has not been bound by its configuration owner"
                )),
            })
            .collect()
    }

    pub(super) fn waiting(
        &self,
        _desired: &GraphSnapshot,
        observed: &ObservedGraph,
    ) -> Vec<DependencyWait> {
        self.references()
            .into_iter()
            .filter_map(|reference| {
                let id = match reference {
                    ComponentReference::Id { id } => id,
                    ComponentReference::Name(name) => {
                        return Some(DependencyWait::ComponentName { name })
                    }
                };
                (!observed.components.get(&id).is_some_and(|node| {
                    node.realization == RealizationState::Created && node.failure.is_none()
                }))
                .then_some(DependencyWait::Component { id })
            })
            .collect()
    }

    pub(super) async fn construct(&self, control: &GraphControl) -> anyhow::Result<()> {
        let desired = control.desired_snapshot();
        let resolved = self.resolve()?;
        let unique: BTreeSet<_> = resolved.iter().collect();
        anyhow::ensure!(
            unique.len() == resolved.len() && !unique.contains(&self.id),
            "reaction inputs contain duplicate or self references"
        );
        for id in &resolved {
            anyhow::ensure!(
                desired
                    .component(id)
                    .is_some_and(super::Runtime::is_query_node),
                "reaction input component '{id}' is not a query"
            );
        }
        let current: BTreeSet<_> = desired
            .subscriptions
            .iter()
            .filter(|(_, consumer)| consumer == &self.id)
            .map(|(producer, _)| producer.clone())
            .collect();
        anyhow::ensure!(
            current == resolved.iter().cloned().collect(),
            "committed reaction inputs are not declared in the graph"
        );
        if let Some(result) = self.state.outcome() {
            return result;
        }
        let guard = self.state.guard();
        let result = async {
            let constructor = self
                .constructor
                .lock()
                .map_err(|_| anyhow::anyhow!("reaction constructor ownership poisoned"))?
                .take()
                .ok_or_else(|| {
                    anyhow::anyhow!("reaction constructor already failed; replace the declaration")
                })?;
            let (instance, metadata) = match constructor {
                ReactionConstructor::Sync(constructor) => (
                    constructor(self.id.clone(), resolved.clone())?,
                    HashMap::new(),
                ),
                ReactionConstructor::Async(constructor) => {
                    constructor(self.id.clone(), resolved.clone()).await?
                }
            };
            let _ = self.instance.set(instance.into());
            let _ = self.state.metadata.set(metadata);
            let instance = self.instance()?;
            anyhow::ensure!(
                instance.id() == self.id.as_str(),
                "reaction constructor substituted its assigned UUID"
            );
            anyhow::ensure!(
                instance.query_ids()
                    == resolved.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "reaction constructor substituted its UUID input bindings"
            );
            Ok(())
        }
        .await;
        guard.finish(result)
    }

    fn instance(&self) -> anyhow::Result<&Arc<dyn Reaction>> {
        self.instance
            .get()
            .ok_or_else(|| anyhow::anyhow!("reaction is awaiting construction"))
    }
}

#[async_trait]
impl Reaction for DeferredReaction {
    fn id(&self) -> &str {
        self.id.as_str()
    }
    fn type_name(&self) -> &str {
        self.instance
            .get()
            .map_or("deferred-reaction", |instance| instance.type_name())
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.instance
            .get()
            .map_or_else(HashMap::new, |instance| instance.properties())
    }
    fn query_ids(&self) -> Vec<String> {
        self.instance
            .get()
            .map_or_else(Vec::new, |instance| instance.query_ids())
    }
    fn auto_start(&self) -> bool {
        self.auto_start
    }
    async fn bind_runtime_context(&self, context: crate::ReactionRuntimeContext) {
        if let Some(instance) = self.instance.get() {
            instance.bind_runtime_context(context).await;
        }
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        self.instance()?.initialize().await
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        if let Some(instance) = self.instance.get() {
            instance.dispose().await?;
        }
        Ok(())
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.instance()?.pause_processing().await
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.instance()?.resume_processing().await
    }
    async fn apply_update(
        &self,
        context: crate::management::OrdinaryComponentUpdateContext,
    ) -> anyhow::Result<()> {
        self.instance()?.apply_update(context).await
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.instance()?.start().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(instance) = self.instance.get() {
            instance.stop().await?;
        }
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        match self.instance.get() {
            Some(instance) => instance.status().await,
            None => ComponentStatus::Added,
        }
    }
    async fn enqueue_query_result(
        &self,
        result: crate::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.instance()?.enqueue_query_result(result).await
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        if let Some(instance) = self.instance.get() {
            instance.delete_owned_resources().await?;
        }
        Ok(())
    }
    async fn set_identity_provider(&self, provider: Arc<dyn crate::identity::IdentityProvider>) {
        if let Some(instance) = self.instance.get() {
            instance.set_identity_provider(provider).await;
        }
    }
    fn is_durable(&self) -> bool {
        self.instance
            .get()
            .is_some_and(|instance| instance.is_durable())
    }
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.instance
            .get()
            .is_some_and(|instance| instance.needs_snapshot_on_fresh_start())
    }
    fn default_recovery_policy(&self) -> crate::ReactionRecoveryPolicy {
        self.instance
            .get()
            .map_or_else(Default::default, |instance| {
                instance.default_recovery_policy()
            })
    }
    async fn bootstrap(&self, context: crate::reactions::BootstrapContext) -> anyhow::Result<()> {
        self.instance()?.bootstrap(context).await
    }
}
