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

use crate::{KubernetesSource, KubernetesSourceBuilder, ResourceSpec};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use drasi_lib::channels::ComponentStatus;
use drasi_lib::component_graph::ComponentUpdate;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::Source;
use serde_json::json;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};

struct MockApiState {
    list_status: AtomicU16,
    watch_status: AtomicU16,
    next_watch_error: AtomicU16,
    watch_count: watch::Sender<usize>,
    watches: Mutex<Vec<mpsc::UnboundedSender<String>>>,
}

struct MockApi {
    address: std::net::SocketAddr,
    state: Arc<MockApiState>,
    task: tokio::task::JoinHandle<()>,
}

impl MockApi {
    async fn new() -> Self {
        let state = Arc::new(MockApiState {
            list_status: AtomicU16::new(200),
            watch_status: AtomicU16::new(200),
            next_watch_error: AtomicU16::new(0),
            watch_count: watch::channel(0).0,
            watches: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .fallback(mock_api_handler)
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn source(&self) -> KubernetesSource {
        self.source_builder().build().unwrap()
    }

    fn source_builder(&self) -> KubernetesSourceBuilder {
        let kubeconfig = json!({
            "apiVersion": "v1",
            "kind": "Config",
            "clusters": [{"name": "test", "cluster": {"server": format!("http://{}", self.address)}}],
            "contexts": [{"name": "test", "context": {"cluster": "test", "user": "test"}}],
            "current-context": "test",
            "users": [{"name": "test", "user": {"token": "test-token"}}]
        });
        KubernetesSource::builder("test-source")
            .with_resources(vec![ResourceSpec {
                api_version: "v1".to_string(),
                kind: "Pod".to_string(),
            }])
            .with_namespaces(vec!["test".to_string()])
            .with_kubeconfig_content(kubeconfig.to_string())
    }

    async fn wait_for_watches(&self, expected: usize) {
        let mut counts = self.state.watch_count.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            counts.wait_for(|count| *count >= expected).await.unwrap();
        })
        .await
        .expect("expected Kubernetes watch request");
    }

    async fn assert_watches_closed(&self) {
        let watches = self.state.watches.lock().unwrap().clone();
        tokio::time::timeout(Duration::from_secs(3), async {
            for sender in watches {
                sender.closed().await;
            }
        })
        .await
        .expect("stopping must close every active watch stream");
    }

    fn send_watch_error(&self, code: u16) {
        self.send_watch_event(json!({"type": "ERROR", "object": api_error(code)}));
    }

    fn send_watch_event(&self, event: serde_json::Value) {
        let event = format!("{event}\n");
        let mut watches = self.state.watches.lock().unwrap();
        watches.retain(|sender| sender.send(event.clone()).is_ok());
        assert!(!watches.is_empty(), "expected an active watch");
    }
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn api_error(code: u16) -> serde_json::Value {
    json!({
        "kind": "Status",
        "apiVersion": "v1",
        "status": "Failure",
        "message": format!("mock Kubernetes API failure ({code})"),
        "reason": "MockFailure",
        "code": code
    })
}

async fn mock_api_handler(
    State(state): State<Arc<MockApiState>>,
    Query(params): Query<HashMap<String, String>>,
    uri: Uri,
) -> Response {
    if !uri.path().ends_with("/pods") {
        return (StatusCode::FORBIDDEN, Json(api_error(403))).into_response();
    }
    if params.get("watch").is_some_and(|value| value == "true") {
        state.watch_count.send_modify(|count| *count += 1);
        let next_error = state.next_watch_error.swap(0, Ordering::SeqCst);
        let code = if next_error == 0 {
            state.watch_status.load(Ordering::SeqCst)
        } else {
            next_error
        };
        if code != 200 {
            if code == 502 {
                return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
            }
            return (StatusCode::from_u16(code).unwrap(), Json(api_error(code))).into_response();
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        state.watches.lock().unwrap().push(sender);
        let stream = futures::stream::unfold(receiver, |mut receiver| async {
            receiver
                .recv()
                .await
                .map(|event| (Ok::<_, Infallible>(event), receiver))
        });
        return (
            [("content-type", "application/json")],
            Body::from_stream(stream),
        )
            .into_response();
    }

    let code = state.list_status.load(Ordering::SeqCst);
    if code != 200 {
        return (StatusCode::from_u16(code).unwrap(), Json(api_error(code))).into_response();
    }
    Json(json!({
        "apiVersion": "v1",
        "kind": "PodList",
        "metadata": {"resourceVersion": "1"},
        "items": []
    }))
    .into_response()
}

async fn wait_for_error(source: &KubernetesSource) {
    let mut status = source.base.status_handle().subscribe_status();
    tokio::time::timeout(Duration::from_secs(3), async {
        status
            .wait_for(|status| *status == ComponentStatus::Error)
            .await
            .unwrap();
    })
    .await
    .expect("fatal watch failures must report Error");
}

#[tokio::test]
async fn startup_rejects_invalid_kubeconfig() {
    let source = KubernetesSource::builder("invalid-config")
        .with_resources(vec![ResourceSpec {
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
        }])
        .with_kubeconfig_content("not a kubeconfig")
        .build()
        .unwrap();
    source
        .start()
        .await
        .expect_err("invalid client configuration");
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.base.task_handle.read().await.is_none());
    source.stop().await.unwrap();
    assert_eq!(source.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn startup_rejects_fatal_list_errors_without_running() {
    for code in [400, 401, 403, 404, 422] {
        let api = MockApi::new().await;
        api.state.list_status.store(code, Ordering::SeqCst);
        let source = api.source();
        let (updates, mut receiver) = mpsc::channel(16);
        source
            .initialize(SourceRuntimeContext::new(
                "test",
                source.id(),
                None,
                updates,
                None,
            ))
            .await;

        let error = source.start().await.expect_err("fatal list failure");
        assert!(format!("{error:#}").contains(&code.to_string()));
        assert_eq!(source.status().await, ComponentStatus::Error);
        assert!(source.base.task_handle.read().await.is_none());
        assert!(source.base.shutdown_tx.read().await.is_none());
        let mut statuses = Vec::new();
        while let Ok(ComponentUpdate::Status { status, .. }) = receiver.try_recv() {
            statuses.push(status);
        }
        assert_eq!(
            statuses,
            vec![ComponentStatus::Starting, ComponentStatus::Error]
        );

        source.stop().await.unwrap();
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
}

#[tokio::test]
async fn startup_rejects_watch_denied_after_list_succeeds() {
    let api = MockApi::new().await;
    api.state.watch_status.store(403, Ordering::SeqCst);
    let source = api.source();
    let result = source.start().await;
    assert!(
        result.is_err(),
        "watch permission is required (watch requests: {})",
        *api.state.watch_count.borrow()
    );
    let error = result.unwrap_err();
    assert!(format!("{error:#}").contains("403"));
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.base.task_handle.read().await.is_none());
    api.assert_watches_closed().await;

    api.state.watch_status.store(200, Ordering::SeqCst);
    source.start().await.unwrap();
    assert_eq!(source.status().await, ComponentStatus::Running);
    source.stop().await.unwrap();
    api.assert_watches_closed().await;
}

#[tokio::test]
async fn fatal_watch_response_after_start_reports_error() {
    for code in [401, 403, 404] {
        let api = MockApi::new().await;
        let source = api.source();
        source.start().await.unwrap();
        api.state.watch_status.store(code, Ordering::SeqCst);

        wait_for_error(&source).await;
        source.stop().await.unwrap();
        assert!(source.base.task_handle.read().await.is_none());
        assert_eq!(source.status().await, ComponentStatus::Stopped);
        api.assert_watches_closed().await;
    }
}

#[tokio::test]
async fn fatal_watch_event_after_start_reports_error() {
    let api = MockApi::new().await;
    let source = api.source();
    source.start().await.unwrap();
    api.wait_for_watches(2).await;
    api.send_watch_error(401);

    wait_for_error(&source).await;
    source.stop().await.unwrap();
    assert!(source.base.task_handle.read().await.is_none());
    api.assert_watches_closed().await;
}

#[tokio::test]
async fn transient_watch_failure_retries_and_stop_joins() {
    let api = MockApi::new().await;
    let source = api.source();
    source.start().await.unwrap();
    api.state.next_watch_error.store(503, Ordering::SeqCst);
    api.wait_for_watches(3).await;
    assert_eq!(source.status().await, ComponentStatus::Running);

    source.stop().await.unwrap();
    assert!(source.base.task_handle.read().await.is_none());
    assert!(source.base.shutdown_tx.read().await.is_none());
    assert_eq!(source.status().await, ComponentStatus::Stopped);
    api.assert_watches_closed().await;
    source.stop().await.unwrap();

    source.start().await.unwrap();
    source.stop().await.unwrap();
    api.assert_watches_closed().await;
}

#[tokio::test]
async fn startup_retries_transient_watch_errors() {
    for code in [502, 503] {
        let api = MockApi::new().await;
        api.state.next_watch_error.store(code, Ordering::SeqCst);
        let source = api.source();
        source.start().await.unwrap();
        assert!(*api.state.watch_count.borrow() >= 2);
        assert_eq!(source.status().await, ComponentStatus::Running);
        source.stop().await.unwrap();
        api.assert_watches_closed().await;
    }
}

#[tokio::test]
async fn watch_delivers_changes_across_stop_start() {
    use drasi_core::models::SourceChange;
    use drasi_lib::channels::SourceEvent;

    let api = MockApi::new().await;
    let source = api.source();
    let pod = json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": "test-pod", "namespace": "test", "uid": "pod-1", "resourceVersion": "2"},
        "spec": {"containers": [{"name": "app", "image": "example:v1"}]}
    });
    for _ in 0..2 {
        let expected_watches = *api.state.watch_count.borrow() + 2;
        let mut receiver = source.base.create_streaming_receiver().await.unwrap();
        source.start().await.unwrap();
        api.wait_for_watches(expected_watches).await;
        api.send_watch_event(json!({"type": "ADDED", "object": pod}));
        let insert = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &insert.event,
            SourceEvent::Change(SourceChange::Insert { element })
                if element.get_reference().element_id.as_ref() == "pod:pod-1"
        ));
        api.send_watch_event(json!({"type": "DELETED", "object": pod}));
        let delete = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &delete.event,
            SourceEvent::Change(SourceChange::Delete { metadata })
                if metadata.reference.element_id.as_ref() == "pod:pod-1"
        ));
        source.stop().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), receiver.recv())
                .await
                .unwrap()
                .is_err()
        );
        api.assert_watches_closed().await;
    }
}

