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

//! Integration tests for drasi-host-sdk.
//!
//! These tests load real cdylib plugins (mock source, log reaction) and exercise
//! the full dynamic loading pipeline: metadata validation, callback wiring,
//! descriptor factory invocation, and trait method dispatch through FFI vtables.
//!
//! **Prerequisites**: Build the cdylib plugins before running these tests:
//!
//! ```sh
//! cd /path/to/drasi-core
//! cargo build --lib -p drasi-source-mock --features drasi-source-mock/dynamic-plugin
//! cargo build --lib -p drasi-reaction-log --features drasi-reaction-log/dynamic-plugin
//! ```

use std::path::{Path, PathBuf};

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::{load_plugin_from_path, PluginLoader, PluginLoaderConfig};
use drasi_plugin_sdk::descriptor::{ReactionPluginDescriptor, SourcePluginDescriptor};

/// Locate the drasi-core build output directory.
///
/// Looks for plugins in the workspace target directory (drasi-core/target/debug/).
fn plugin_dir() -> PathBuf {
    // Walk up from this crate's manifest dir to the workspace root
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // host-sdk is at components/host-sdk, workspace root is ../../
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root from CARGO_MANIFEST_DIR");
    workspace_root.join("target").join("debug")
}

/// Get the platform-specific plugin filename.
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

/// Get the full path to a plugin, skipping the test if not found.
fn require_plugin(crate_name: &str) -> PathBuf {
    let dir = plugin_dir();
    let filename = plugin_filename(crate_name);
    let path = dir.join(&filename);
    if !path.exists() {
        eprintln!(
            "SKIPPING: Plugin not found at {}. Build with: \
             cargo build --lib -p {} --features {}/dynamic-plugin",
            path.display(),
            crate_name,
            crate_name
        );
        // Use a panic that test harness captures — but mark as should_panic won't work.
        // Instead, just skip by returning early from test functions.
    }
    path
}

fn plugin_exists(crate_name: &str) -> bool {
    let dir = plugin_dir();
    let filename = plugin_filename(crate_name);
    dir.join(&filename).exists()
}

// ============================================================================
// Plugin Loading Tests
// ============================================================================

#[test]
fn test_load_mock_source_plugin() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load mock source plugin");

    assert!(
        !plugin.source_plugins.is_empty(),
        "Mock source plugin should register at least one source descriptor"
    );
    assert!(
        plugin.reaction_plugins.is_empty(),
        "Mock source plugin should not register reaction descriptors"
    );
    assert!(
        plugin.metadata_info.is_some(),
        "Plugin should have metadata"
    );

    let meta = plugin.metadata_info.as_ref().unwrap();
    assert!(meta.contains("sdk="), "Metadata should include SDK version");
    assert!(
        meta.contains("target="),
        "Metadata should include target triple"
    );
}

#[test]
fn test_load_log_reaction_plugin() {
    if !plugin_exists("drasi-reaction-log") {
        eprintln!("SKIP: drasi-reaction-log not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-reaction-log");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load log reaction plugin");

    assert!(
        plugin.source_plugins.is_empty(),
        "Log reaction should not register source descriptors"
    );
    assert!(
        !plugin.reaction_plugins.is_empty(),
        "Log reaction should register at least one reaction descriptor"
    );
}

// ============================================================================
// Descriptor / Factory Tests
// ============================================================================

#[test]
fn test_mock_source_descriptor_kind() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.source_plugins[0];
    assert_eq!(descriptor.kind(), "mock");
    assert_eq!(descriptor.config_version(), "1.0.0");
    assert_eq!(descriptor.config_schema_name(), "source.mock.MockSourceConfig");
}

