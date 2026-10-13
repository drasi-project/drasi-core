// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}

struct RetiredMetadata {
    dropped: Arc<AtomicBool>,
    panic: bool,
}

impl Drop for RetiredMetadata {
    fn drop(&mut self) {
        assert!(tokio::task::try_id().is_some());
        self.dropped.store(true, Ordering::SeqCst);
        if self.panic {
            panic!("metadata destructor during final cleanup");
        }
    }
}

async fn cleanup_independent_retirement_and_pipe_tasks() {
    for panic in [false, true] {
        let mut graph = ComputationGraph::builder("independent-cleanup")
            .service(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(id("service"), vec![]).unwrap(),
                calls: Arc::new(AtomicUsize::new(0)),
                handler: None,
            }))
            .build()
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let metadata = RetiredMetadata {
            dropped: dropped.clone(),
            panic,
        };
        let mut operations = Operations::default();
        retire_callback(
            &graph,
            &mut operations,
            Some(graph.components[0].clone()),
            move |_| {
                drop(metadata);
                Ok(())
            },
        )
        .unwrap();
        let identity = super::super::execution::GraphIdentity {
            graph: graph.snapshot.id.clone(),
            run_epoch: graph.observed().run_epoch,
            phase: FailurePhase::Creation,
        };
        let (abort, _registration) = AbortHandle::new_pair();
        let pipe = graph
            .tasks
            .pipes
            .spawn(identity, abort, async {
                Ok(ProvidedBinding {
                    control: None,
                    outgoing: None,
                    incoming: None,
                })
            })
            .unwrap();
        drop(pipe);
        drop(operations);
        assert!(graph.tasks.preparations.is_empty());
        assert!(!graph.tasks.pipes.is_empty());
        assert!(!graph.tasks.retirements.is_empty());
        let errors = cleanup(&mut graph).await;
        assert!(dropped.load(Ordering::SeqCst));
        assert!(graph.tasks.is_empty());
        if panic {
            assert!(errors.iter().any(GraphError::damages_generation));
            assert_eq!(
                graph.observed().components[&id("service")].lifecycle,
                ComponentLifecycle::Failed
            );
        } else {
            assert!(errors.is_empty(), "{errors:?}");
        }
        graph.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cleanup_independent_retirement_and_pipe_tasks_current_thread() {
    cleanup_independent_retirement_and_pipe_tasks().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleanup_independent_retirement_and_pipe_tasks_multi_thread() {
    cleanup_independent_retirement_and_pipe_tasks().await;
}

struct Service {
    descriptor: ComponentDescriptor,
    calls: Arc<AtomicUsize>,
    handler: Option<Arc<dyn ControlHandler>>,
}
#[async_trait::async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.handler.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

struct PanicHandler;
#[async_trait::async_trait]
impl ControlHandler for PanicHandler {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        if matches!(message.notification, ControlNotification::Custom { .. }) {
            panic!("control panic while start completion is pending");
        }
        Ok(())
    }
}

struct QuarantineAtPublication {
    slot: Arc<InstanceSlot>,
    fired: AtomicBool,
}
impl PublicationObserver for QuarantineAtPublication {
    fn publish(&self, _: &ComputationInspection, current: &ComputationInspection) {
        if current.observed.components[&self.slot.id].lifecycle == ComponentLifecycle::Running
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            self.slot.quarantine(GraphError::Component {
                component: self.slot.id.clone(),
                operation: "control notification",
                source: ComponentPanic::new(anyhow::anyhow!(
                    "panic between validation and dispatch"
                ))
                .into(),
            });
        }
    }
}

