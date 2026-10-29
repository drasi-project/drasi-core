// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::NativePlugin;
use drasi_computation_contracts::{
    ConsumerProgressUpdates, ConsumerProgressView, SourceProgressUpdates, SourceProgressView,
};
use drasi_computation_plugin_sdk::{self as sdk, Factory};
use drasi_lib::computation::v1::*;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio_stream::StreamExt;

#[path = "bootstrap_coordination_foreign_tests.rs"]
mod foreign;
#[path = "bootstrap_coordination_release_tests.rs"]
mod release;

#[derive(Default)]
struct State {
    progress: Mutex<Option<sdk::SourceBootstrapProgress>>,
    live: Mutex<Option<sdk::SourceBootstrapLiveProgress>>,
    stop_started: AtomicUsize,
    block_stop: AtomicBool,
    fail_stop: AtomicBool,
    stop_release: tokio::sync::Notify,
    provider_stop: Arc<ProviderStop>,
    open_calls: AtomicUsize,
    fail_open: AtomicBool,
    foreign_open_reader: AtomicBool,
    invalid_open_scope: AtomicBool,
    source_lifecycle: Arc<SourceLifecycle>,
}

#[derive(Default)]
struct SourceLifecycle {
    fail_start: AtomicBool,
    fail_stop: AtomicBool,
    block_start: AtomicBool,
    block_stop: AtomicBool,
    release_start: tokio::sync::Notify,
    release_stop: tokio::sync::Notify,
}

#[derive(Default)]
struct ProviderStop {
    started: AtomicUsize,
    blocked: AtomicBool,
    fail: AtomicBool,
    release: tokio::sync::Notify,
}

struct SourceFactory {
    shared: bool,
    state: Arc<State>,
}

fn metadata() -> sdk::FactoryMetadata {
    sdk::FactoryMetadata {
        implementation: ImplementationIdentity::try_new("test/coordinated", "1").unwrap(),
        configuration_version: 1,
        configuration: sdk::ConfigSchema {
            fields: BTreeMap::from([(
                "token".into(),
                sdk::ConfigField {
                    value_type: sdk::ConfigType::String,
                    required: false,
                    secret: true,
                },
            )]),
            allow_additional: true,
        },
        role: ComponentRole::Source,
        completion: None,
        capabilities: sdk::Capabilities::default(),
        ports: vec![PortDescriptor::new(
            PortId::try_new("out").unwrap(),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    }
}

impl sdk::Factory for SourceFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        metadata()
    }
    fn supports_source_progress(&self) -> bool {
        !self.shared
    }
    fn supports_consumer_progress(&self) -> bool {
        self.shared
    }
    fn supports_bootstrap_coordination(&self) -> bool {
        true
    }
    fn create(
        &self,
        _: &CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::bail!("fixture requires a coordinator")
    }
    fn create_bootstrap_coordinator(
        &self,
        _: &CreateRequest,
        progress: sdk::SourceBootstrapProgress,
    ) -> anyhow::Result<Arc<dyn sdk::NativeSourceBootstrapCoordinator>> {
        *self.state.progress.lock().unwrap() = Some(progress);
        Ok(self.state.clone())
    }
}

struct LiveSource {
    descriptor: ComponentDescriptor,
    live: sdk::SourceBootstrapLiveProgress,
    lifecycle: Arc<SourceLifecycle>,
}

#[async_trait]
impl sdk::NativeComponent for LiveSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        match &self.live {
            sdk::SourceBootstrapLiveProgress::Shared(view) => Some(view.clone()),
            _ => None,
        }
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.lifecycle.block_start.load(Ordering::SeqCst) {
            self.lifecycle.release_start.notified().await;
        }
        anyhow::ensure!(
            !self.lifecycle.fail_start.load(Ordering::SeqCst),
            "source start failed"
        );
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if self.lifecycle.block_stop.load(Ordering::SeqCst) {
            self.lifecycle.release_stop.notified().await;
        }
        anyhow::ensure!(
            !self.lifecycle.fail_stop.load(Ordering::SeqCst),
            "source cleanup failed"
        );
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeSource for LiveSource {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        match &self.live {
            sdk::SourceBootstrapLiveProgress::Single(view) => Some(view.clone()),
            _ => None,
        }
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}

