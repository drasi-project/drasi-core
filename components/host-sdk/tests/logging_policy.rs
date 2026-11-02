// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_host_sdk::callbacks::{instance_log_callback, InstanceCallbackContext};
use drasi_lib::observability::{ComponentLogKey, ComponentLogRuntime, ComponentLogType, LogPolicy};
use drasi_plugin_sdk::ffi::{FfiLogEntry, FfiLogLevel, FfiStr};
use std::{ffi::c_void, sync::Arc, time::Duration};
use tracing_subscriber::{filter::filter_fn, prelude::*};

#[tokio::test]
async fn callbacks_use_host_keys_log_only_policy_and_the_actual_destination() {
    let off = ComponentLogRuntime::new(LogPolicy::new("off").unwrap()).unwrap();
    let info = ComponentLogRuntime::new(LogPolicy::new("info,blocked=off").unwrap()).unwrap();
    let (updates, _receiver) = tokio::sync::mpsc::channel(8);
    let context = |runtime: &ComponentLogRuntime| {
        Arc::new(InstanceCallbackContext {
            instance_id: "host-instance".into(),
            component_id: "host-sink".into(),
            component_type: ComponentLogType::Sink,
            runtime_handle: tokio::runtime::Handle::current(),
            log_registry: runtime.registry(),
            update_tx: updates.clone().into(),
        })
    };
    let off_context = context(&off);
    let info_context = context(&info);
    let emit = |context: &Arc<InstanceCallbackContext>, level, target: &str| {
        let entry = FfiLogEntry {
            level,
            plugin_id: FfiStr::from_str("test-plugin"),
            target: FfiStr::from_str(target),
            message: FfiStr::from_str("plugin message"),
            timestamp_us: 0,
            instance_id: FfiStr::from_str("spoof-instance"),
            component_id: FfiStr::from_str("spoof-source"),
        };
        instance_log_callback(Arc::as_ptr(context).cast_mut().cast::<c_void>(), &entry);
    };
    let subscriber = tracing_subscriber::registry()
        .with(off.layer())
        .with(info.layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::sink)
                .with_filter(filter_fn(|metadata| metadata.is_span())),
        );
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::debug_span!("query.evaluate");
        let _entered = span.enter();
        emit(&off_context, FfiLogLevel::Info, "allowed");
        emit(&info_context, FfiLogLevel::Info, "allowed");
        emit(&info_context, FfiLogLevel::Debug, "allowed");
        emit(&info_context, FfiLogLevel::Info, "blocked");
    });
    off.flush().await.unwrap();
    info.flush().await.unwrap();
    let key = ComponentLogKey::new("host-instance", ComponentLogType::Sink, "host-sink");
    assert_eq!(off.registry().log_count_by_key(&key).await, 0);
    let history = info.registry().get_history_by_key(&key).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].component_type, ComponentLogType::Sink);
    let spoof = ComponentLogKey::new("spoof-instance", ComponentLogType::Source, "spoof-source");
    assert_eq!(info.registry().log_count_by_key(&spoof).await, 0);
    off.shutdown(Duration::from_secs(2)).await.unwrap();
    info.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn unsupported_legacy_span_filters_are_explicit() {
    let logging = ComponentLogRuntime::new(LogPolicy::new("[scope]=info").unwrap()).unwrap();
    let result = logging.registry().legacy_log_level();
    logging.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(result.is_err());
}
