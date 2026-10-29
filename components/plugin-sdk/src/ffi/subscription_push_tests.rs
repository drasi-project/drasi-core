use super::*;
use crate::ffi::payload::{consume_bootstrap_event, consume_source_event};
use drasi_core::models::{ElementMetadata, ElementReference};
use drasi_lib::channels::events::{SourceEvent, SourceEventWrapper};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Mutex,
};
use std::time::Duration;

struct Receiver(tokio::sync::mpsc::Receiver<Arc<SourceEventWrapper>>);

#[async_trait::async_trait]
impl ChangeReceiver<SourceEventWrapper> for Receiver {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        self.0
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("source closed"))
    }
}

struct CallbackContext {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    finished: mpsc::Sender<()>,
    events: AtomicUsize,
    sentinels: AtomicUsize,
    decoded: AtomicBool,
}

impl CallbackContext {
    fn event(&self, decoded: bool) -> bool {
        self.events.fetch_add(1, Ordering::SeqCst);
        self.decoded.store(decoded, Ordering::SeqCst);
        let _ = self.entered.send(());
        let _ = self
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5));
        false
    }

    fn finish(&self) -> bool {
        self.sentinels.fetch_add(1, Ordering::SeqCst);
        let _ = self.finished.send(());
        false
    }
}

extern "C" fn source_callback(ctx: *mut c_void, event: *mut FfiSourceEvent) -> bool {
    let context = unsafe { &*(ctx as *const CallbackContext) };
    if event.is_null() {
        return context.finish();
    }
    let decoded = unsafe { consume_source_event(&*event) }.is_some();
    unsafe { drop(Box::from_raw(event)) };
    context.event(decoded)
}

extern "C" fn bootstrap_callback(ctx: *mut c_void, event: *mut FfiBootstrapEvent) -> bool {
    let context = unsafe { &*(ctx as *const CallbackContext) };
    if event.is_null() {
        return context.finish();
    }
    let decoded = unsafe { consume_bootstrap_event(&*event) }.is_some();
    unsafe { drop(Box::from_raw(event)) };
    context.event(decoded)
}

extern "C" fn unused_executor(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

fn assert_push_backpressure_does_not_starve_runtime(bootstrap: bool, shutdown_runtime: bool) {
    let mut runtime = Some(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap(),
    );
    let (events_tx, events_rx) = tokio::sync::mpsc::channel(1);
    let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel(1);
    let change = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "one"),
            labels: vec!["Node".into()].into(),
            effective_from: 1,
        },
    };
    if bootstrap {
        bootstrap_tx
            .try_send(BootstrapEvent {
                source_id: "source".into(),
                change,
                timestamp: chrono::Utc::now(),
                sequence: 1,
            })
            .unwrap();
    } else {
        events_tx
            .try_send(Arc::new(SourceEventWrapper::new(
                "source".into(),
                SourceEvent::Change(change),
                chrono::Utc::now(),
                1,
            )))
            .unwrap();
    }
    let subscription = unsafe {
        Box::from_raw(wrap_subscription_response(
            drasi_lib::SubscriptionResponse {
                query_id: "query".into(),
                source_id: "source".into(),
                receiver: Box::new(Receiver(events_rx)),
                bootstrap_receiver: bootstrap.then_some(bootstrap_rx),
                position_handle: None,
                bootstrap_result_receiver: None,
            },
            unused_executor,
            runtime.as_ref().unwrap().handle().clone(),
        ))
    };
    let receiver = unsafe { Box::from_raw(subscription.receiver) };
    let bootstrap_receiver = if bootstrap {
        Some(unsafe { Box::from_raw(subscription.bootstrap_receiver) })
    } else {
        None
    };
    unsafe {
        subscription.query_id.into_string();
        subscription.source_id.into_string();
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let context = Arc::new(CallbackContext {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        finished: finished_tx,
        events: AtomicUsize::new(0),
        sentinels: AtomicUsize::new(0),
        decoded: AtomicBool::new(false),
    });
    let ctx = Arc::as_ptr(&context) as *mut c_void;
    if let Some(receiver) = &bootstrap_receiver {
        (receiver.start_push_fn)(receiver.state, bootstrap_callback, ctx);
    } else {
        (receiver.start_push_fn)(receiver.state, source_callback, ctx);
    }
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let (progress_tx, progress_rx) = mpsc::channel();
    runtime.as_ref().unwrap().spawn(async move {
        progress_tx.send(()).unwrap();
    });
    let progress = progress_rx.recv_timeout(Duration::from_secs(1));

    // Release the blocked callback even when the regression assertion will fail.
    (receiver.drop_fn)(receiver.state);
    if let Some(receiver) = &bootstrap_receiver {
        (receiver.drop_fn)(receiver.state);
    }
    if shutdown_runtime {
        runtime.take().unwrap().shutdown_background();
    }
    let early_sentinel = finished_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    release_tx.send(()).unwrap();
    if !early_sentinel {
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    if let Some(runtime) = runtime {
        runtime.shutdown_timeout(Duration::from_secs(2));
    }

    assert!(
        !early_sentinel,
        "context released while callback was in flight"
    );
    assert_eq!(context.events.load(Ordering::SeqCst), 1);
    assert_eq!(context.sentinels.load(Ordering::SeqCst), 1);
    assert!(context.decoded.load(Ordering::SeqCst));
    assert!(
        progress.is_ok(),
        "a backpressured FFI push callback must not block the plugin runtime"
    );
}

#[test]
fn source_push_backpressure_does_not_starve_runtime() {
    assert_push_backpressure_does_not_starve_runtime(false, false);
}

#[test]
fn bootstrap_push_backpressure_does_not_starve_runtime() {
    assert_push_backpressure_does_not_starve_runtime(true, false);
}

#[test]
fn source_push_shutdown_waits_for_in_flight_callback() {
    assert_push_backpressure_does_not_starve_runtime(false, true);
}

#[test]
fn bootstrap_push_shutdown_waits_for_in_flight_callback() {
    assert_push_backpressure_does_not_starve_runtime(true, true);
}