async fn start_completion_race() {
    for during_publication in [false, true] {
        let bad_calls = Arc::new(AtomicUsize::new(0));
        let healthy_calls = Arc::new(AtomicUsize::new(0));
        let service = |name: &str, calls, handler| {
            Box::new(Service {
                descriptor: ComponentDescriptor::try_new(id(name), vec![]).unwrap(),
                calls,
                handler,
            })
        };
        let mut graph = ComputationGraph::builder("start-panic-race")
            .service(service(
                "bad",
                bad_calls.clone(),
                Some(Arc::new(PanicHandler)),
            ))
            .service(service("healthy", healthy_calls.clone(), None))
            .build()
            .unwrap();
        graph.snapshot.control_connections = vec![(id("healthy"), id("bad"))].into();
        let (_cancel, mut cancel) = watch::channel(false);
        let mut operations = Operations::default();
        let controls = deploy(&mut graph, &mut cancel, &mut operations)
            .await
            .unwrap();
        operations.refresh_connections(&graph).unwrap();
        let bad = graph.ids[&id("bad")];
        let healthy = graph.ids[&id("healthy")];
        operations.begin_start(&graph, BTreeSet::from([bad]), None, true);
        operations.advance_start(&graph).unwrap();
        let completed = operations.futures.next().await.unwrap();
        assert!(completed.result.as_ref().is_ok_and(|result| result.is_ok()));

        let observer: Option<Arc<dyn PublicationObserver>> = if during_publication {
            let observer: Arc<dyn PublicationObserver> = Arc::new(QuarantineAtPublication {
                slot: graph.components[bad].clone(),
                fired: AtomicBool::new(false),
            });
            graph.inspector.observe(&observer);
            Some(observer)
        } else {
            operations.peer_controls[&healthy]
                .notify_neighbor(
                    &id("bad"),
                    ControlNotification::Custom {
                        kind: "panic".into(),
                        payload: serde_json::Value::Null,
                    },
                )
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while graph.components[bad].damage().is_none() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            None
        };
        operations
            .complete(&graph, completed, &controls)
            .expect("a quarantined start is a local outcome, not driver failure");
        operations.advance_start(&graph).unwrap();
        assert!(matches!(
            graph.observed().startup.as_ref().unwrap().components[&id("bad")],
            StartOutcome::StartFailed(_)
        ));
        assert_eq!(
            graph.observed().components[&id("bad")].lifecycle,
            ComponentLifecycle::Failed
        );
        assert!(graph.observed().components[&id("bad")]
            .failure
            .as_ref()
            .unwrap()
            .cause
            .damages_generation());
        assert_eq!(bad_calls.load(Ordering::SeqCst), 0);
        operations.begin_start(&graph, BTreeSet::from([healthy]), None, true);
        operations.advance_start(&graph).unwrap();
        let completed = operations.futures.next().await.unwrap();
        operations.complete(&graph, completed, &controls).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while healthy_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(graph.observed().components[&id("healthy")]
            .failure
            .is_none());
        drop(observer);
        drop(operations);
        drop(controls);
        graph.shutdown().await.unwrap();
        assert!(graph.tasks.is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn start_success_racing_control_quarantine_current_thread() {
    start_completion_race().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_success_racing_control_quarantine_multi_thread() {
    start_completion_race().await;
}

struct Resource(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ResourceCleanup for Resource {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct Constructor(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ResourceConstructor for Constructor {
    async fn construct(&self) -> anyhow::Result<ResourceHandle> {
        let resource = Arc::new(Resource(self.0.clone()));
        Ok(ResourceHandle::new(ResourceRole::StateStore, resource.clone()).with_cleanup(resource))
    }
}

async fn resource_without_control_or_drain() {
    let mut graph = ComputationGraph::empty("unconsumed-resource").unwrap();
    let resource = ResourceId::try_new("resource").unwrap();
    graph.snapshot.resources.insert(
        resource.clone(),
        ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "resource".into(),
        },
    );
    update(&graph, |state| {
        state.resources.insert(
            resource.clone(),
            ObservedResource {
                realization: ResourceRealization::Pending,
                generation: 1,
                revision: state.revision,
                transition_time: Utc::now(),
                failure: None,
            },
        );
    });
    let closes = Arc::new(AtomicUsize::new(0));
    let job = super::super::resource_tasks::construct(
        &graph,
        &resource,
        Arc::new(Constructor(closes.clone())),
        BTreeMap::new(),
        ResourceRole::StateStore,
    )
    .unwrap();
    job.wait().await.unwrap();
    assert!(graph.tasks.controls.is_empty() && graph.tasks.drains.is_empty());
    assert!(graph.tasks.resources.contains(&resource));
    assert!(!graph.resource_handles.contains_key(&resource));
    graph.shutdown().await.unwrap();
    assert!(
        graph.tasks.is_empty(),
        "successful shutdown must settle every job family"
    );
    assert!(
        graph.resource_handles.contains_key(&resource),
        "the completed constructor value must be published, not dropped"
    );
    graph.dispose().await.unwrap();
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert!(graph.tasks.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn unconsumed_resource_without_drains_is_settled_current_thread() {
    resource_without_control_or_drain().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconsumed_resource_without_drains_is_settled_multi_thread() {
    resource_without_control_or_drain().await;
}

#[test]
fn force_scope_rejects_unfinished_resource_cleanup_before_effects() {
    let mut graph = ComputationGraph::builder("unfinished-resource-scope")
        .service(Box::new(Service {
            descriptor: ComponentDescriptor::try_new(id("user"), vec![]).unwrap(),
            calls: Arc::default(),
            handler: None,
        }))
        .build()
        .unwrap();
    let resource = ResourceId::try_new("resource").unwrap();
    graph.snapshot.resources.insert(
        resource.clone(),
        ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "resource".into(),
        },
    );
    graph
        .snapshot
        .component_resources
        .insert(id("user"), BTreeSet::from([resource.clone()]));
    let selected = BTreeSet::from([graph.ids[&id("user")]]);
    assert!(Operations::force_scope(&graph, &selected).is_ok());
    graph.pending_resource_cleanup.insert(
        resource.clone(),
        ResourceHandle::new(ResourceRole::StateStore, Arc::new(1usize)),
    );
    let rejected = Operations::force_scope(&graph, &selected).unwrap_err();
    assert!(matches!(rejected, GraphError::Validation { source, .. }
        if source.downcast_ref::<ForceCancellationResourceUnavailable>()
            .is_some_and(|unavailable| unavailable.resource == resource)));
    assert!(graph.components[selected.first().copied().unwrap()]
        .damage()
        .is_none());
    graph.pending_resource_cleanup.clear();
}

#[test]
fn reserved_resource_identity_fences_aliases_without_retaining_ownership() {
    let provider = Arc::new(7usize);
    let original = ResourceHandle::new(ResourceRole::StateStore, Arc::new(1usize))
        .with_shared_identity(provider.clone());
    let reservation = original.scope_identity();
    let alias = ResourceHandle::new(ResourceRole::StateStore, Arc::new(2usize))
        .with_shared_identity(provider.clone());
    let unrelated = ResourceHandle::new(ResourceRole::StateStore, Arc::new(3usize))
        .with_shared_identity(Arc::new(7usize));
    assert!(reservation.matches(&original));
    assert!(reservation.matches(&alias));
    assert!(!reservation.matches(&unrelated));
    let weak = Arc::downgrade(&provider);
    drop(provider);
    drop(original);
    drop(alias);
    assert!(
        weak.upgrade().is_none(),
        "a reservation must not retain storage ownership"
    );
    assert!(!reservation.matches(&unrelated));
}