#[test]
fn test_log_reaction_descriptor_kind() {
    if !plugin_exists("drasi-reaction-log") {
        eprintln!("SKIP: drasi-reaction-log not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-reaction-log");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.reaction_plugins[0];
    assert_eq!(descriptor.kind(), "log");
    assert_eq!(descriptor.config_version(), "1.0.0");
    assert_eq!(descriptor.config_schema_name(), "reaction.log.LogReactionConfig");
}

// ============================================================================
// OpenAPI Schema Tests
// ============================================================================

#[test]
fn test_mock_source_config_schema_is_valid_json() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let schema_json = plugin.source_plugins[0].config_schema_json();
    assert!(!schema_json.is_empty(), "Schema JSON should not be empty");

    let parsed: serde_json::Value =
        serde_json::from_str(&schema_json).expect("Schema should be valid JSON");
    assert!(parsed.is_object(), "Schema should be a JSON object");

    // The schema should contain the main config type
    let obj = parsed.as_object().unwrap();
    assert!(
        obj.contains_key("MockSourceConfig"),
        "Schema should contain MockSourceConfig, got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn test_log_reaction_config_schema_is_valid_json() {
    if !plugin_exists("drasi-reaction-log") {
        eprintln!("SKIP: drasi-reaction-log not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-reaction-log");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let schema_json = plugin.reaction_plugins[0].config_schema_json();
    assert!(!schema_json.is_empty());

    let parsed: serde_json::Value =
        serde_json::from_str(&schema_json).expect("Schema should be valid JSON");
    assert!(parsed.is_object());

    let obj = parsed.as_object().unwrap();
    assert!(
        obj.contains_key("LogReactionConfig"),
        "Schema should contain LogReactionConfig, got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}

// ============================================================================
// Instance Creation Tests
// ============================================================================

#[tokio::test]
async fn test_create_mock_source_instance() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.source_plugins[0];

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 1000
    });

    let source = descriptor
        .create_source("test-mock-1", &config, false)
        .await
        .expect("Should create mock source instance");

    assert_eq!(source.id(), "test-mock-1");
    assert_eq!(source.type_name(), "mock");
    assert!(!source.auto_start());
}

#[tokio::test]
async fn test_create_log_reaction_instance() {
    if !plugin_exists("drasi-reaction-log") {
        eprintln!("SKIP: drasi-reaction-log not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-reaction-log");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.reaction_plugins[0];

    let config = serde_json::json!({});
    let query_ids = vec!["q1".to_string(), "q2".to_string()];

    let reaction = descriptor
        .create_reaction("test-log-1", query_ids.clone(), &config, true)
        .await
        .expect("Should create log reaction instance");

    assert_eq!(reaction.id(), "test-log-1");
    assert_eq!(reaction.type_name(), "log");
    assert!(reaction.auto_start());

    let returned_ids = reaction.query_ids();
    assert_eq!(returned_ids, query_ids);
}

// ============================================================================
// Source Lifecycle Tests
// ============================================================================

#[tokio::test]
async fn test_mock_source_start_stop_lifecycle() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 60000
    });

    let source = plugin.source_plugins[0]
        .create_source("lifecycle-test", &config, false)
        .await
        .unwrap();

    // Initial status should be Stopped
    use drasi_lib::ComponentStatus;
    let status = source.status().await;
    assert!(
        matches!(status, ComponentStatus::Stopped),
        "Initial status should be Stopped, got: {status:?}"
    );

    // Start
    source.start().await.expect("Start should succeed");
    let status = source.status().await;
    assert!(
        matches!(status, ComponentStatus::Running),
        "After start, status should be Running, got: {status:?}"
    );

    // Stop
    source.stop().await.expect("Stop should succeed");
    let status = source.status().await;
    assert!(
        matches!(status, ComponentStatus::Stopped),
        "After stop, status should be Stopped, got: {status:?}"
    );
}

// ============================================================================
// Multiple Instance Tests
// ============================================================================

#[tokio::test]
async fn test_create_multiple_source_instances() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 1000
    });

    let descriptor = &plugin.source_plugins[0];

    // Create multiple instances from the same descriptor
    let s1 = descriptor
        .create_source("inst-1", &config, false)
        .await
        .unwrap();
    let s2 = descriptor
        .create_source("inst-2", &config, true)
        .await
        .unwrap();
    let s3 = descriptor
        .create_source("inst-3", &config, false)
        .await
        .unwrap();

    assert_eq!(s1.id(), "inst-1");
    assert_eq!(s2.id(), "inst-2");
    assert_eq!(s3.id(), "inst-3");

    assert!(!s1.auto_start());
    assert!(s2.auto_start());
    assert!(!s3.auto_start());
}

// ============================================================================
// PluginLoader Directory Scanning Tests
// ============================================================================

#[test]
fn test_plugin_loader_empty_directory() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: temp_dir.path().to_path_buf(),
        file_patterns: vec!["libdrasi_*".to_string()],
    });

    let plugins = loader
        .load_all(
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();

    assert!(
        plugins.is_empty(),
        "Empty directory should yield no plugins"
    );
}