#[async_trait]
impl sdk::NativeSourceBootstrapCoordinator for State {
    fn create_source(
        &self,
        request: &CreateRequest,
        _: sdk::ControlSender,
        live: sdk::SourceBootstrapLiveProgress,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        *self.live.lock().unwrap() = Some(match &live {
            sdk::SourceBootstrapLiveProgress::Single(view) => {
                sdk::SourceBootstrapLiveProgress::Single(view.clone())
            }
            sdk::SourceBootstrapLiveProgress::Shared(view) => {
                sdk::SourceBootstrapLiveProgress::Shared(view.clone())
            }
        });
        Ok(sdk::Component::Source(Box::new(LiveSource {
            descriptor: ComponentDescriptor::try_new(request.id.clone(), metadata().ports)?,
            live,
            lifecycle: self.source_lifecycle.clone(),
        }))
        .into())
    }
    fn open(
        &self,
        query: &ComponentId,
        mut progress: SourceProgressView,
    ) -> anyhow::Result<Arc<dyn sdk::NativeBootstrapProvider>> {
        self.open_calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail_open.swap(false, Ordering::SeqCst), "open failed");
        if self.foreign_open_reader.swap(false, Ordering::SeqCst) {
            progress = SourceProgressView::new(owner(query.as_str()));
        }
        Ok(Arc::new(Provider(
            progress,
            self.provider_stop.clone(),
            self.invalid_open_scope.swap(false, Ordering::SeqCst),
        )))
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_started.fetch_add(1, Ordering::SeqCst);
        if self.block_stop.load(Ordering::SeqCst) {
            self.stop_release.notified().await;
        }
        anyhow::ensure!(
            !self.fail_stop.swap(false, Ordering::SeqCst),
            "cleanup failed"
        );
        Ok(())
    }
}

struct Provider(SourceProgressView, Arc<ProviderStop>, bool);

#[async_trait]
impl sdk::NativeBootstrapProvider for Provider {
    fn validate_query_scope(&self, _: &str, _: &str, _: &ComponentId) -> anyhow::Result<()> {
        anyhow::ensure!(!self.2, "provider scope mismatch");
        Ok(())
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        Some(self.0.clone())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.1.started.fetch_add(1, Ordering::SeqCst);
        if self.1.blocked.load(Ordering::SeqCst) {
            self.1.release.notified().await;
        }
        anyhow::ensure!(
            !self.1.fail.swap(false, Ordering::SeqCst),
            "provider cleanup failed"
        );
        Ok(())
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.0.snapshot()?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(tokio_stream::empty()),
            watermarks: Vec::new(),
        })
    }
}

struct PairedFactory;
impl sdk::BootstrapFactory for PairedFactory {
    fn metadata(&self) -> sdk::BootstrapFactoryMetadata {
        sdk::BootstrapFactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/paired", "1").unwrap(),
            configuration_version: 1,
            configuration: sdk::ConfigSchema::default(),
            source_progress: true,
        }
    }
    fn requires_source_bootstrap(&self) -> bool {
        true
    }
    fn create(
        &self,
        _: &CreateRequest,
        _: Option<sdk::NativeSourceProgress>,
    ) -> anyhow::Result<Arc<dyn sdk::NativeBootstrapProvider>> {
        anyhow::bail!("unpaired fixture")
    }
    fn create_with_source_bootstrap(
        &self,
        _: &CreateRequest,
        source: sdk::NativeSourceBootstrap,
    ) -> anyhow::Result<sdk::LinkedBootstrapProvider> {
        anyhow::ensure!(
            source.metadata().source.id.as_str() == "source",
            "wrong source"
        );
        source.open()
    }
}

fn source_definition(shared: bool, state: Arc<State>) -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "test/source-library",
        "1",
        vec![Arc::new(SourceFactory { shared, state })],
        vec![GraphChangeCodec::schema()],
    )
}

