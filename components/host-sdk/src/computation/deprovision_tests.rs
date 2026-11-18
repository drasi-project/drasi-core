// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{future::Future, sync::atomic::AtomicBool, task::Poll};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    initializations: AtomicUsize,
    disposals: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    deprovisions: AtomicUsize,
    completed: AtomicUsize,
    block_stop: AtomicBool,
    fail_stop: AtomicBool,
    block_deprovision: AtomicBool,
    fail_deprovision: AtomicBool,
    block_dispose: AtomicBool,
    stop_entered: Notify,
    stop_release: Notify,
    deprovision_entered: Notify,
    deprovision_release: Notify,
    dispose_entered: Notify,
    dispose_release: Notify,
    control_entered: Notify,
    control_release: Notify,
}

struct Factory {
    state: Arc<State>,
    declared: bool,
    implemented: bool,
    control: bool,
    reads: bool,
}

impl sdk::Factory for Factory {
    /// Every field described (fixture).
    fn configuration_details(&self) -> sdk::ConfigDetails {
        sdk::ConfigDetails {
            fields: sdk::Factory::metadata(self)
                .configuration
                .fields
                .into_keys()
                .map(|name| {
                    let details = sdk::configuration::ConfigFieldDetails {
                        description: format!("Fixture field {name}."),
                        ..Default::default()
                    };
                    (name, details)
                })
                .collect(),
        }
    }
    fn describe(
        &self,
        _request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(drasi_computation_plugin_sdk::ComponentDescription::undeclared())
    }
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("test/stateful-transformer", "1")
                .unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: ConfigSchema::default(),
            ports: vec![port(PortDirection::Input), port(PortDirection::Output)],
            completion: None,
            capabilities: Capabilities {
                control: self.control,
                erases_owned_state: self.declared,
                output_snapshots: if self.reads {
                    vec![PortId::try_new("out").unwrap()]
                } else {
                    Vec::new()
                },
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        _services: sdk::ServiceBindings,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let component = StatefulTransformer {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            state: self.state.clone(),
            implemented: self.implemented,
            reads: self.reads,
        };
        Ok(sdk::CreatedComponent {
            component: sdk::Component::Transformer(Box::new(component)),
            control_handler: self.control.then(|| {
                Arc::new(Control(self.state.clone())) as Arc<dyn sdk::NativeControlHandler>
            }),
        })
    }
}

struct StatefulTransformer {
    descriptor: ComponentDescriptor,
    state: Arc<State>,
    implemented: bool,
    reads: bool,
}

#[async_trait]
impl sdk::NativeComponent for StatefulTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<Value> {
        Ok(json!({}))
    }

    async fn initialize(
        &mut self,
        _: &sdk::lifecycle::InitializeContext,
    ) -> sdk::lifecycle::LifecycleResult {
        self.state.initializations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn pause(&mut self, _: &sdk::lifecycle::PauseContext) -> sdk::lifecycle::LifecycleResult {
        Ok(())
    }

    async fn resume(
        &mut self,
        _: &sdk::lifecycle::ResumeContext,
    ) -> sdk::lifecycle::LifecycleResult {
        Ok(())
    }

    async fn dispose(
        &mut self,
        context: &sdk::lifecycle::DisposeContext,
    ) -> sdk::lifecycle::LifecycleResult {
        if context.erases_owned_state() && self.implemented {
            self.delete_owned_resources().await?;
        }
        self.state.disposals.fetch_add(1, Ordering::SeqCst);
        if self.state.block_dispose.load(Ordering::SeqCst) {
            self.state.dispose_entered.notify_one();
            self.state.dispose_release.notified().await;
        }
        Ok(())
    }

    fn output_readers(&self) -> OutputReaders {
        if self.reads {
            OutputReaders::new()
                .with_snapshot(PortId::try_new("out").unwrap(), Arc::new(PendingSnapshot))
        } else {
            OutputReaders::default()
        }
    }

    async fn start(
        &mut self,
        _: &mut sdk::lifecycle::StartContext,
    ) -> sdk::lifecycle::LifecycleResult {
        self.state.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&mut self, _: &sdk::lifecycle::StopContext) -> sdk::lifecycle::LifecycleResult {
        self.state.stops.fetch_add(1, Ordering::SeqCst);
        if self.state.block_stop.load(Ordering::SeqCst) {
            self.state.stop_entered.notify_one();
            self.state.stop_release.notified().await;
        }
        if self.state.fail_stop.load(Ordering::SeqCst) {
            return Err(anyhow::anyhow!("injected stop failure").into());
        }
        Ok(())
    }
}