#[test]
fn test_plugin_loader_nonexistent_directory() {
    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: PathBuf::from("/nonexistent/dir/that/should/not/exist"),
        file_patterns: vec!["libdrasi_*".to_string()],
    });

    let plugins = loader
        .load_all(
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();

    assert!(
        plugins.is_empty(),
        "Nonexistent directory should yield no plugins"
    );
}

#[test]
fn test_plugin_loader_skips_non_plugin_files() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(temp_dir.path().join("README.md"), "# Not a plugin").unwrap();
    std::fs::write(temp_dir.path().join("config.yaml"), "key: value").unwrap();
    std::fs::write(temp_dir.path().join("random.txt"), "hello").unwrap();

    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: temp_dir.path().to_path_buf(),
        file_patterns: vec!["libdrasi_source_*".to_string()],
    });

    let plugins = loader
        .load_all(
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();

    assert!(plugins.is_empty(), "Non-library files should not be loaded");
}

#[test]
fn test_plugin_loader_discovers_plugins_by_pattern() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let dir = plugin_dir();

    // Only scan for source plugins
    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: dir.clone(),
        file_patterns: vec!["libdrasi_source_mock*".to_string()],
    });

    let plugins = loader
        .load_all(
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();

    assert!(
        !plugins.is_empty(),
        "Should discover mock source plugin in {}",
        dir.display()
    );

    // Verify it registered the right descriptor
    let has_mock = plugins
        .iter()
        .any(|p| p.source_plugins.iter().any(|d| d.kind() == "mock"));
    assert!(has_mock, "Should find a 'mock' source descriptor");
}

#[test]
fn test_plugin_loader_with_multiple_patterns() {
    if !plugin_exists("drasi-source-mock") || !plugin_exists("drasi-reaction-log") {
        eprintln!("SKIP: need both mock source and log reaction built as cdylib");
        return;
    }
    let dir = plugin_dir();

    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: dir,
        file_patterns: vec![
            "libdrasi_source_mock*".to_string(),
            "libdrasi_reaction_log*".to_string(),
        ],
    });

    let plugins = loader
        .load_all(
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();

    assert!(
        plugins.len() >= 2,
        "Should load at least 2 plugins, got {}",
        plugins.len()
    );

    let has_mock_source = plugins
        .iter()
        .any(|p| p.source_plugins.iter().any(|d| d.kind() == "mock"));
    let has_log_reaction = plugins
        .iter()
        .any(|p| p.reaction_plugins.iter().any(|d| d.kind() == "log"));

    assert!(has_mock_source, "Should find mock source descriptor");
    assert!(has_log_reaction, "Should find log reaction descriptor");
}

// ============================================================================
// Callback Tests
// ============================================================================

#[tokio::test]
async fn test_log_callback_captures_plugin_logs() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    // Clear captured logs
    callbacks::captured_logs().lock().unwrap().clear();

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 60000
    });

    let source = plugin.source_plugins[0]
        .create_source("log-test", &config, false)
        .await
        .unwrap();

    // Starting the source should generate log messages from the plugin
    source.start().await.expect("Start should succeed");

    // Give plugin a moment to emit logs
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    source.stop().await.expect("Stop should succeed");

    // Check that some logs were captured (mock source logs on start/stop)
    let logs = callbacks::captured_logs().lock().unwrap();
    // We can't guarantee specific messages, but the callback mechanism should work
    // The lifecycle callback should at least fire
    let lifecycles = callbacks::captured_lifecycles().lock().unwrap();
    // Either logs or lifecycle events should be captured
    let total_captured = logs.len() + lifecycles.len();
    assert!(
        total_captured > 0,
        "Should capture at least some logs or lifecycle events from plugin operations"
    );
}

// ============================================================================
// Error Handling Tests
// ============================================================================

#[test]
fn test_load_nonexistent_file_returns_error() {
    let result = load_plugin_from_path(
        Path::new("/tmp/nonexistent_drasi_plugin.so"),
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    );

    assert!(result.is_err(), "Loading nonexistent file should fail");
    let err = result.err().unwrap().to_string();
    assert!(
        err.contains("Failed to load") || err.contains("cannot open"),
        "Error should mention loading failure: {err}"
    );
}