fn paired_definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new_with_bootstrap(
        "test/paired-library",
        "1",
        Vec::new(),
        vec![Arc::new(PairedFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

fn scope(generation: u64) -> Scope {
    Scope {
        instance_id: "instance".into(),
        graph_id: "graph".into(),
        generation,
    }
}

fn owner(id: &str) -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new("graph", ComponentId::try_new(id).unwrap()).unwrap())
}

fn progress(
    shared: bool,
    single: &Arc<QuerySourceProgress>,
    group: &Arc<ConsumerProgressSet>,
) -> NativeSourceBootstrapProgress {
    if shared {
        NativeSourceBootstrapProgress::Shared(group.clone())
    } else {
        NativeSourceBootstrapProgress::Single(single.clone())
    }
}

fn readers_active(state: &State, active: bool) {
    match state.progress.lock().unwrap().as_ref().unwrap() {
        sdk::SourceBootstrapProgress::Single(reader) => {
            assert_eq!(reader.snapshot().is_ok(), active)
        }
        sdk::SourceBootstrapProgress::Shared { consumers, members } => {
            assert_eq!(consumers.snapshot().is_ok(), active);
            assert_eq!(members.len(), 2);
            for reader in members.values() {
                assert_eq!(reader.snapshot().is_ok(), active);
            }
        }
    }
}

async fn coordination_matrix() -> anyhow::Result<()> {
    for shared in [false, true] {
        let state = Arc::new(State::default());
        let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
        let paired = NativePlugin::from_definition(paired_definition()?)?;
        let factory = &source.factories()[0];
        let query = owner("query");
        let other = owner("other");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), other.clone()],
        )?);
        assert!(
            factory
                .create_source_bootstrap(
                    ComponentId::try_new("source")?,
                    json!({}),
                    scope(0),
                    progress(!shared, &query, &group),
                )
                .is_err(),
            "a source cannot silently change its negotiated progress mode"
        );
        let coordinator = factory.create_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(0),
            progress(shared, &query, &group),
        )?;
        readers_active(&state, true);
        let binding = coordinator.bind_query(query.component_id(), &scope(0), &query)?;
        let table = binding.table();
        unsafe {
            table.retain.unwrap()(table.context);
            assert!(take_reply(table.describe.unwrap()(table.context)).is_ok());
        }
        drop(binding);
        unsafe {
            assert!(take_reply(table.describe.unwrap()(table.context)).is_err());
            let mut raw = abi::bootstrap::BootstrapHandle::null();
            assert!(take_status(table.open.unwrap()(table.context, &mut raw)).is_err());
            table.release.unwrap()(table.context);
        }
        let foreign_plugin =
            NativePlugin::from_definition(source_definition(shared, Arc::new(State::default()))?)?;
        let foreign_error = foreign_plugin.factories()[0]
            .create_component_with_source_bootstrap(
                ComponentId::try_new("source")?,
                json!({}),
                scope(0),
                coordinator.clone(),
                progress(shared, &query, &group),
            )
            .err()
            .expect(
                "same factory metadata from a different PluginState cannot adopt a coordinator",
            );
        assert!(foreign_error
            .to_string()
            .contains("another actual native factory"));

        let foreign = owner("query");
        let foreign_group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), other.clone()],
        )?);
        assert!(
            factory
                .create_component_with_source_bootstrap(
                    ComponentId::try_new("source")?,
                    json!({}),
                    scope(5),
                    coordinator.clone(),
                    progress(shared, &foreign, &foreign_group),
                )
                .is_err(),
            "same-named owners and equal memberships are not ownership"
        );
        for (id, config, scope) in [
            ("wrong-source", json!({}), scope(5)),
            ("source", json!({"different": true}), scope(5)),
            (
                "source",
                json!({}),
                Scope {
                    instance_id: "foreign".into(),
                    ..scope(5)
                },
            ),
            (
                "source",
                json!({}),
                Scope {
                    graph_id: "foreign".into(),
                    ..scope(5)
                },
            ),
        ] {
            assert!(factory
                .create_component_with_source_bootstrap(
                    ComponentId::try_new(id)?,
                    config,
                    scope,
                    coordinator.clone(),
                    progress(shared, &query, &group),
                )
                .is_err());
        }
        let paired_factory = &paired.bootstrap_factories()[0];
        assert!(paired_factory
            .create_provider(
                query.component_id().clone(),
                json!({}),
                Some(scope(0)),
                Some(query.clone()),
            )
            .is_err());
        assert!(paired_factory
            .create_paired_query_provider(
                query.component_id().clone(),
                json!({}),
                scope(0),
                foreign,
                coordinator.clone(),
            )
            .is_err());
        assert!(paired_factory
            .create_paired_query_provider(
                other.component_id().clone(),
                json!({}),
                scope(0),
                query.clone(),
                coordinator.clone(),
            )
            .is_err());
        let provider = paired_factory.create_paired_query_provider(
            query.component_id().clone(),
            json!({}),
            scope(0),
            query.clone(),
            coordinator.clone(),
        )?;
        assert!(
            paired_factory
                .create_paired_query_provider(
                    query.component_id().clone(),
                    json!({}),
                    scope(0),
                    query.clone(),
                    coordinator.clone(),
                )
                .is_err(),
            "query provider leases cannot overlap"
        );
        assert!(Arc::ptr_eq(provider.source_progress().unwrap(), &query));
        assert!(
            matches!(provider.recovery_reader(), Some(SourceProgressReader::Local(actual))
            if Arc::ptr_eq(&actual, &query))
        );
        provider.prepare().await?;
        assert!(provider.snapshot().await?.changes.next().await.is_none());
        provider.complete_snapshot().await?;

        // Resource generation zero must accept a reconstructed live generation.
        let mut live = factory.create_component_with_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(99),
            coordinator.clone(),
            progress(shared, &query, &group),
        )?;
        if shared {
            let live = state.live.lock().unwrap();
            let sdk::SourceBootstrapLiveProgress::Shared(live) = live.as_ref().unwrap() else {
                panic!()
            };
            assert!(
                live.snapshot().is_err(),
                "live readers activate at source start only"
            );
            let progress = state.progress.lock().unwrap();
            let sdk::SourceBootstrapProgress::Shared { consumers, .. } = progress.as_ref().unwrap()
            else {
                panic!()
            };
            assert!(!live.same_binding(consumers));
        } else {
            let live = state.live.lock().unwrap();
            let sdk::SourceBootstrapLiveProgress::Single(live) = live.as_ref().unwrap() else {
                panic!()
            };
            let progress = state.progress.lock().unwrap();
            let sdk::SourceBootstrapProgress::Single(progress) = progress.as_ref().unwrap() else {
                panic!()
            };
            assert!(!live.same_owner(progress));
        }
        live.start().await?;
        assert!(
            factory
                .create_component_with_source_bootstrap(
                    ComponentId::try_new("source")?,
                    json!({}),
                    scope(100),
                    coordinator.clone(),
                    progress(shared, &query, &group),
                )
                .is_err(),
            "a retained live source lease fences reconstruction"
        );
        live.stop().await?;
        assert!(
            factory
                .create_component_with_source_bootstrap(
                    ComponentId::try_new("source")?,
                    json!({}),
                    scope(100),
                    coordinator.clone(),
                    progress(shared, &query, &group),
                )
                .is_err(),
            "a stopped but retained Instance still owns the source lease"
        );
        readers_active(&state, true);
        drop(live);
        let mut live = factory.create_component_with_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(101),
            coordinator.clone(),
            progress(shared, &query, &group),
        )?;
        live.start().await?;
        live.stop().await?;
        state.provider_stop.blocked.store(true, Ordering::SeqCst);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), provider.stop())
                .await
                .is_err()
        );
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 1);
        assert!(provider.prepare().await.is_err());
        readers_active(&state, true);
        state.provider_stop.blocked.store(false, Ordering::SeqCst);
        state.provider_stop.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), provider.stop()).await??;
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 1);
        ResourceCleanup::shutdown(provider.as_ref()).await?;
        assert!(provider.prepare().await.is_err());
        drop(provider);

        state.fail_stop.store(true, Ordering::SeqCst);
        assert!(coordinator.shutdown().await.is_err());
        readers_active(&state, true);
        state.block_stop.store(true, Ordering::SeqCst);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), coordinator.shutdown())
                .await
                .is_err()
        );
        assert_eq!(state.stop_started.load(Ordering::SeqCst), 2);
        readers_active(&state, true);
        assert!(factory
            .create_component_with_source_bootstrap(
                ComponentId::try_new("source")?,
                json!({}),
                scope(102),
                coordinator.clone(),
                progress(shared, &query, &group),
            )
            .is_err());
        assert!(paired_factory
            .create_paired_query_provider(
                query.component_id().clone(),
                json!({}),
                scope(0),
                query.clone(),
                coordinator.clone(),
            )
            .is_err());
        state.stop_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), coordinator.shutdown()).await??;
        assert_eq!(
            state.stop_started.load(Ordering::SeqCst),
            2,
            "resume the original accepted stop"
        );
        readers_active(&state, false);
        assert!(
            live.start().await.is_err(),
            "a retained old source cannot restart a retired coordinator"
        );
        drop(live);
        coordinator.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_current_thread() -> anyhow::Result<()> {
    coordination_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_multi_thread() -> anyhow::Result<()> {
    coordination_matrix().await
}

async fn replacement_matrix() -> anyhow::Result<()> {
    for shared in [false, true] {
        let state = Arc::new(State::default());
        let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
        let paired = NativePlugin::from_definition(paired_definition()?)?;
        let query = owner("query");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), owner("other")],
        )?);
        let coordinator = source.factories()[0].create_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(0),
            progress(shared, &query, &group),
        )?;
        let open = || {
            paired.bootstrap_factories()[0].create_paired_query_provider(
                query.component_id().clone(),
                json!({}),
                scope(0),
                query.clone(),
                coordinator.clone(),
            )
        };
        let old = open()?;
        old.prepare().await?;
        let mut stale_stream = old.snapshot().await?.changes;
        assert!(open().is_err(), "an active snapshot cannot be superseded");

        state.provider_stop.fail.store(true, Ordering::SeqCst);
        assert!(old.stop().await.is_err());
        assert!(open().is_err(), "failed cleanup is not a replacement lease");
        assert!(old.freeze_for_retirement().is_err());
        readers_active(&state, true);

        state.provider_stop.blocked.store(true, Ordering::SeqCst);
        assert!(tokio::time::timeout(Duration::from_millis(20), old.stop())
            .await
            .is_err());
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 2);
        assert!(
            open().is_err(),
            "cancelled waiting cannot publish a stopped lease"
        );
        assert!(old.freeze_for_retirement().is_err());
        readers_active(&state, true);
        state.provider_stop.blocked.store(false, Ordering::SeqCst);
        state.provider_stop.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), old.stop()).await??;
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 2);

        let current = open()?;
        current.prepare().await?;
        assert!(
            old.prepare().await.is_err(),
            "a retained superseded provider cannot resume"
        );
        assert!(old.snapshot().await.is_err());
        assert!(old.complete_snapshot().await.is_err());
        assert!(stale_stream.next().await.unwrap().is_err());
        drop(stale_stream);
        old.stop().await?;
        old.shutdown().await?;
        assert_eq!(
            state.provider_stop.started.load(Ordering::SeqCst),
            2,
            "old STOP/retirement must not invoke the engine after replacement"
        );
        drop(old);
        assert!(current.snapshot().await?.changes.next().await.is_none());
        current.complete_snapshot().await?;
        current.stop().await?;
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 3);
        let last = open()?;
        current.shutdown().await?;
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 3);
        last.prepare().await?;
        last.stop().await?;
        last.shutdown().await?;
        coordinator.shutdown().await?;
        assert!(last.prepare().await.is_err());
        assert!(open().is_err());
        readers_active(&state, false);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_provider_replacement_current_thread() -> anyhow::Result<()> {
    replacement_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_provider_replacement_multi_thread() -> anyhow::Result<()> {
    replacement_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_dropped_unstopped_or_failed_providers_remain_fenced(
) -> anyhow::Result<()> {
    for shared in [false, true] {
        for phase in ["unprepared", "prepared", "failed-stop", "cancelled-stop"] {
            let state = Arc::new(State::default());
            let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
            let paired = NativePlugin::from_definition(paired_definition()?)?;
            let query = owner("query");
            let group = Arc::new(ConsumerProgressSet::new(
                "graph",
                ComponentId::try_new("source")?,
                [query.clone(), owner("other")],
            )?);
            let coordinator = source.factories()[0].create_source_bootstrap(
                ComponentId::try_new("source")?,
                json!({}),
                scope(0),
                progress(shared, &query, &group),
            )?;
            let open = || {
                paired.bootstrap_factories()[0].create_paired_query_provider(
                    query.component_id().clone(),
                    json!({}),
                    scope(0),
                    query.clone(),
                    coordinator.clone(),
                )
            };
            let provider = open()?;
            if phase != "unprepared" {
                provider.prepare().await?;
            }
            if phase == "failed-stop" {
                state.provider_stop.fail.store(true, Ordering::SeqCst);
                assert!(provider.stop().await.is_err());
            }
            if phase == "cancelled-stop" {
                state.provider_stop.blocked.store(true, Ordering::SeqCst);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), provider.stop())
                        .await
                        .is_err()
                );
            }
            drop(provider);
            assert!(
                open().is_err(),
                "dropping a {phase} provider must not erase its cleanup obligation"
            );
            assert_eq!(
                state.open_calls.load(Ordering::SeqCst),
                1,
                "rejected reacquisition must not reach the engine"
            );
            readers_active(&state, true);
            state.provider_stop.blocked.store(false, Ordering::SeqCst);
            state.provider_stop.release.notify_one();
            coordinator.shutdown().await?;
            readers_active(&state, false);
            assert!(open().is_err());
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_dropped_failed_sources_remain_fenced() -> anyhow::Result<()> {
    for shared in [false, true] {
        for phase in [
            "failed-start",
            "failed-stop",
            "cancelled-start",
            "cancelled-stop",
        ] {
            let state = Arc::new(State::default());
            let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
            let query = owner("query");
            let group = Arc::new(ConsumerProgressSet::new(
                "graph",
                ComponentId::try_new("source")?,
                [query.clone(), owner("other")],
            )?);
            let coordinator = source.factories()[0].create_source_bootstrap(
                ComponentId::try_new("source")?,
                json!({}),
                scope(0),
                progress(shared, &query, &group),
            )?;
            let make = |generation| {
                source.factories()[0].create_component_with_source_bootstrap(
                    ComponentId::try_new("source")?,
                    json!({}),
                    scope(generation),
                    coordinator.clone(),
                    progress(shared, &query, &group),
                )
            };
            let never_started = make(1)?;
            drop(never_started);
            let mut live = make(2)?;
            state
                .source_lifecycle
                .fail_stop
                .store(true, Ordering::SeqCst);
            match phase {
                "failed-start" => {
                    state
                        .source_lifecycle
                        .fail_start
                        .store(true, Ordering::SeqCst);
                    assert!(live.start().await.is_err());
                }
                "failed-stop" => {
                    live.start().await?;
                    assert!(live.stop().await.is_err());
                }
                "cancelled-start" => {
                    state
                        .source_lifecycle
                        .block_start
                        .store(true, Ordering::SeqCst);
                    assert!(
                        tokio::time::timeout(Duration::from_millis(20), live.start())
                            .await
                            .is_err()
                    );
                }
                "cancelled-stop" => {
                    live.start().await?;
                    state
                        .source_lifecycle
                        .block_stop
                        .store(true, Ordering::SeqCst);
                    assert!(tokio::time::timeout(Duration::from_millis(20), live.stop())
                        .await
                        .is_err());
                }
                _ => unreachable!(),
            }
            drop(live);
            assert!(
                make(3).is_err(),
                "dropping a {phase} source must not erase unfinished cleanup"
            );
            readers_active(&state, true);
            state
                .source_lifecycle
                .block_start
                .store(false, Ordering::SeqCst);
            state
                .source_lifecycle
                .block_stop
                .store(false, Ordering::SeqCst);
            state.source_lifecycle.release_start.notify_one();
            state.source_lifecycle.release_stop.notify_one();
            coordinator.shutdown().await?;
            readers_active(&state, false);
        }
    }
    Ok(())
}

async fn rejected_replacement_matrix() -> anyhow::Result<()> {
    for shared in [false, true] {
        let state = Arc::new(State::default());
        let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
        let paired = NativePlugin::from_definition(paired_definition()?)?;
        let query = owner("query");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), owner("other")],
        )?);
        let coordinator = source.factories()[0].create_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(0),
            progress(shared, &query, &group),
        )?;
        let open = || {
            paired.bootstrap_factories()[0].create_paired_query_provider(
                query.component_id().clone(),
                json!({}),
                scope(0),
                query.clone(),
                coordinator.clone(),
            )
        };
        let old = open()?;
        old.prepare().await?;
        old.stop().await?;
        for failure in [
            &state.fail_open,
            &state.foreign_open_reader,
            &state.invalid_open_scope,
        ] {
            failure.store(true, Ordering::SeqCst);
            assert!(open().is_err());
            // Rejected construction/validation must not retire the old stopped lease.
            old.prepare().await?;
            assert!(old.snapshot().await?.changes.next().await.is_none());
            old.complete_snapshot().await?;
            assert!(
                open().is_err(),
                "resuming the old provider must fence concurrent replacement"
            );
            old.stop().await?;
        }
        let replacement = open()?;
        replacement.prepare().await?;
        assert!(old.prepare().await.is_err());
        let stops = state.provider_stop.started.load(Ordering::SeqCst);
        old.stop().await?;
        old.shutdown().await?;
        assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), stops);
        assert!(replacement.snapshot().await?.changes.next().await.is_none());
        replacement.complete_snapshot().await?;
        replacement.shutdown().await?;
        coordinator.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_rejected_replacement_preserves_original_current_thread(
) -> anyhow::Result<()> {
    rejected_replacement_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_rejected_replacement_preserves_original_multi_thread(
) -> anyhow::Result<()> {
    rejected_replacement_matrix().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_shared_queries_keep_independent_provider_leases(
) -> anyhow::Result<()> {
    let state = Arc::new(State::default());
    let source = NativePlugin::from_definition(source_definition(true, state.clone())?)?;
    let paired = NativePlugin::from_definition(paired_definition()?)?;
    let left = owner("left");
    let right = owner("right");
    let group = Arc::new(ConsumerProgressSet::new(
        "graph",
        ComponentId::try_new("source")?,
        [left.clone(), right.clone()],
    )?);
    let coordinator = source.factories()[0].create_source_bootstrap(
        ComponentId::try_new("source")?,
        json!({}),
        scope(0),
        NativeSourceBootstrapProgress::Shared(group),
    )?;
    let open = |owner: &Arc<QuerySourceProgress>| {
        paired.bootstrap_factories()[0].create_paired_query_provider(
            owner.component_id().clone(),
            json!({}),
            scope(0),
            owner.clone(),
            coordinator.clone(),
        )
    };
    let first = open(&left)?;
    let second = open(&right)?;
    assert!(Arc::ptr_eq(first.source_progress().unwrap(), &left));
    assert!(Arc::ptr_eq(second.source_progress().unwrap(), &right));
    assert!(first
        .validate_query_scope("instance", "graph", right.component_id())
        .is_err());
    first.prepare().await?;
    second.prepare().await?;
    state.provider_stop.blocked.store(true, Ordering::SeqCst);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), first.stop())
            .await
            .is_err()
    );
    assert!(open(&left).is_err());
    assert!(second.snapshot().await?.changes.next().await.is_none());
    second.complete_snapshot().await?;
    readers_active(&state, true);
    state.provider_stop.blocked.store(false, Ordering::SeqCst);
    state.provider_stop.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), first.stop()).await??;
    let replacement = open(&left)?;
    first.shutdown().await?;
    readers_active(&state, true);
    assert_eq!(state.provider_stop.started.load(Ordering::SeqCst), 1);
    replacement.prepare().await?;
    assert!(replacement.snapshot().await?.changes.next().await.is_none());
    replacement.complete_snapshot().await?;
    replacement.stop().await?;
    replacement.shutdown().await?;
    // Retiring one query binding does not revoke the other member's reader.
    readers_active(&state, true);
    second.stop().await?;
    second.prepare().await?;
    assert!(second.snapshot().await?.changes.next().await.is_none());
    second.complete_snapshot().await?;
    second.shutdown().await?;
    coordinator.shutdown().await?;
    readers_active(&state, false);
    Ok(())
}

