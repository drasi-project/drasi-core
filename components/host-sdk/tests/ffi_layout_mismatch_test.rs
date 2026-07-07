// Copyright 2025 The Drasi Authors.
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

//! T3 — cross-cdylib **layout-mismatch** regression for issue #602.
//!
//! Issue #602 was a heap corruption (`free(): invalid pointer`) caused by the
//! host taking ownership of a plugin-allocated `repr(Rust)` `SourceEventWrapper`
//! (which embeds `bytes::Bytes` / `Arc<str>`) via `Box::from_raw`. That is
//! unconditionally undefined behaviour, for two independent reasons: `repr(Rust)`
//! has no stable layout across independently compiled cdylibs, *and* `bytes::Bytes`
//! carries a `&'static Vtable` pointer that is only valid in the producing
//! module's address space — so even byte-identical layouts would still free a
//! foreign interior pointer. The fix transfers events as serialized MessagePack
//! bytes instead.
//!
//! This test reproduces the bug **deterministically** by forcing a layout
//! disagreement: the mock-source cdylib is built with one `-Zlayout-seed` and the
//! test host with another, then run under glibc heap hardening (`MALLOC_CHECK_`).
//!
//! - **Before the fix:** the host reads `SourceEventWrapper`/`Element` fields at
//!   the wrong offsets → frees a garbage interior pointer → SIGABRT/SIGSEGV.
//! - **After the fix:** the serialized transfer is layout-independent → clean.
//!
//! An in-workspace cdylib test with **matching** layout/toolchain does NOT
//! reproduce the bug (confirmed empirically), so the forced seed mismatch is
//! required. Because that needs the nightly `-Zrandomize-layout`/`-Zlayout-seed`
//! flags, this test is `#[ignore]`d by default and is driven by the Makefile
//! target:
//!
//! ```text
//! make test-ffi-layout-mismatch
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use drasi_lib::sources::Source;
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;

fn plugin_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root from CARGO_MANIFEST_DIR");
    workspace_root.join("target").join("debug").join("plugins")
}

fn plugin_filename(crate_name: &str) -> String {
    let lib_name = crate_name.replace('-', "_");
    if cfg!(target_os = "macos") {
        format!("lib{lib_name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{lib_name}.dll")
    } else {
        format!("lib{lib_name}.so")
    }
}

/// Drive a stream of source change events from the mock-source cdylib across the
/// FFI boundary and assert the process survives and delivers value-correct,
/// host-owned events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gated: run via `make test-ffi-layout-mismatch` (nightly -Zlayout-seed + MALLOC_CHECK_)"]
async fn mock_source_change_stream_survives_layout_mismatch() {
    let path = plugin_dir().join(plugin_filename("drasi-source-mock"));
    assert!(
        path.exists(),
        "mock source cdylib not found at {} — build it first (see make test-ffi-layout-mismatch)",
        path.display()
    );

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load mock source plugin");

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 5
    });
    let source = plugin.source_plugins[0]
        .create_source("layout-mismatch-src", &config, false)
        .await
        .expect("Should create mock source instance");

    let (event_tx, _event_rx) =
        tokio::sync::mpsc::channel::<drasi_lib::component_graph::ComponentUpdate>(100);
    let context = drasi_lib::context::SourceRuntimeContext::new(
        "layout-mismatch-instance",
        "layout-mismatch-src",
        None,
        event_tx,
        None,
    );
    source.initialize(context).await;
    source.start().await.expect("Should start source");

    let settings = drasi_lib::config::SourceSubscriptionSettings {
        source_id: "layout-mismatch-src".to_string(),
        enable_bootstrap: false,
        query_id: "layout-mismatch-query".to_string(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from: None,
        request_position_handle: false,
    };
    let sub = source.subscribe(settings).await.expect("Should subscribe");
    let mut receiver = sub.receiver;

    // Drain a batch of change events. Each event carries an `Element` with
    // `Arc<str>` data; before the fix, receiving (and dropping) it under a layout
    // mismatch corrupts the heap. After the fix it is deserialized into a
    // host-owned value and is safe to read and drop.
    let mut received = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while received < 50 && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), receiver.recv()).await {
            Ok(Ok(wrapper)) => {
                // Touch the event's heap data — reads through `Arc<str>` and any
                // `bytes::Bytes`. This is what corrupts/aborts before the fix.
                let _ = wrapper.source_id.len();
                if let drasi_lib::channels::events::SourceEvent::Change(change) = &wrapper.event {
                    let _ = format!("{:?}", change.get_reference());
                }
                received += 1;
                // Drop the host-owned wrapper explicitly to exercise the drop glue.
                drop(wrapper);
            }
            Ok(Err(_)) => break, // channel closed
            Err(_) => continue,  // timeout tick
        }
    }

    assert!(
        received >= 1,
        "expected to receive at least one change event from the mock source"
    );
    println!("LAYOUT_MISMATCH_OK: received {received} change events without corruption");

    source.stop().await.expect("Should stop source");
    drop(receiver);
    drop(source);
    drop(plugin);
    drop(_event_rx);
}