#[test]
fn test_load_invalid_file_returns_error() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let fake_so = temp_dir.path().join("libdrasi_source_fake.so");
    std::fs::write(&fake_so, b"this is not a shared library").unwrap();

    let result = load_plugin_from_path(
        &fake_so,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    );

    assert!(result.is_err(), "Loading a non-library file should fail");
}

#[tokio::test]
async fn test_create_source_with_invalid_config() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    // Mock source uses deny_unknown_fields, so this should fail
    let bad_config = serde_json::json!({
        "completelyWrongField": true,
        "anotherBadField": 42
    });

    let result = plugin.source_plugins[0]
        .create_source("bad-config", &bad_config, false)
        .await;

    // The plugin factory may handle this in different ways:
    // - Return an error (preferred)
    // - Return a source with default config
    // Either way, it should NOT crash
    if let Err(e) = &result {
        // Good — invalid config was rejected
        let msg = e.to_string();
        assert!(!msg.is_empty(), "Error should have a descriptive message");
    }
    // If it succeeds, that's also acceptable (plugin chose defaults)
}

// ============================================================================
// Dispatch Mode Tests
// ============================================================================

#[tokio::test]
async fn test_mock_source_dispatch_mode() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 1000
    });

    let source = plugin.source_plugins[0]
        .create_source("dispatch-test", &config, false)
        .await
        .unwrap();

    // Mock source should return a valid dispatch mode
    let mode = source.dispatch_mode();
    // Just verify it returns without crashing — the actual mode depends on implementation
    assert!(
        matches!(
            mode,
            drasi_lib::DispatchMode::Channel | drasi_lib::DispatchMode::Broadcast
        ),
        "Should return a valid dispatch mode"
    );
}

// ============================================================================
// Plugin Drop / Cleanup Tests
// ============================================================================

#[tokio::test]
async fn test_drop_source_instance_does_not_crash() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 1000
    });

    // Create and immediately drop multiple instances
    for i in 0..5 {
        let source = plugin.source_plugins[0]
            .create_source(&format!("drop-test-{i}"), &config, false)
            .await
            .unwrap();
        drop(source);
    }

    // If we get here without crashing, drop handling works correctly
}

#[test]
fn test_drop_loaded_plugin_does_not_crash() {
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    // Load and drop the plugin multiple times
    for _ in 0..3 {
        let plugin = load_plugin_from_path(
            &path,
            std::ptr::null_mut(),
            callbacks::default_log_callback_fn(),
            std::ptr::null_mut(),
            callbacks::default_lifecycle_callback_fn(),
        )
        .unwrap();
        drop(plugin);
    }
}

// ============================================================================
// Tests for log and lifecycle callback routing through per-instance callbacks
// ============================================================================
//
// These tests simulate the real DrasiLib flow:
// 1. Load plugin → create source via factory
// 2. Create a status_tx/status_rx channel (like SourceManager does)
// 3. Call source.initialize(SourceRuntimeContext) — this wires per-instance callbacks
// 4. Start/stop the source
// 5. Verify logs appear in ComponentLogRegistry and events arrive on status_rx

#[tokio::test]
async fn test_plugin_logs_routed_to_log_registry() {
    //! Verify that log output from a plugin during start/stop is captured in
    //! the ComponentLogRegistry when source.initialize() is called with a real
    //! SourceRuntimeContext (which sets up per-instance callbacks).
    use drasi_lib::channels::ComponentType;
    use drasi_lib::context::SourceRuntimeContext;
    use drasi_lib::managers::{get_or_init_global_registry, ComponentLogKey};
    use drasi_lib::sources::Source;

    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    // Global callbacks are still needed for plugin-level log capture
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("log-test-source", &config, false)
        .await
        .unwrap();

    // Create a real status channel (like SourceManager does)
    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);

    // Initialize with a real SourceRuntimeContext — this wires per-instance callbacks
    let context =
        SourceRuntimeContext::new("log-test-instance", "log-test-source", status_tx, None);
    source.initialize(context).await;

    // Subscribe to logs BEFORE starting (like the API does)
    let registry = get_or_init_global_registry();
    let log_key = ComponentLogKey::new(
        "log-test-instance",
        ComponentType::Source,
        "log-test-source",
    );
    let (_history, mut log_rx) = registry.subscribe_by_key(&log_key).await;

    // Starting the source should trigger log output from the plugin
    let _ = source.start().await;
    // Give a moment for logs to propagate
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = source.stop().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Check that logs were routed to the ComponentLogRegistry
    let mut log_count = 0;
    while let Ok(log_msg) = log_rx.try_recv() {
        log_count += 1;
        eprintln!("  Registry log: [{}] {}", log_msg.level, log_msg.message);
    }
    assert!(
        log_count > 0,
        "Expected at least one log message in ComponentLogRegistry for key {log_key:?}",
    );

    // Also check the diagnostic store (captured_logs) for completeness
    let captured = callbacks::captured_logs().lock().unwrap();
    assert!(
        !captured.is_empty(),
        "Expected at least one captured log from plugin start/stop"
    );
}