#[tokio::test]
async fn startup_failure_closes_already_validated_watches() {
    let api = MockApi::new().await;
    let source = api
        .source_builder()
        .with_resources(vec![
            ResourceSpec {
                api_version: "v1".to_string(),
                kind: "Pod".to_string(),
            },
            ResourceSpec {
                api_version: "v1".to_string(),
                kind: "ConfigMap".to_string(),
            },
        ])
        .build()
        .unwrap();
    let error = source
        .start()
        .await
        .expect_err("second resource is forbidden");
    assert!(format!("{error:#}").contains("ConfigMap"));
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.base.task_handle.read().await.is_none());
    api.assert_watches_closed().await;
    source.stop().await.unwrap();
}

#[tokio::test]
async fn startup_retries_are_bounded() {
    let api = MockApi::new().await;
    api.state.list_status.store(503, Ordering::SeqCst);
    let source = api.source();
    let error = tokio::time::timeout(Duration::from_secs(12), source.start())
        .await
        .expect("startup must not wait indefinitely")
        .expect_err("persistent transient failures must time out");
    assert!(format!("{error:#}").contains("timed out"));
    assert_eq!(source.status().await, ComponentStatus::Error);
    assert!(source.base.task_handle.read().await.is_none());
    assert!(source.base.shutdown_tx.read().await.is_none());
    source.stop().await.unwrap();
}

