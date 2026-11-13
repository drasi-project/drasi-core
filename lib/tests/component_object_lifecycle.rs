// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

#[derive(Default)]
struct Calls {
    events: Mutex<Vec<&'static str>>,
    fail_initialize: AtomicBool,
    block_initialize: AtomicBool,
    fail_start: AtomicBool,
    fail_stop: AtomicBool,
    fail_dispose: AtomicBool,
    owns_resource: AtomicBool,
}

impl Calls {
    fn record(&self, event: &'static str) {
        self.events
            .lock()
            .expect("component lifecycle fixture")
            .push(event);
    }

    fn events(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .expect("component lifecycle fixture")
            .clone()
    }
}

struct Member {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    bound: bool,
}

impl Member {
    fn new(calls: Arc<Calls>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(ComponentId::generate(), vec![])
                .expect("component lifecycle fixture"),
            calls,
            bound: false,
        }
    }
}

#[async_trait]
impl ComputationComponent for Member {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn bind_control(&mut self, _: ComponentControl) {
        self.bound = true;
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        assert!(
            self.bound,
            "mandatory control binding precedes initialization"
        );
        self.calls.record("initialize");
        self.calls.owns_resource.store(true, Ordering::Release);
        if self.calls.block_initialize.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        anyhow::ensure!(
            !self.calls.fail_initialize.load(Ordering::Acquire),
            "initialization failed"
        );
        Ok(())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.record("start");
        anyhow::ensure!(
            !self.calls.fail_start.load(Ordering::Acquire),
            "start failed"
        );
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.record("stop");
        anyhow::ensure!(!self.calls.fail_stop.load(Ordering::Acquire), "stop failed");
        Ok(())
    }

    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.calls.record("dispose");
        anyhow::ensure!(
            !self.calls.fail_dispose.load(Ordering::Acquire),
            "disposal failed"
        );
        self.calls.owns_resource.store(false, Ordering::Release);
        Ok(())
    }

    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.calls.record("pause");
        Ok(())
    }

    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.calls.record("resume");
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Member {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

fn graph(calls: Arc<Calls>) -> (ComputationGraph, ComponentId) {
    let member = Member::new(calls);
    let id = member.descriptor.id().clone();
    let graph = ComputationGraph::builder("object-lifecycle")
        .service(
            ComponentName::try_new("member").expect("component lifecycle fixture"),
            Box::new(member),
        )
        .lifecycle_policy(id.clone(), LifecyclePolicy { auto_start: false })
        .build()
        .expect("component lifecycle fixture");
    (graph, id)
}

async fn remove(control: &GraphControl, id: &ComponentId) -> ReconciliationReport {
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![id.clone()]),
                policy: RemovalPolicy::Reject,
            }],
        )
        .await
        .expect("component lifecycle fixture");
    control
        .reconcile(preview, TopologyBindings::default())
        .await
        .expect("component lifecycle fixture")
}

async fn wait_for_disposal(calls: &Calls) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !calls.events().contains(&"dispose") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("component lifecycle fixture");
}

fn both_runtimes(test: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>) {
    for multithreaded in [false, true] {
        let runtime = if multithreaded {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("component lifecycle fixture")
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("component lifecycle fixture")
        };
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), test())
                .await
                .expect("component lifecycle fixture")
        });
    }
}

#[test]
fn initialized_once_across_restart_and_temporary_pause() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (result, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                member
                    .wait_created()
                    .await
                    .expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                member.stop().await.expect("component lifecycle fixture");
                member.stop().await.expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                control
                    .quiesce_components(
                        control.desired_snapshot().revision,
                        GraphSelection::Exact(vec![id.clone()]),
                    )
                    .await
                    .expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                member.stop().await.expect("component lifecycle fixture");
                control.cancel();
            });
            assert!(matches!(result, Err(GraphError::Cancelled)));
            assert_eq!(
                calls.events(),
                ["initialize", "start", "stop", "start", "pause", "resume", "stop", "dispose"]
            );
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}

