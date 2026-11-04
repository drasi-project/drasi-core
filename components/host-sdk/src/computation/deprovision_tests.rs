// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::{future::Future, sync::atomic::AtomicBool, task::Poll};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    starts: AtomicUsize,
    stops: AtomicUsize,
    deprovisions: AtomicUsize,
    completed: AtomicUsize,
    block_stop: AtomicBool,
    fail_stop: AtomicBool,
    block_deprovision: AtomicBool,
    fail_deprovision: AtomicBool,
    stop_entered: Notify,
    stop_release: Notify,
    deprovision_entered: Notify,
    deprovision_release: Notify,
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
                deprovision: self.declared,
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

    fn supports_deprovision(&self) -> bool {
        self.implemented
    }

    fn output_readers(&self) -> OutputReaders {
        if self.reads {
            OutputReaders::new()
                .with_snapshot(PortId::try_new("out").unwrap(), Arc::new(PendingSnapshot))
        } else {
            OutputReaders::default()
        }
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.state.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.state.stops.fetch_add(1, Ordering::SeqCst);
        if self.state.block_stop.load(Ordering::SeqCst) {
            self.state.stop_entered.notify_one();
            self.state.stop_release.notified().await;
        }
        anyhow::ensure!(
            !self.state.fail_stop.load(Ordering::SeqCst),
            "injected stop failure"
        );
        Ok(())
    }

    async fn deprovision(&mut self) -> anyhow::Result<()> {
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

fn fixture(
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
    let proxy =
        plugin.factories()[0].create_component(ComponentId::try_new("stateful")?, json!({}))?;
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
    let (mut unsupported, _) = fixture(false, false, false, false)?;
    assert!(unsupported.deprovision().await.is_err());
    unsupported.start().await?;
    unsupported.stop().await?;

    for (declared, implemented) in [(true, false), (false, true)] {
        let error = match fixture(declared, implemented, false, false) {
            Ok(_) => panic!("a mismatched state-removal capability was accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("native deprovision capability mismatch"));
    }
    Ok(())
}

async fn deletion_requires_a_successful_stop_and_permanently_fences_restart_case(
) -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false)?;
    transformer.start().await?;
    assert!(transformer.deprovision().await.is_err());
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);

    state.fail_stop.store(true, Ordering::SeqCst);
    assert!(transformer.stop().await.is_err());
    assert!(transformer.deprovision().await.is_err());
    state.fail_stop.store(false, Ordering::SeqCst);
    transformer.stop().await?;
    transformer.deprovision().await?;
    transformer.deprovision().await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert!(transformer.start().await.is_err());

    let (mut unstarted, state) = fixture(true, true, false, false)?;
    unstarted.deprovision().await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    assert!(unstarted.start().await.is_err());
    Ok(())
}

async fn cancelled_stop_is_retained_and_cannot_authorize_deletion_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false)?;
    transformer.start().await?;
    state.block_stop.store(true, Ordering::SeqCst);

    let mut stop = Box::pin(transformer.stop());
    assert_pending(&mut stop).await;
    state.stop_entered.notified().await;
    drop(stop);
    assert!(transformer.deprovision().await.is_err());
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);

    state.stop_release.notify_one();
    transformer.stop().await?;
    assert_eq!(state.stops.load(Ordering::SeqCst), 1);
    transformer.deprovision().await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn cancelled_and_failed_deprovision_remain_fenced_and_are_retryable_case(
) -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, false)?;
    transformer.start().await?;
    transformer.stop().await?;
    state.block_deprovision.store(true, Ordering::SeqCst);

    let mut deletion = Box::pin(transformer.deprovision());
    assert_pending(&mut deletion).await;
    state.deprovision_entered.notified().await;
    drop(deletion);
    assert!(transformer.start().await.is_err());
    assert!(transformer.stop().await.is_err());
    state.deprovision_release.notify_one();
    transformer.deprovision().await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);

    let (mut failed, state) = fixture(true, true, false, false)?;
    failed.start().await?;
    failed.stop().await?;
    state.fail_deprovision.store(true, Ordering::SeqCst);
    assert!(failed.deprovision().await.is_err());
    assert!(failed.start().await.is_err());
    state.fail_deprovision.store(false, Ordering::SeqCst);
    failed.deprovision().await?;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 2);
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn deletion_waits_for_an_accepted_control_operation_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, true, false)?;
    transformer.start().await?;
    let remote = transformer.inner.clone();
    let message = sdk::wire::encode(&sdk::ControlMessage {
        from: ComponentId::try_new("observer")?,
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
    transformer.stop().await?;

    let mut deletion = Box::pin(transformer.deprovision());
    assert_pending(&mut deletion).await;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);
    state.control_release.notify_one();
    control.await?;
    deletion.await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

async fn deletion_waits_for_unpolled_reads_to_release_their_provider_case() -> anyhow::Result<()> {
    let (mut transformer, state) = fixture(true, true, false, true)?;
    transformer.start().await?;
    let readers = transformer.output_readers();
    let snapshot = readers.ports()[&PortId::try_new("out")?]
        .snapshot()
        .cloned()
        .expect("snapshot reader");
    let mut read = Box::pin(snapshot.snapshot());
    assert_pending(&mut read).await;
    transformer.stop().await?;

    let mut deletion = Box::pin(transformer.deprovision());
    assert_pending(&mut deletion).await;
    assert_eq!(state.deprovisions.load(Ordering::SeqCst), 0);
    drop(read);
    deletion.await?;
    assert_eq!(state.completed.load(Ordering::SeqCst), 1);
    Ok(())
}

macro_rules! runtime_variants {
    ($case:ident, $current:ident, $multi:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $current() -> anyhow::Result<()> {
            $case().await
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $multi() -> anyhow::Result<()> {
            $case().await
        }
    };
}

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
    deletion_waits_for_an_accepted_control_operation_case,
    outstanding_control_current_thread,
    outstanding_control_multi_thread
);
runtime_variants!(
    deletion_waits_for_unpolled_reads_to_release_their_provider_case,
    outstanding_read_current_thread,
    outstanding_read_multi_thread
);