impl StatefulTransformer {
    async fn delete_owned_resources(&mut self) -> anyhow::Result<()> {
        self.state.deprovisions.fetch_add(1, Ordering::SeqCst);
        if self.state.block_deprovision.load(Ordering::SeqCst) {
            self.state.deprovision_entered.notify_one();
            self.state.deprovision_release.notified().await;
        }
        anyhow::ensure!(
            !self.state.fail_deprovision.load(Ordering::SeqCst),
            "injected deprovision failure"
        );
        self.state.completed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeTransformer for StatefulTransformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(Vec::new())
    }
}

struct PendingSnapshot;

#[async_trait]
impl SnapshotProvider for PendingSnapshot {
    async fn snapshot(&self) -> std::result::Result<OutputSnapshot, OutputReadError> {
        std::future::pending().await
    }
}

struct Control(Arc<State>);

#[async_trait]
impl sdk::NativeControlHandler for Control {
    async fn on_message(
        &self,
        _: sdk::ControlMessage,
        _: sdk::ControlSender,
    ) -> anyhow::Result<()> {
        self.0.control_entered.notify_one();
        self.0.control_release.notified().await;
        Ok(())
    }
}

async fn fixture(
    declared: bool,
    implemented: bool,
    control: bool,
    reads: bool,
) -> anyhow::Result<(NativeComponentProxy, Arc<State>)> {
    let state = Arc::new(State::default());
    let plugin = NativePlugin::from_definition(sdk::PluginDefinition::new(
        "deprovision-test",
        "1",
        vec![Arc::new(Factory {
            state: state.clone(),
            declared,
            implemented,
            control,
            reads,
        })],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let mut proxy = plugin.factories()[0].create_component(
        ComponentId::try_new("d8672572-ab46-4e79-9547-1379df5190bf")?,
        json!({}),
    )?;
    proxy
        .initialize(&crate::computation::test_hooks::initialize_ctx())
        .await?;
    Ok((proxy, state))
}

async fn assert_pending<T>(future: &mut std::pin::Pin<Box<impl Future<Output = T>>>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await
}

async fn capability_is_optional_and_must_match_the_constructed_component_case() -> anyhow::Result<()>
{
    let (mut unsupported, _) = fixture(false, false, false, false).await?;
    assert!(unsupported
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    unsupported
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    unsupported
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;

    // ABI 4 declares erasure only in factory metadata, so an undeclared
    // capability is refused before the plugin runs even when the component
    // could erase.
    let (mut undeclared, state) = fixture(false, true, false, false).await?;
    assert!(undeclared
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);
    Ok(())
}

async fn deletion_requires_a_successful_stop_and_permanently_fences_restart_case(
) -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false).await?;
    transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    assert!(transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);

    state.fail_stop.store(true, Ordering::SeqCst);
    assert!(transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await
        .is_err());
    assert!(transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    state.fail_stop.store(false, Ordering::SeqCst);
    transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert!(transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());

    let (mut unstarted, state) = fixture(true, true, false, false).await?;
    unstarted
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert!(unstarted
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    Ok(())
}

async fn cancelled_stop_is_retained_and_cannot_authorize_deletion_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false).await?;
    transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    state.block_stop.store(true, Ordering::SeqCst);

    let stop_context = crate::computation::test_hooks::stop_ctx();
    let mut stop = Box::pin(transformer.stop(&stop_context));
    assert_pending(&mut stop).await;
    state.stop_entered.notified().await;
    drop(stop);
    assert!(transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);

    state.stop_release.notify_one();
    transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn cancelled_and_failed_deprovision_remain_fenced_and_are_retryable_case(
) -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false).await?;
    transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    state.block_deprovision.store(true, Ordering::SeqCst);

    let erase_context = crate::computation::test_hooks::erase_ctx();
    let mut deletion = Box::pin(transformer.dispose(&erase_context));
    assert_pending(&mut deletion).await;
    state.deprovision_entered.notified().await;
    drop(deletion);
    assert!(transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    assert!(transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await
        .is_err());
    state.deprovision_release.notify_one();
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);

    let (mut failed, state) = fixture(true, true, false, false).await?;
    failed
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    failed
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    state.fail_deprovision.store(true, Ordering::SeqCst);
    assert!(failed
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await
        .is_err());
    assert!(failed
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    state.fail_deprovision.store(false, Ordering::SeqCst);
    failed
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 2);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn deletion_cancels_an_accepted_control_operation_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, true, false).await?;
    transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    let remote = transformer.inner.clone();
    let message = sdk::wire::encode(&sdk::ControlMessage {
        from: ComponentId::try_new("ca2454e5-2adc-48f2-a4eb-902d00105a46")?,
        generation: 1,
        direction: sdk::ControlDirection::Upstream,
        notification: sdk::ControlNotification::Custom {
            kind: "wait".into(),
            payload: Value::Null,
        },
    })?;
    let mut control = Box::pin(remote.unit(abi::operation::CONTROL, &message));
    assert_pending(&mut control).await;
    state.control_entered.notified().await;
    transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    let error = control
        .await
        .expect_err("stopped control reply must be fenced");
    assert!(error.chain().any(|cause| cause
        .downcast_ref::<sdk::transport::Failure>()
        .is_some_and(|failure| matches!(
            failure.code,
            abi::status::CLOSED | abi::status::CANCELLED
        ))));
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn deletion_cancels_unpolled_reads_and_releases_their_provider_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, true).await?;
    transformer
        .start(&crate::computation::test_hooks::start_ctx())
        .await?;
    let readers = transformer.output_readers();
    let snapshot = readers.ports()[&PortId::try_new("out")?]
        .snapshot()
        .cloned()
        .expect("snapshot reader");
    let mut read = Box::pin(snapshot.snapshot());
    assert_pending(&mut read).await;
    transformer
        .stop(&crate::computation::test_hooks::stop_ctx())
        .await?;
    assert!(read.await.is_err());
    transformer
        .dispose(&crate::computation::test_hooks::erase_ctx())
        .await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