/// Drive the **bootstrap** event path from the mock-source cdylib across the FFI
/// boundary under the same layout mismatch. Bootstrap events embed the same
/// `repr(Rust)` `Element`/`Arc<str>` payloads as change events and follow the
/// same serialize/`drop_fn` contract (issue #602), so they need their own
/// regression. Before the fix, receiving and dropping a bootstrap event under a
/// layout mismatch corrupts the heap; after the fix the serialized transfer is
/// layout-independent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "gated: run via `make test-ffi-layout-mismatch` (nightly -Zlayout-seed + MALLOC_CHECK_)"]
async fn mock_source_bootstrap_stream_survives_layout_mismatch() {
    let path = plugin_dir().join(plugin_filename("drasi-source-mock"));
    assert!(
        path.exists(),
        "mock source cdylib not found at {} — build it first (see make test-ffi-layout-mismatch)",
        path.display()
    );

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load mock source plugin");

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 5
    });
    let source = plugin.source_plugins[0]
        .create_source("layout-mismatch-boot-src", &config, false)
        .await
        .expect("Should create mock source instance");

    let (event_tx, _event_rx) =
        tokio::sync::mpsc::channel::<drasi_lib::component_graph::ComponentUpdate>(100);
    let context = drasi_lib::context::SourceRuntimeContext::new(
        "layout-mismatch-boot-instance",
        "layout-mismatch-boot-src",
        None,
        event_tx,
        None,
    );
    source.initialize(context).await;
    source.start().await.expect("Should start source");

    // enable_bootstrap: true drives the FfiBootstrapEvent deserialization path.
    let settings = drasi_lib::config::SourceSubscriptionSettings {
        source_id: "layout-mismatch-boot-src".to_string(),
        enable_bootstrap: true,
        query_id: "layout-mismatch-boot-query".to_string(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from: None,
        request_position_handle: false,
    };
    let sub = source.subscribe(settings).await.expect("Should subscribe");
    let mut bootstrap_receiver = sub
        .bootstrap_receiver
        .expect("mock source should provide a bootstrap receiver when enable_bootstrap is true");

    // Drain bootstrap events. Each carries an `Element` with `Arc<str>` data;
    // touching and dropping it under a layout mismatch corrupts the heap before
    // the fix.
    let mut received = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), bootstrap_receiver.recv()).await {
            Ok(Some(event)) => {
                // Touch the event's heap data — reads through `Arc<str>`. This is
                // what corrupts/aborts before the fix.
                let _ = event.source_id.len();
                let _ = format!("{:?}", event.change.get_reference());
                received += 1;
                // Drop the host-owned event explicitly to exercise the drop glue.
                drop(event);
            }
            Ok(None) => break,  // bootstrap channel closed (stream complete)
            Err(_) => continue, // timeout tick
        }
    }

    assert!(
        received >= 1,
        "expected to receive at least one bootstrap event from the mock source"
    );
    println!("LAYOUT_MISMATCH_OK: received {received} bootstrap events without corruption");

    source.stop().await.expect("Should stop source");
    drop(bootstrap_receiver);
    drop(source);
    drop(plugin);
    drop(_event_rx);
}