#[test]
fn initialized_but_never_started_is_disposed() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture")
                    .wait_created()
                    .await
                    .expect("component lifecycle fixture");
                control.cancel();
            });
            assert_eq!(calls.events(), ["initialize", "dispose"]);
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}

#[test]
fn failed_initialization_disposes_partial_resources_without_start_or_stop() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            calls.fail_initialize.store(true, Ordering::Release);
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                assert!(member.wait_created().await.is_err());
                wait_for_disposal(&calls).await;
                assert!(member.start().await.is_err());
                assert!(control.observed().components[&id].failure.is_some());
                control.cancel();
            });
            assert_eq!(calls.events(), ["initialize", "dispose"]);
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}

#[test]
fn failed_start_is_stopped_and_reuses_the_initialized_object() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            calls.fail_start.store(true, Ordering::Release);
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                assert!(member.start().await.is_err());
                member.stop().await.expect("component lifecycle fixture");
                calls.fail_start.store(false, Ordering::Release);
                member.start().await.expect("component lifecycle fixture");
                member.stop().await.expect("component lifecycle fixture");
                control.cancel();
            });
            assert_eq!(
                calls.events(),
                ["initialize", "start", "stop", "start", "stop", "dispose"]
            );
        })
    });
}

#[test]
fn failed_disposal_retains_owner_and_blocks_activation_until_removal_retries() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            calls.fail_dispose.store(true, Ordering::Release);
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                member
                    .wait_created()
                    .await
                    .expect("component lifecycle fixture");
                let report = remove(&control, &id).await;
                assert!(!report.committed);
                assert!(!report.failures.is_empty());
                assert!(calls.owns_resource.load(Ordering::Acquire));
                assert!(member.start().await.is_err());
                calls.fail_dispose.store(false, Ordering::Release);
                assert!(remove(&control, &id).await.committed);
                assert!(
                    member.start().await.is_err(),
                    "retired object handle must remain fenced"
                );
                control.cancel();
            });
            assert_eq!(calls.events(), ["initialize", "dispose", "dispose"]);
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}

#[test]
fn initialization_timeout_disposes_the_retained_object_and_fences_start() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            calls.block_initialize.store(true, Ordering::Release);
            let member = Member::new(calls.clone());
            let id = member.descriptor.id().clone();
            let mut graph = ComputationGraph::builder("initialization-deadline")
                .service(
                    ComponentName::try_new("member").expect("component lifecycle fixture"),
                    Box::new(member),
                )
                .construction_timeout(Duration::from_millis(20))
                .lifecycle_policy(id.clone(), LifecyclePolicy { auto_start: false })
                .build()
                .expect("component lifecycle fixture");
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                assert!(member.wait_created().await.is_err());
                wait_for_disposal(&calls).await;
                assert!(member.start().await.is_err());
                control.cancel();
            });
            assert_eq!(calls.events(), ["initialize", "dispose"]);
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}

#[test]
fn failed_stop_cannot_reactivate_or_release_its_owner_before_retry() {
    both_runtimes(|| {
        Box::pin(async {
            let calls = Arc::new(Calls::default());
            let (mut graph, id) = graph(calls.clone());
            let run = graph.run().expect("component lifecycle fixture");
            let control = run.control();
            let (_, ()) = tokio::join!(run, async {
                control
                    .deployment_report()
                    .await
                    .expect("component lifecycle fixture");
                let member = control
                    .component_handle_by_id(&id)
                    .expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                calls.fail_stop.store(true, Ordering::Release);
                assert!(member.stop().await.is_err());
                assert!(member.start().await.is_err());
                assert!(calls.owns_resource.load(Ordering::Acquire));
                assert_eq!(calls.events(), ["initialize", "start", "stop"]);
                calls.fail_stop.store(false, Ordering::Release);
                member.stop().await.expect("component lifecycle fixture");
                member.start().await.expect("component lifecycle fixture");
                member.stop().await.expect("component lifecycle fixture");
                control.cancel();
            });
            assert_eq!(
                calls.events(),
                ["initialize", "start", "stop", "stop", "start", "stop", "dispose"]
            );
            assert!(!calls.owns_resource.load(Ordering::Acquire));
        })
    });
}
