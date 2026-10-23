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

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use drasi_host_sdk::SourceProxy;
use drasi_lib::channels::{ChannelChangeReceiver, SubscriptionResponse};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::sources::{Source, SourceError};
use drasi_lib::ComponentStatus;
use drasi_plugin_sdk::ffi::{
    build_source_vtable, build_source_vtable_from_boxed, FfiLifecycleEventType, FfiStr,
    SourceVtable,
};

enum Behavior {
    Unavailable(Option<Bytes>),
    Sequence(Option<u64>),
    Other,
    Paused {
        started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<tokio::sync::Notify>,
        host_progress: Arc<AtomicBool>,
        progress_before_return: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        response: std::sync::Mutex<Option<SubscriptionResponse>>,
    },
}

struct ProbeSource(Behavior);

impl Drop for ProbeSource {
    fn drop(&mut self) {
        if let Behavior::Paused { dropped, .. } = &self.0 {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl Source for ProbeSource {
    fn id(&self) -> &str {
        "recovery-probe"
    }

    fn type_name(&self) -> &str {
        "probe"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Running
    }

    async fn initialize(&self, _context: drasi_lib::SourceRuntimeContext) {}

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        match &self.0 {
            Behavior::Sequence(expected) => {
                anyhow::ensure!(
                    settings.resume_sequence == *expected,
                    "resume sequence changed: expected {expected:?}, received {:?}",
                    settings.resume_sequence
                );
                anyhow::bail!("resume sequence preserved")
            }
            Behavior::Unavailable(earliest) => {
                Err(anyhow::Error::new(SourceError::PositionUnavailable {
                    source_id: settings.source_id,
                    requested: settings
                        .resume_from
                        .ok_or_else(|| anyhow::anyhow!("resume position was lost"))?,
                    earliest_available: earliest.clone(),
                })
                .context("subscription recovery probe"))
            }
            Behavior::Other => anyhow::bail!("permission denied by recovery probe"),
            Behavior::Paused {
                started,
                release,
                host_progress,
                progress_before_return,
                response,
                ..
            } => {
                let started = started
                    .lock()
                    .expect("start signal lock poisoned")
                    .take()
                    .expect("probe subscribed more than once");
                let _ = started.send(());
                release.notified().await;
                progress_before_return
                    .store(host_progress.load(Ordering::SeqCst), Ordering::SeqCst);
                response
                    .lock()
                    .expect("response lock poisoned")
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("paused subscription completed"))
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("create plugin probe runtime")
    })
}

extern "C" fn unused_executor(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

fn lifecycle(_: &str, _: FfiLifecycleEventType, _: &str) {}

fn current_library() -> libloading::Library {
    #[cfg(unix)]
    {
        libloading::os::unix::Library::this().into()
    }
    #[cfg(windows)]
    {
        libloading::os::windows::Library::this()
            .expect("load current process library")
            .into()
    }
}

fn source_vtable(behavior: Behavior, boxed: bool) -> SourceVtable {
    let source = ProbeSource(behavior);
    if boxed {
        build_source_vtable_from_boxed(Box::new(source), unused_executor, lifecycle, runtime)
    } else {
        build_source_vtable(source, unused_executor, lifecycle, runtime)
    }
}

fn source(behavior: Behavior, boxed: bool) -> SourceProxy {
    SourceProxy::new(source_vtable(behavior, boxed), Arc::new(current_library()))
}

fn settings(position: Bytes) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: "recovery-probe".into(),
        enable_bootstrap: false,
        query_id: "query".into(),
        nodes: ["Account".into()].into(),
        relations: Default::default(),
        resume_from: Some(position),
        resume_sequence: Some(11),
        request_position_handle: true,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn source_position_unavailable_survives_both_vtable_builders() {
    for boxed in [false, true] {
        for earliest in [
            None,
            Some(Bytes::new()),
            Some(Bytes::from_static(&[9, 0, 2])),
        ] {
            for requested in [Bytes::new(), Bytes::from_static(&[0, 255, 3])] {
                let source = source(Behavior::Unavailable(earliest.clone()), boxed);
                let error = source
                    .subscribe(settings(requested.clone()))
                    .await
                    .err()
                    .expect("subscription must fail");
                let Some(SourceError::PositionUnavailable {
                    source_id,
                    requested: actual,
                    earliest_available,
                }) = error.downcast_ref::<SourceError>()
                else {
                    panic!("structured recovery error was lost: {error:#}");
                };
                assert_eq!(source_id, "recovery-probe");
                assert_eq!(actual, &requested);
                assert_eq!(earliest_available, &earliest);
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn source_generic_failure_does_not_become_position_unavailable() {
    for boxed in [false, true] {
        let error = source(Behavior::Other, boxed)
            .subscribe(settings(Bytes::from_static(&[1])))
            .await
            .err()
            .expect("subscription must fail");
        assert!(error.downcast_ref::<SourceError>().is_none());
        assert!(error
            .to_string()
            .contains("permission denied by recovery probe"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn resume_sequence_presence_survives_both_source_bridges() {
    for boxed in [false, true] {
        for expected in [None, Some(0), Some(1), Some(u64::MAX)] {
            let mut settings = settings(Bytes::new());
            settings.resume_from = None;
            settings.resume_sequence = expected;
            let error = source(Behavior::Sequence(expected), boxed)
                .subscribe(settings)
                .await
                .err()
                .expect("probe returns its observation as an error");
            assert_eq!(error.to_string(), "resume sequence preserved");
        }
    }
}

#[test]
fn both_source_builders_reject_invalid_subscription_parameters() {
    let byte = [0];
    for boxed in [false, true] {
        let vtable = source_vtable(Behavior::Unavailable(None), boxed);
        let subscribe = vtable.subscribe_fn;
        let state = vtable.state;
        let _owner = SourceProxy::new(vtable, Arc::new(current_library()));
        for (nodes, relations, ptr, len, expected) in [
            (
                "{",
                "[]",
                std::ptr::null(),
                0,
                "Invalid subscription node labels",
            ),
            (
                "[]",
                "{",
                std::ptr::null(),
                0,
                "Invalid subscription relation labels",
            ),
            ("[]", "[]", std::ptr::null(), 1, "Null resume position"),
            ("[]", "[]", byte.as_ptr(), 65_537, "64 KiB"),
            ("[]", "[]", std::ptr::null(), 0, "resume position was lost"),
        ] {
            let outcome = subscribe(
                state,
                FfiStr::from_str("recovery-probe"),
                false,
                FfiStr::from_str("query"),
                FfiStr::from_str(nodes),
                FfiStr::from_str(relations),
                ptr,
                len,
                true,
                11,
                true,
            );
            let error = unsafe { outcome.into_result() }.unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
            assert!(error.downcast_ref::<SourceError>().is_none());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn source_proxy_rejects_oversized_positions_without_resetting() {
    let error = source(Behavior::Unavailable(None), true)
        .subscribe(settings(Bytes::from(vec![0; 65_537])))
        .await
        .err()
        .expect("oversized position must be rejected");
    assert!(error.to_string().contains("64 KiB"));
    assert!(error.downcast_ref::<SourceError>().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn source_subscribe_keeps_the_host_runtime_responsive() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let host_progress = Arc::new(AtomicBool::new(false));
    let progress_before_return = Arc::new(AtomicBool::new(false));
    let source = source(
        Behavior::Paused {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: release.clone(),
            host_progress: host_progress.clone(),
            progress_before_return: progress_before_return.clone(),
            dropped: Arc::new(AtomicBool::new(false)),
            response: std::sync::Mutex::new(None),
        },
        true,
    );

    // A separate thread releases a broken blocking bridge so this test fails
    // instead of hanging the entire test process.
    let (rescue_tx, rescue_rx) = std::sync::mpsc::channel();
    let rescue_release = release.clone();
    let rescue = std::thread::spawn(move || {
        if rescue_rx.recv_timeout(Duration::from_secs(2)).is_err() {
            rescue_release.notify_one();
        }
    });
    let host = tokio::spawn(async move {
        started_rx.await.unwrap();
        host_progress.store(true, Ordering::SeqCst);
        release.notify_one();
        let _ = rescue_tx.send(());
    });
    assert!(source.subscribe(settings(Bytes::new())).await.is_err());
    host.await.unwrap();
    rescue.join().unwrap();
    assert!(
        progress_before_return.load(Ordering::SeqCst),
        "source subscribe pinned the host runtime and needed the rescue thread"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_subscribe_keeps_the_plugin_alive_until_the_call_finishes() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let source = Arc::new(source(
        Behavior::Paused {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: release.clone(),
            host_progress: Arc::new(AtomicBool::new(false)),
            progress_before_return: Arc::new(AtomicBool::new(false)),
            dropped: dropped.clone(),
            response: std::sync::Mutex::new(None),
        },
        true,
    ));
    let pending_source = source.clone();
    let pending =
        tokio::spawn(async move { pending_source.subscribe(settings(Bytes::new())).await.err() });
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(source);
    assert!(!dropped.load(Ordering::SeqCst));
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled call retained its completed source");
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_successful_subscribe_releases_the_unused_receiver() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let response = SubscriptionResponse {
        query_id: "query".into(),
        source_id: "recovery-probe".into(),
        receiver: Box::new(ChannelChangeReceiver::new(event_rx)),
        bootstrap_receiver: None,
        position_handle: None,
        bootstrap_result_receiver: None,
    };
    let source = Arc::new(source(
        Behavior::Paused {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: release.clone(),
            host_progress: Arc::new(AtomicBool::new(false)),
            progress_before_return: Arc::new(AtomicBool::new(false)),
            dropped: dropped.clone(),
            response: std::sync::Mutex::new(Some(response)),
        },
        true,
    ));
    let pending_source = source.clone();
    let pending =
        tokio::spawn(async move { pending_source.subscribe(settings(Bytes::new())).await.err() });
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    drop(source);
    assert!(!dropped.load(Ordering::SeqCst));
    assert!(!event_tx.is_closed());
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), event_tx.closed())
        .await
        .expect("cancelled subscription leaked its receiver");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled successful call retained its source");
}