struct Subscriptions {
    group: Option<Box<dyn ConsumerProgressUpdates>>,
    members: Vec<Box<dyn SourceProgressUpdates>>,
}

impl Subscriptions {
    fn new(state: &State) -> anyhow::Result<Self> {
        Ok(match state.progress.lock().unwrap().as_ref().unwrap() {
            sdk::SourceBootstrapProgress::Single(reader) => Self {
                group: None,
                members: vec![reader.subscribe()?],
            },
            sdk::SourceBootstrapProgress::Shared { consumers, members } => Self {
                group: Some(consumers.subscribe()?),
                members: members
                    .values()
                    .map(SourceProgressView::subscribe)
                    .collect::<anyhow::Result<_>>()?,
            },
        })
    }

    async fn verify(&mut self, active: bool) -> anyhow::Result<()> {
        if let Some(group) = &mut self.group {
            assert_eq!(group.snapshot().is_ok(), active);
            let deadline = if active {
                Duration::from_millis(10)
            } else {
                Duration::from_secs(2)
            };
            let wait = tokio::time::timeout(deadline, group.changed()).await;
            if active {
                assert!(wait.is_err());
            } else {
                assert!(wait?.is_err());
            }
        }
        for member in &mut self.members {
            assert_eq!(member.snapshot().is_ok(), active);
            let deadline = if active {
                Duration::from_millis(10)
            } else {
                Duration::from_secs(2)
            };
            let wait = tokio::time::timeout(deadline, member.changed()).await;
            if active {
                assert!(wait.is_err());
            } else {
                assert!(wait?.is_err());
            }
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_coordination_stop_keeps_every_subscription_until_cleanup() -> anyhow::Result<()>
{
    for shared in [false, true] {
        let state = Arc::new(State::default());
        let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
        let query = owner("query");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), owner("other")],
        )?);
        let coordinator = source.factories()[0].create_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({}),
            scope(0),
            progress(shared, &query, &group),
        )?;
        let mut subscriptions = Subscriptions::new(&state)?;
        subscriptions.verify(true).await?;
        state.fail_stop.store(true, Ordering::SeqCst);
        assert!(coordinator.shutdown().await.is_err());
        subscriptions.verify(true).await?;
        state.block_stop.store(true, Ordering::SeqCst);
        let (first, second) = tokio::join!(
            tokio::time::timeout(Duration::from_millis(20), coordinator.shutdown()),
            tokio::time::timeout(Duration::from_millis(30), coordinator.shutdown()),
        );
        assert!(first.is_err() && second.is_err());
        assert_eq!(state.stop_started.load(Ordering::SeqCst), 2);
        subscriptions.verify(true).await?;
        let mut late = Subscriptions::new(&state)?;
        late.verify(true).await?;
        state.stop_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), coordinator.shutdown()).await??;
        assert_eq!(state.stop_started.load(Ordering::SeqCst), 2);
        subscriptions.verify(false).await?;
        late.verify(false).await?;
        assert!(Subscriptions::new(&state).is_err());
        coordinator.shutdown().await?;
        assert_eq!(state.stop_started.load(Ordering::SeqCst), 2);
        assert!(coordinator
            .bind_query(query.component_id(), &scope(0), &query)
            .is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_recipes_preserve_secrets_and_exact_owners() -> anyhow::Result<()> {
    use crate::management::{
        HostManagementResources, NativeBootstrapConfig, NativeSourceBootstrapConfig,
    };
    use drasi_lib::management::ManagementResourceResolver;

    for shared in [false, true] {
        let state = Arc::new(State::default());
        let source = NativePlugin::from_definition(source_definition(shared, state.clone())?)?;
        let paired = NativePlugin::from_definition(paired_definition()?)?;
        let mut registry = crate::PluginRegistry::new();
        registry.register_computation_plugin(source.clone())?;
        registry.register_computation_plugin(paired.clone())?;
        let factories = registry.computation_source_bootstrap_factories()?;
        assert_eq!(factories.len(), 1);
        let resolver = HostManagementResources::new(
            &registry,
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?;
        let spec = |id: &str, role| ResourceSpecification {
            id: ResourceId::try_new(id).unwrap(),
            role,
            ownership: ResourceOwnership::Graph,
            binding: id.into(),
        };
        let coordinator_spec = spec("coordinator", ResourceRole::Bootstrap);
        let bootstrap_spec = spec("bootstrap", ResourceRole::Bootstrap);
        let progress_spec = spec("progress", ResourceRole::Checkpoint);
        let secrets_spec = spec("secrets", ResourceRole::SecretStore);
        let query_spec = spec("query-progress", ResourceRole::Checkpoint);
        let query = owner("query");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), owner("other")],
        )?);
        let config = NativeSourceBootstrapConfig {
            component: ComponentId::try_new("source")?,
            implementation: source.factories()[0].metadata().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                "token".into(),
                ConfigurationValue::Reference {
                    resource: secrets_spec.id.clone(),
                    key: "secret:TOKEN".into(),
                    secret: true,
                },
            )]),
            source_progress: (!shared).then(|| progress_spec.id.clone()),
            consumer_progress: shared.then(|| progress_spec.id.clone()),
        };
        let mut recipe = serde_json::to_value(&config)?;
        recipe["kind"] = json!("nativeSourceBootstrap");
        let secret = resolver
            .resolve(
                "instance",
                "graph",
                &secrets_spec,
                &json!({"kind":"configuration", "secrets":{"TOKEN":"fixture-only"}}),
            )
            .await?;
        let progress_handle = if shared {
            ResourceHandle::new(ResourceRole::Checkpoint, group.clone())
        } else {
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(query.clone())),
            )
        };
        let dependencies = BTreeMap::from([
            (progress_spec.id.clone(), progress_handle.clone()),
            (secrets_spec.id.clone(), secret),
        ]);
        let roles: BTreeMap<_, _> = dependencies
            .iter()
            .map(|(id, handle)| (id.clone(), handle.role()))
            .collect();
        assert!(config
            .validate(&coordinator_spec, &BTreeMap::new())
            .is_err());
        let mut invalid = roles.clone();
        invalid.insert(ResourceId::try_new("extra")?, ResourceRole::Checkpoint);
        assert!(config.validate(&coordinator_spec, &invalid).is_err());
        let mut invalid = config.clone();
        invalid.source_progress = Some(progress_spec.id.clone());
        invalid.consumer_progress = Some(progress_spec.id.clone());
        assert!(invalid.validate(&coordinator_spec, &roles).is_err());
        invalid = config.clone();
        invalid.configuration.insert(
            "token".into(),
            ConfigurationValue::Literal(json!("fixture-only")),
        );
        assert!(invalid
            .validate_configuration(&factories, &roles, &dependencies)
            .is_err());
        invalid = config.clone();
        invalid.configuration_version += 1;
        assert!(invalid
            .validate_configuration(&factories, &roles, &dependencies)
            .is_err());
        let coordinator = resolver
            .resolve_with_dependencies(
                "instance",
                "graph",
                &coordinator_spec,
                &recipe,
                &dependencies,
            )
            .await?;
        assert!(coordinator.has_cleanup());
        let actual = coordinator.get::<NativeSourceBootstrapResource>()?;
        let mut wrong = actual.request.clone();
        wrong.configuration_version += 1;
        assert!(actual.validate_source_request(&wrong).is_err());
        wrong = actual.request.clone();
        wrong.implementation = ImplementationIdentity::try_new("foreign", "1")?;
        assert!(actual.validate_source_request(&wrong).is_err());
        wrong = actual.request.clone();
        wrong.configuration["token"] = json!("different");
        assert!(actual.validate_source_request(&wrong).is_err());
        assert_eq!(actual.request.configuration["token"], "fixture-only");
        assert!(!serde_json::to_string(&recipe)?.contains("fixture-only"));
        readers_active(&state, true);

        let bootstrap_config = NativeBootstrapConfig {
            component: query.component_id().clone(),
            implementation: paired.bootstrap_factories()[0]
                .metadata()
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            source_progress: Some(query_spec.id.clone()),
            source_bootstrap: Some(coordinator_spec.id.clone()),
        };
        let mut bootstrap_recipe = serde_json::to_value(&bootstrap_config)?;
        bootstrap_recipe["kind"] = json!("nativeBootstrap");
        let dependencies = BTreeMap::from([
            (coordinator_spec.id.clone(), coordinator),
            (
                query_spec.id.clone(),
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(query.clone())),
                ),
            ),
        ]);
        let mut foreign = dependencies.clone();
        foreign.insert(
            query_spec.id.clone(),
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(owner("query"))),
            ),
        );
        assert!(resolver
            .resolve_with_dependencies(
                "instance",
                "graph",
                &bootstrap_spec,
                &bootstrap_recipe,
                &foreign,
            )
            .await
            .is_err());
        assert!(resolver
            .resolve_with_dependencies(
                "other-instance",
                "graph",
                &bootstrap_spec,
                &bootstrap_recipe,
                &dependencies,
            )
            .await
            .is_err());
        let bootstrap = resolver
            .resolve_with_dependencies(
                "instance",
                "graph",
                &bootstrap_spec,
                &bootstrap_recipe,
                &dependencies,
            )
            .await?;
        assert!(bootstrap.has_cleanup());
        let provider = bootstrap.get::<QueryBootstrapResource>()?;
        assert!(
            matches!(provider.0.recovery_reader(), Some(SourceProgressReader::Local(actual))
            if Arc::ptr_eq(&actual, &query))
        );
        provider.0.prepare().await?;
        assert!(provider.0.snapshot().await?.changes.next().await.is_none());
        provider.0.complete_snapshot().await?;
        provider.0.stop().await?;
        let mut live = source.factories()[0].create_component_with_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({"token":"fixture-only"}),
            scope(44),
            actual.clone(),
            progress(shared, &query, &group),
        )?;
        live.start().await?;
        live.stop().await?;
        drop(live);
        drop(provider);
        drop(bootstrap);
        actual.shutdown().await?;
        readers_active(&state, false);
    }
    Ok(())
}