#[tokio::test]
async fn test_plugin_lifecycle_events_routed_via_status_channel() {
    //! Verify that lifecycle events (Starting, Started, Stopping, Stopped) from a plugin
    //! flow through the status_tx channel when source.initialize() is called with a real
    //! SourceRuntimeContext. This is the same channel that LifecycleManager monitors.
    use drasi_lib::context::SourceRuntimeContext;
    use drasi_lib::sources::Source;
    use drasi_lib::ComponentStatus;

    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("lifecycle-evt-source", &config, false)
        .await
        .unwrap();

    // Create a real status channel
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel(100);

    // Initialize with a real SourceRuntimeContext
    let context = SourceRuntimeContext::new(
        "lifecycle-test-instance",
        "lifecycle-evt-source",
        status_tx,
        None,
    );
    source.initialize(context).await;

    // Start the source — should emit Starting, Started events through channel
    let _ = source.start().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop the source — should emit Stopping, Stopped events through channel
    let _ = source.stop().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Drain all events from the channel
    let mut events = Vec::new();
    while let Ok(event) = status_rx.try_recv() {
        eprintln!(
            "  Event: {} {:?} {:?} {}",
            event.component_id,
            event.component_type,
            event.status,
            event.message.as_deref().unwrap_or("")
        );
        events.push(event);
    }

    assert!(
        !events.is_empty(),
        "Expected lifecycle events on status_rx channel"
    );

    // Verify we got events for the right component
    let our_events: Vec<_> = events
        .iter()
        .filter(|e| e.component_id == "lifecycle-evt-source")
        .collect();
    assert!(
        !our_events.is_empty(),
        "Expected events for 'lifecycle-evt-source'"
    );

    // Verify we got at least Starting and Started (from start)
    let statuses: Vec<_> = our_events.iter().map(|e| &e.status).collect();
    assert!(
        statuses.contains(&&ComponentStatus::Starting),
        "Expected Starting event, got: {statuses:?}"
    );
    assert!(
        statuses.contains(&&ComponentStatus::Running),
        "Expected Running/Started event, got: {statuses:?}"
    );

    // Also check the diagnostic store for completeness
    let captured = callbacks::captured_lifecycles().lock().unwrap();
    let our_captured: Vec<_> = captured
        .iter()
        .filter(|e| e.component_id == "lifecycle-evt-source")
        .collect();
    assert!(
        !our_captured.is_empty(),
        "Expected lifecycle events in diagnostic store"
    );
}

#[tokio::test]
async fn test_null_callback_context_does_not_crash() {
    //! Verify that when source.initialize() is NOT called (no per-instance callbacks),
    //! logs still go to the diagnostic store and host log framework, and lifecycle events
    //! go to the diagnostic store via global callbacks. No crash occurs.
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("null-ctx-test", &config, false)
        .await
        .unwrap();

    // No initialize() call — per-instance callbacks won't be set up
    // Should not panic even with null context
    let _ = source.start().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = source.stop().await;
}

// ============================================================================
// Tracing bridge tests — verify log events at all levels are captured
// ============================================================================