#[tokio::test]
async fn stop_waits_for_aborted_task_resources() {
    struct TaskResource(Arc<AtomicBool>);
    impl Drop for TaskResource {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let api = MockApi::new().await;
    let source = api.source();
    let released = Arc::new(AtomicBool::new(false));
    let resource = TaskResource(released.clone());
    source
        .base
        .set_task_handle(tokio::spawn(async move {
            let _resource = resource;
            std::future::pending::<()>().await;
        }))
        .await;

    source.stop().await.unwrap();
    assert!(
        released.load(Ordering::SeqCst),
        "aborting is not enough: stop must await resource destruction"
    );
    assert!(source.base.task_handle.read().await.is_none());
    assert_eq!(source.status().await, ComponentStatus::Stopped);
}

#[tokio::test]
async fn stop_reports_unexpected_task_failure() {
    let api = MockApi::new().await;
    let source = api.source();
    let task = tokio::spawn(async { panic!("simulated Kubernetes task panic") });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    source.base.set_task_handle(task).await;

    let error = source
        .stop()
        .await
        .expect_err("task panic must not be ignored");
    assert!(error
        .downcast_ref::<tokio::task::JoinError>()
        .unwrap()
        .is_panic());
    assert!(source.base.task_handle.read().await.is_none());
    assert_eq!(source.status().await, ComponentStatus::Error);
    source.stop().await.unwrap();
}