macro_rules! runtime_variants {
    ($case:ident, $current:ident, $multi:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $current() -> anyhow::Result<()> {
            tokio::time::timeout(std::time::Duration::from_secs(10), $case()).await?
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $multi() -> anyhow::Result<()> {
            tokio::time::timeout(std::time::Duration::from_secs(10), $case()).await?
        }
    };
}

async fn instance_lifecycle_does_not_repeat_initialization_or_delete_owned_state_case(
) -> anyhow::Result<()> {
    let (mut component, state) = fixture(true, true, false, false).await?;
    for _ in 0..2 {
        component
            .start(&crate::computation::test_hooks::start_ctx())
            .await?;
        component
            .stop(&crate::computation::test_hooks::stop_ctx())
            .await?;
    }
    component
        .dispose(&crate::computation::test_hooks::dispose_ctx())
        .await?;
    component
        .dispose(&crate::computation::test_hooks::dispose_ctx())
        .await?;
    assert_eq!(state.initializations.load(Ordering::SeqCst), 1);
    assert_eq!(state.starts.load(Ordering::SeqCst), 2);
    assert_eq!(state.stops.load(Ordering::SeqCst), 2);
    assert_eq!(state.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);
    assert!(component
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    Ok(())
}

async fn cancelled_disposal_retains_the_original_operation_case() -> anyhow::Result<()> {
    let (mut component, state) = fixture(true, true, false, false).await?;
    state.block_dispose.store(true, Ordering::SeqCst);
    let dispose_context = crate::computation::test_hooks::dispose_ctx();
    let mut disposal = Box::pin(component.dispose(&dispose_context));
    assert_pending(&mut disposal).await;
    state.dispose_entered.notified().await;
    drop(disposal);
    assert!(component
        .start(&crate::computation::test_hooks::start_ctx())
        .await
        .is_err());
    state.dispose_release.notify_one();
    component
        .dispose(&crate::computation::test_hooks::dispose_ctx())
        .await?;
    assert_eq!(state.initializations.load(Ordering::SeqCst), 1);
    assert_eq!(state.starts.load(Ordering::SeqCst), 0);
    assert_eq!(state.stops.load(Ordering::SeqCst), 0);
    assert_eq!(state.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);
    Ok(())
}

runtime_variants!(
    instance_lifecycle_does_not_repeat_initialization_or_delete_owned_state_case,
    instance_lifetime_current_thread,
    instance_lifetime_multi_thread
);
runtime_variants!(
    cancelled_disposal_retains_the_original_operation_case,
    cancelled_disposal_current_thread,
    cancelled_disposal_multi_thread
);
runtime_variants!(
    capability_is_optional_and_must_match_the_constructed_component_case,
    capability_current_thread,
    capability_multi_thread
);
runtime_variants!(
    deletion_requires_a_successful_stop_and_permanently_fences_restart_case,
    stopped_deletion_current_thread,
    stopped_deletion_multi_thread
);
runtime_variants!(
    cancelled_stop_is_retained_and_cannot_authorize_deletion_case,
    cancelled_stop_current_thread,
    cancelled_stop_multi_thread
);
runtime_variants!(
    cancelled_and_failed_deprovision_remain_fenced_and_are_retryable_case,
    retry_deletion_current_thread,
    retry_deletion_multi_thread
);
runtime_variants!(
    deletion_cancels_an_accepted_control_operation_case,
    outstanding_control_current_thread,
    outstanding_control_multi_thread
);
runtime_variants!(
    deletion_cancels_unpolled_reads_and_releases_their_provider_case,
    outstanding_read_current_thread,
    outstanding_read_multi_thread
);