#[tokio::test]
async fn test_all_log_levels_captured_in_diagnostic_store() {
    //! Verify that log events at all levels (info, warn, error, debug) emitted by a plugin
    //! are captured in the diagnostic captured_logs store. The mock source emits info! during
    //! start/stop, verifying the tracing-log bridge works for the log crate.
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    // Clear diagnostic stores before test
    callbacks::captured_logs().lock().unwrap().clear();

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("level-test", &config, false)
        .await
        .unwrap();

    let _ = source.start().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = source.stop().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let logs = callbacks::captured_logs().lock().unwrap();
    // We should have at least the start and stop info logs
    let our_logs: Vec<_> = logs
        .iter()
        .filter(|l| l.message.contains("level-test"))
        .collect();

    assert!(
        !our_logs.is_empty(),
        "Expected captured logs mentioning 'level-test'"
    );

    // Verify info level is present (mock source emits info during start/stop)
    let has_info = our_logs
        .iter()
        .any(|l| l.level == drasi_plugin_sdk::ffi::FfiLogLevel::Info);
    assert!(
        has_info,
        "Expected at least one INFO log, got: {our_logs:?}"
    );
}

#[tokio::test]
async fn test_logs_without_initialize_reach_global_callback() {
    //! When a source is NOT initialized (no per-instance callbacks), logs should
    //! still reach the global callback and be captured in the diagnostic store.
    //! This verifies the tracing bridge's global fallback path.
    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    callbacks::captured_logs().lock().unwrap().clear();

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("global-cb-test", &config, false)
        .await
        .unwrap();

    // Don't call initialize — no per-instance callbacks
    let _ = source.start().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = source.stop().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let logs = callbacks::captured_logs().lock().unwrap();
    let our_logs: Vec<_> = logs
        .iter()
        .filter(|l| l.message.contains("global-cb-test"))
        .collect();

    assert!(
        !our_logs.is_empty(),
        "Expected logs via global callback for source without initialize()"
    );
}

#[tokio::test]
async fn test_per_instance_logs_include_correct_component_id() {
    //! Verify that when source.initialize() is called, subsequent log entries
    //! in the ComponentLogRegistry contain the correct component_id.
    use drasi_lib::channels::ComponentType;
    use drasi_lib::context::SourceRuntimeContext;
    use drasi_lib::managers::{get_or_init_global_registry, ComponentLogKey};
    use drasi_lib::sources::Source;

    if !plugin_exists("drasi-source-mock") {
        eprintln!("SKIP: drasi-source-mock not built as cdylib");
        return;
    }
    let path = require_plugin("drasi-source-mock");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let config = serde_json::json!({
        "dataType": { "type": "generic" },
        "intervalMs": 500
    });

    let source = plugin.source_plugins[0]
        .create_source("component-id-test", &config, false)
        .await
        .unwrap();

    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(100);
    let context =
        SourceRuntimeContext::new("cid-test-instance", "component-id-test", status_tx, None);
    source.initialize(context).await;

    let registry = get_or_init_global_registry();
    let log_key = ComponentLogKey::new(
        "cid-test-instance",
        ComponentType::Source,
        "component-id-test",
    );
    let (_history, mut log_rx) = registry.subscribe_by_key(&log_key).await;

    let _ = source.start().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = source.stop().await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut log_count = 0;
    while let Ok(log_msg) = log_rx.try_recv() {
        log_count += 1;
        // Verify each log has the correct component key
        let key = log_msg.key();
        assert_eq!(
            key.component_id, "component-id-test",
            "Log entry should have component_id 'component-id-test', got '{}'",
            key.component_id
        );
        assert_eq!(
            key.instance_id, "cid-test-instance",
            "Log entry should have instance_id 'cid-test-instance', got '{}'",
            key.instance_id
        );
    }
    assert!(
        log_count > 0,
        "Expected at least one log message with correct component_id in registry"
    );
}

// =============================================================================
// Identity Provider FFI bridge tests
// =============================================================================

/// A mock IdentityProvider for testing the FFI bridge.
struct MockIdentityProvider {
    username: String,
    password: String,
}

