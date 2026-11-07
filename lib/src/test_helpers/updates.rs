// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    channels::{ComponentStatus, SubscriptionResponse},
    computation::v1::{
        ComponentHandle, ComponentId, ComponentName, ComponentUpdateData, ComponentUpdateDirective,
        ComponentUpdateMode, ComponentUpdatePlan,
    },
    management::{
        OrdinaryComponent, OrdinaryComponentDefinition, OrdinaryComponentRecord,
        OrdinaryComponentResolver,
    },
    DrasiLib, Reaction, Source,
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
pub(crate) struct Trace {
    calls: Mutex<Vec<&'static str>>,
    fail_delete: AtomicBool,
}

impl Trace {
    fn push(&self, event: &'static str) {
        self.calls.lock().unwrap().push(event);
    }
    pub(crate) fn events(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

struct Member {
    id: String,
    auto_start: bool,
    version: u64,
    inputs: Vec<String>,
    status: crate::component_graph::ComponentStatusHandle,
    bound: AtomicBool,
    trace: Arc<Trace>,
}

impl Member {
    fn new(
        record: &OrdinaryComponentRecord,
        inputs: &[ComponentId],
        trace: Arc<Trace>,
    ) -> anyhow::Result<Self> {
        let configuration = match &record.component {
            OrdinaryComponent::Source { configuration, .. }
            | OrdinaryComponent::Reaction { configuration, .. } => configuration,
            _ => anyhow::bail!("fixture requires a registered source or reaction recipe"),
        };
        let version = configuration["version"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("fixture version must be an integer"))?;
        trace.push("create");
        Ok(Self {
            id: record.id.to_string(),
            auto_start: record.auto_start,
            version,
            inputs: inputs.iter().map(ToString::to_string).collect(),
            status: crate::component_graph::ComponentStatusHandle::new(record.id.as_str()),
            bound: AtomicBool::new(false),
            trace,
        })
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::from([("version".into(), self.version.into())])
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.bound.load(Ordering::Acquire),
            "context binding must precede initialization"
        );
        self.trace.push("initialize");
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.trace.push("start");
        self.status.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.trace.push("stop");
        self.status.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
}

#[async_trait]
impl Source for Member {
    fn id(&self) -> &str {
        &self.id
    }
    fn type_name(&self) -> &str {
        "fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.properties()
    }
    fn auto_start(&self) -> bool {
        self.auto_start
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    async fn bind_runtime_context(&self, context: crate::SourceRuntimeContext) {
        self.status.wire(context.update_tx).await;
        self.bound.store(true, Ordering::Release);
        self.trace.push("bind");
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        self.initialize().await
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.start().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.stop().await
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        self.trace.push("dispose");
        Ok(())
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        self.trace.push("delete");
        anyhow::ensure!(
            !self.trace.fail_delete.load(Ordering::Acquire),
            "owned-resource deletion failed"
        );
        Ok(())
    }
    async fn subscribe(
        &self,
        _: crate::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        anyhow::bail!("lifecycle fixture has no data subscription")
    }
}

#[async_trait]
impl Reaction for Member {
    fn id(&self) -> &str {
        &self.id
    }
    fn type_name(&self) -> &str {
        "fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.properties()
    }
    fn query_ids(&self) -> Vec<String> {
        self.inputs.clone()
    }
    fn auto_start(&self) -> bool {
        self.auto_start
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    async fn bind_runtime_context(&self, context: crate::ReactionRuntimeContext) {
        assert!(context.snapshot_fetcher.is_some());
        self.status.wire(context.update_tx).await;
        self.bound.store(true, Ordering::Release);
        self.trace.push("bind");
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        self.initialize().await
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.start().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.stop().await
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        self.trace.push("dispose");
        Ok(())
    }
}

struct Resolver(Arc<Trace>);

#[async_trait]
impl OrdinaryComponentResolver for Resolver {
    fn plan_update(
        &self,
        _: &str,
        current: &OrdinaryComponentRecord,
        proposed: &OrdinaryComponentRecord,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(current.id == proposed.id, "immutable UUID changed");
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }
    async fn create_source(
        &self,
        _: &str,
        record: &OrdinaryComponentRecord,
        _: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Source>, HashMap<String, String>)> {
        Ok((
            Box::new(Member::new(record, &[], self.0.clone())?),
            HashMap::new(),
        ))
    }
    async fn create_reaction(
        &self,
        _: &str,
        record: &OrdinaryComponentRecord,
        inputs: &[ComponentId],
        _: Option<&ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Reaction>, HashMap<String, String>)> {
        Ok((
            Box::new(Member::new(record, inputs, self.0.clone())?),
            HashMap::new(),
        ))
    }
}

pub(crate) async fn core() -> (Arc<DrasiLib>, Arc<Trace>) {
    let trace = Arc::new(Trace::default());
    let core = DrasiLib::builder()
        .with_id("test-instance")
        .with_startup_timeout_secs(5)
        .with_shutdown_timeout_secs(5)
        .with_construction_timeout_secs(2)
        .with_activation_timeout_secs(2)
        .with_cleanup_timeout_secs(2)
        .with_ordinary_component_resolver(Arc::new(Resolver(trace.clone())))
        .build()
        .await
        .unwrap();
    assert!(!core.allow_external_component_ids());
    (Arc::new(core), trace)
}

pub(crate) fn definition(
    id: ComponentId,
    label: &str,
    reaction: bool,
    version: u64,
) -> OrdinaryComponentRecord {
    OrdinaryComponentRecord {
        id,
        name: ComponentName::try_new(label).unwrap(),
        auto_start: false,
        component: if reaction {
            OrdinaryComponent::Reaction {
                kind: "fixture".into(),
                configuration: serde_json::json!({"version": version}),
                inputs: vec![],
            }
        } else {
            OrdinaryComponent::Source {
                kind: "fixture".into(),
                configuration: serde_json::json!({"version": version}),
            }
        },
    }
}

pub(crate) async fn add(core: &DrasiLib, label: &str, reaction: bool) -> ComponentHandle {
    let template = definition(ComponentId::generate(), label, reaction, 0);
    core.create_managed_ordinary_component(
        0,
        "initial",
        OrdinaryComponentDefinition {
            id: None,
            name: template.name,
            auto_start: false,
            component: template.component,
        },
    )
    .await
    .unwrap();
    settle(core).await;
    let handle = core.component_by_name(label).unwrap();
    handle.wait_created().await.unwrap();
    handle
}

pub(crate) async fn settle(core: &DrasiLib) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = core.reconcile_desired_state().await.unwrap();
            assert!(status.error.is_none(), "{status:?}");
            if core
                .desired_configuration()
                .unwrap()
                .pending_updates
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_resource_deletion_precedes_disposal() {
        let (core, trace) = core().await;
        let source = add(&core, "owned", false).await;
        source.start().await.unwrap();
        source.wait_started().await.unwrap();
        core.remove_source(source.id().as_str(), true)
            .await
            .unwrap();
        assert_eq!(
            trace.events(),
            ["create", "bind", "initialize", "start", "stop", "delete", "dispose"]
        );
        assert!(source.observed().is_err());
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_resource_deletion_retains_the_cleanup_owner_for_retry() {
        let (core, trace) = core().await;
        let source = add(&core, "owned", false).await;
        source.start().await.unwrap();
        source.wait_started().await.unwrap();
        trace.fail_delete.store(true, Ordering::Release);
        assert!(core
            .remove_source(source.id().as_str(), true)
            .await
            .is_err());
        assert!(source.observed().is_ok());
        assert!(!trace.events().contains(&"dispose"));
        assert!(source.start().await.is_err());
        trace.fail_delete.store(false, Ordering::Release);
        settle(&core).await;
        assert!(source.observed().is_err());
        assert_eq!(
            trace.events(),
            ["create", "bind", "initialize", "start", "stop", "delete", "delete", "dispose"]
        );
        core.shutdown().await.unwrap();
    }
}