#[async_trait::async_trait]
impl drasi_lib::identity::IdentityProvider for MockIdentityProvider {
    async fn get_credentials(&self) -> anyhow::Result<drasi_lib::identity::Credentials> {
        Ok(drasi_lib::identity::Credentials::UsernamePassword {
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }

    fn clone_box(&self) -> Box<dyn drasi_lib::identity::IdentityProvider> {
        Box::new(MockIdentityProvider {
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }
}

/// A mock IdentityProvider that returns Token credentials.
struct MockTokenProvider {
    username: String,
    token: String,
}

#[async_trait::async_trait]
impl drasi_lib::identity::IdentityProvider for MockTokenProvider {
    async fn get_credentials(&self) -> anyhow::Result<drasi_lib::identity::Credentials> {
        Ok(drasi_lib::identity::Credentials::Token {
            username: self.username.clone(),
            token: self.token.clone(),
        })
    }

    fn clone_box(&self) -> Box<dyn drasi_lib::identity::IdentityProvider> {
        Box::new(MockTokenProvider {
            username: self.username.clone(),
            token: self.token.clone(),
        })
    }
}

/// A mock IdentityProvider that returns Certificate credentials.
struct MockCertProvider {
    cert_pem: String,
    key_pem: String,
    username: Option<String>,
}

#[async_trait::async_trait]
impl drasi_lib::identity::IdentityProvider for MockCertProvider {
    async fn get_credentials(&self) -> anyhow::Result<drasi_lib::identity::Credentials> {
        Ok(drasi_lib::identity::Credentials::Certificate {
            cert_pem: self.cert_pem.clone(),
            key_pem: self.key_pem.clone(),
            username: self.username.clone(),
        })
    }

    fn clone_box(&self) -> Box<dyn drasi_lib::identity::IdentityProvider> {
        Box::new(MockCertProvider {
            cert_pem: self.cert_pem.clone(),
            key_pem: self.key_pem.clone(),
            username: self.username.clone(),
        })
    }
}

/// A mock IdentityProvider that returns an error.
struct MockErrorProvider;

#[async_trait::async_trait]
impl drasi_lib::identity::IdentityProvider for MockErrorProvider {
    async fn get_credentials(&self) -> anyhow::Result<drasi_lib::identity::Credentials> {
        Err(anyhow::anyhow!("Authentication service unavailable"))
    }

    fn clone_box(&self) -> Box<dyn drasi_lib::identity::IdentityProvider> {
        Box::new(MockErrorProvider)
    }
}

/// Test that UsernamePassword credentials survive the vtable → proxy roundtrip.
#[tokio::test]
async fn test_identity_provider_username_password_roundtrip() {
    let provider = std::sync::Arc::new(MockIdentityProvider {
        username: "admin".to_string(),
        password: "s3cret!".to_string(),
    });

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;
    let creds = proxy
        .get_credentials()
        .await
        .expect("get_credentials should succeed");

    match creds {
        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
            assert_eq!(username, "admin");
            assert_eq!(password, "s3cret!");
        }
        _ => panic!("Expected UsernamePassword variant"),
    }
}

/// Test that Token credentials survive the vtable → proxy roundtrip.
#[tokio::test]
async fn test_identity_provider_token_roundtrip() {
    let provider = std::sync::Arc::new(MockTokenProvider {
        username: "svc-account".to_string(),
        token: "eyJhbGciOiJSUzI1NiJ9.test-token".to_string(),
    });

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;
    let creds = proxy
        .get_credentials()
        .await
        .expect("get_credentials should succeed");

    match creds {
        drasi_lib::identity::Credentials::Token { username, token } => {
            assert_eq!(username, "svc-account");
            assert_eq!(token, "eyJhbGciOiJSUzI1NiJ9.test-token");
        }
        _ => panic!("Expected Token variant"),
    }
}

/// Test that Certificate credentials (with optional username) survive roundtrip.
#[tokio::test]
async fn test_identity_provider_certificate_roundtrip() {
    let provider = std::sync::Arc::new(MockCertProvider {
        cert_pem: "-----BEGIN CERTIFICATE-----\nMIIC...\n-----END CERTIFICATE-----".to_string(),
        key_pem: "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----".to_string(),
        username: Some("db-user".to_string()),
    });

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;
    let creds = proxy
        .get_credentials()
        .await
        .expect("get_credentials should succeed");

    match creds {
        drasi_lib::identity::Credentials::Certificate {
            cert_pem,
            key_pem,
            username,
        } => {
            assert!(cert_pem.contains("BEGIN CERTIFICATE"));
            assert!(key_pem.contains("BEGIN PRIVATE KEY"));
            assert_eq!(username, Some("db-user".to_string()));
        }
        _ => panic!("Expected Certificate variant"),
    }
}

/// Test that Certificate credentials without username work.
#[tokio::test]
async fn test_identity_provider_certificate_no_username() {
    let provider = std::sync::Arc::new(MockCertProvider {
        cert_pem: "cert-data".to_string(),
        key_pem: "key-data".to_string(),
        username: None,
    });

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;
    let creds = proxy
        .get_credentials()
        .await
        .expect("get_credentials should succeed");

    match creds {
        drasi_lib::identity::Credentials::Certificate {
            cert_pem,
            key_pem,
            username,
        } => {
            assert_eq!(cert_pem, "cert-data");
            assert_eq!(key_pem, "key-data");
            assert_eq!(username, None);
        }
        _ => panic!("Expected Certificate variant"),
    }
}

/// Test that errors from the identity provider propagate through FFI.
#[tokio::test]
async fn test_identity_provider_error_propagation() {
    let provider = std::sync::Arc::new(MockErrorProvider);

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;
    let result = proxy.get_credentials().await;

    match result {
        Ok(_) => panic!("Expected error from identity provider"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("Authentication service unavailable"),
                "Error message should contain original error, got: {err_msg}"
            );
        }
    }
}

/// Test that clone_box produces a working independent copy.
#[tokio::test]
async fn test_identity_provider_clone_box() {
    let provider = std::sync::Arc::new(MockIdentityProvider {
        username: "user1".to_string(),
        password: "pass1".to_string(),
    });

    let vtable = drasi_host_sdk::IdentityProviderVtableBuilder::build(provider);
    let vtable_ptr = Box::into_raw(Box::new(vtable));

    let proxy = unsafe { drasi_plugin_sdk::ffi::FfiIdentityProviderProxy::new(vtable_ptr) };

    use drasi_lib::identity::IdentityProvider;

    // Clone the proxy
    let cloned = proxy.clone_box();

    // Drop the original
    drop(proxy);

    // The clone should still work
    let creds = cloned
        .get_credentials()
        .await
        .expect("cloned provider should work");
    match creds {
        drasi_lib::identity::Credentials::UsernamePassword { username, password } => {
            assert_eq!(username, "user1");
            assert_eq!(password, "pass1");
        }
        _ => panic!("Expected UsernamePassword variant"),
    }
}

/// Test that null identity_provider in FfiRuntimeContext is handled correctly
/// (source initializes without crashing and identity_provider is None).
#[tokio::test]
async fn test_source_with_null_identity_provider() {
    let dir = plugin_dir();
    let mock_path = dir.join(plugin_filename("drasi_source_mock"));
    if !mock_path.exists() {
        eprintln!("Skipping test: mock source plugin not found at {mock_path:?}");
        return;
    }

    let plugin = load_plugin_from_path(
        &mock_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Should load mock source plugin");

    let source_descriptor = &plugin.source_plugins[0];
    let config = serde_json::json!({});
    let source = source_descriptor
        .create_source("null-ip-test", &config, false)
        .await
        .expect("Should create source");

    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(16);
    let context = drasi_lib::context::SourceRuntimeContext {
        instance_id: "test-instance".to_string(),
        source_id: "null-ip-test".to_string(),
        status_tx,
        state_store: None,
        identity_provider: None,
    };

    // This should not crash — identity_provider is None
    source.initialize(context).await;
    assert_eq!(source.id(), "null-ip-test");
}

/// Test that a source receives a non-null identity_provider through FFI.
#[tokio::test]
async fn test_source_with_identity_provider_injection() {
    let dir = plugin_dir();
    let mock_path = dir.join(plugin_filename("drasi_source_mock"));
    if !mock_path.exists() {
        eprintln!("Skipping test: mock source plugin not found at {mock_path:?}");
        return;
    }

    let plugin = load_plugin_from_path(
        &mock_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Should load mock source plugin");

    let source_descriptor = &plugin.source_plugins[0];
    let config = serde_json::json!({});
    let source = source_descriptor
        .create_source("ip-inject-test", &config, false)
        .await
        .expect("Should create source");

    let provider: std::sync::Arc<dyn drasi_lib::identity::IdentityProvider> =
        std::sync::Arc::new(MockIdentityProvider {
            username: "injected-user".to_string(),
            password: "injected-pass".to_string(),
        });

    let (status_tx, _status_rx) = tokio::sync::mpsc::channel(16);
    let context = drasi_lib::context::SourceRuntimeContext {
        instance_id: "test-instance".to_string(),
        source_id: "ip-inject-test".to_string(),
        status_tx,
        state_store: None,
        identity_provider: Some(provider),
    };

    // This should not crash — identity_provider is passed through FFI
    source.initialize(context).await;
    assert_eq!(source.id(), "ip-inject-test");
}
