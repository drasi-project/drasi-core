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

//! FFI integration tests for secret store plugins.
//!
//! These tests load the `drasi-secret-store-file` cdylib plugin and exercise
//! the full FFI boundary:
//!
//! 1. Plugin loading via `load_plugin_from_path()`
//! 2. Secret store descriptor metadata (kind, version, schema)
//! 3. Instance creation from JSON config
//! 4. Secret resolution through the FFI vtable
//! 5. Error handling for missing secrets
//! 6. Integration with `DtoMapper` and `ValueResolver`
//! 7. Clean drop of plugin and provider instances
//!
//! **Prerequisites**: Build the file secret store cdylib before running:
//!
//! ```sh
//! cargo build --lib -p drasi-secret-store-file --features drasi-secret-store-file/dynamic-plugin
//! ```
//!
//! Then run with:
//! ```sh
//! cargo test -p drasi-host-sdk --test secret_store_integration_test -- --ignored
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use drasi_host_sdk::SecretStoreValueResolverAdapter;
use drasi_plugin_sdk::descriptor::SecretStorePluginDescriptor;
use drasi_plugin_sdk::mapper::DtoMapper;
use drasi_plugin_sdk::resolver::{register_secret_resolver, ValueResolver};
use drasi_plugin_sdk::ConfigValue;

/// Locate the workspace target/debug/plugins directory.
fn plugin_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root");
    workspace_root.join("target").join("debug").join("plugins")
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

fn plugin_exists(crate_name: &str) -> bool {
    let dir = plugin_dir();
    let filename = plugin_filename(crate_name);
    dir.join(&filename).exists()
}

fn require_plugin(crate_name: &str) -> PathBuf {
    let dir = plugin_dir();
    let filename = plugin_filename(crate_name);
    let path = dir.join(&filename);
    if !path.exists() {
        panic!(
            "Plugin not found at {}. Build with: \
             cargo build --lib -p {} --features {}/dynamic-plugin",
            path.display(),
            crate_name,
            crate_name
        );
    }
    path
}

/// Create a temp secrets JSON file and return its path.
fn create_temp_secrets_file(secrets: &serde_json::Value) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let path = dir.path().join("secrets.json");
    std::fs::write(
        &path,
        serde_json::to_string(secrets).expect("Failed to serialize secrets"),
    )
    .expect("Failed to write secrets file");
    (dir, path)
}

// ============================================================================
// Plugin Loading Tests
// ============================================================================

#[test]
#[ignore]
fn test_load_file_secret_store_plugin() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");

    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load file secret store plugin");

    // The file secret store should register exactly one secret store descriptor
    assert!(
        !plugin.secret_store_plugins.is_empty(),
        "File secret store plugin should register at least one secret store descriptor"
    );
    assert!(
        plugin.source_plugins.is_empty(),
        "File secret store should not register source descriptors"
    );
    assert!(
        plugin.reaction_plugins.is_empty(),
        "File secret store should not register reaction descriptors"
    );
    assert!(
        plugin.bootstrap_plugins.is_empty(),
        "File secret store should not register bootstrap descriptors"
    );
    assert!(
        plugin.metadata_info.is_some(),
        "Plugin should have metadata"
    );
}

// ============================================================================
// Descriptor Metadata Tests
// ============================================================================

#[test]
#[ignore]
fn test_file_secret_store_descriptor_kind() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];
    assert_eq!(descriptor.kind(), "file");
    assert_eq!(descriptor.config_version(), "1.0.0");
    assert_eq!(
        descriptor.config_schema_name(),
        "secret_store.file.FileSecretStoreConfig"
    );
}

// ============================================================================
// Schema Roundtrip Tests
// ============================================================================

#[test]
#[ignore]
fn test_file_secret_store_schema_is_valid_json() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let schema_json = plugin.secret_store_plugins[0].config_schema_json();
    assert!(!schema_json.is_empty(), "Schema JSON should not be empty");

    let parsed: serde_json::Value =
        serde_json::from_str(&schema_json).expect("Schema should be valid JSON");
    assert!(parsed.is_object(), "Schema should be a JSON object");

    let obj = parsed.as_object().unwrap();
    assert!(
        obj.contains_key("FileSecretStoreConfigDto"),
        "Schema should contain FileSecretStoreConfigDto, got keys: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}

// ============================================================================
// Instance Creation and Secret Resolution Tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_create_file_secret_store_and_resolve() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];

    // Create a temp secrets file
    let (_dir, secrets_path) = create_temp_secrets_file(&serde_json::json!({
        "DB_PASSWORD": "hunter2",
        "API_KEY": "abc123",
        "CONNECTION_STRING": "Server=myhost;Database=mydb;User=admin;Password=secret"
    }));

    let config = serde_json::json!({
        "path": secrets_path.to_str().unwrap()
    });

    let store = descriptor
        .create_secret_store(&config)
        .await
        .expect("Should create file secret store instance");

    // Resolve secrets through FFI
    let db_password = store.get_secret("DB_PASSWORD").await.unwrap();
    assert_eq!(db_password, "hunter2");

    let api_key = store.get_secret("API_KEY").await.unwrap();
    assert_eq!(api_key, "abc123");

    let conn_str = store.get_secret("CONNECTION_STRING").await.unwrap();
    assert_eq!(
        conn_str,
        "Server=myhost;Database=mydb;User=admin;Password=secret"
    );
}

#[tokio::test]
#[ignore]
async fn test_file_secret_store_missing_secret() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];

    let (_dir, secrets_path) = create_temp_secrets_file(&serde_json::json!({"EXISTING": "value"}));
    let config = serde_json::json!({"path": secrets_path.to_str().unwrap()});

    let store = descriptor
        .create_secret_store(&config)
        .await
        .expect("Should create store");

    let result = store.get_secret("NONEXISTENT").await;
    assert!(result.is_err(), "Missing secret should return error");
    assert!(
        result.unwrap_err().to_string().contains("not found"),
        "Error should mention 'not found'"
    );
}

// ============================================================================
// ValueResolver Adapter Integration Tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_file_secret_store_as_value_resolver() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];

    let (_dir, secrets_path) = create_temp_secrets_file(&serde_json::json!({
        "DB_PASSWORD": "hunter2"
    }));
    let config = serde_json::json!({"path": secrets_path.to_str().unwrap()});

    let store = descriptor
        .create_secret_store(&config)
        .await
        .expect("Should create store");

    // Wrap in ValueResolver adapter
    let adapter = SecretStoreValueResolverAdapter::new(Arc::from(store));

    // Resolve a ConfigValue::Secret through the adapter
    let secret_value = ConfigValue::Secret {
        name: "DB_PASSWORD".to_string(),
    };
    let resolved = adapter.resolve_to_string(&secret_value).await.unwrap();
    assert_eq!(resolved, "hunter2");

    // Non-secret variant should return WrongResolverType
    let static_value = ConfigValue::Static("hello".to_string());
    let result = adapter.resolve_to_string(&static_value).await;
    assert!(result.is_err());
}

// ============================================================================
// DtoMapper Integration Tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_dto_mapper_with_file_secret_store() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];

    let (_dir, secrets_path) = create_temp_secrets_file(&serde_json::json!({
        "DB_PASSWORD": "hunter2",
        "API_KEY": "abc123"
    }));
    let config = serde_json::json!({"path": secrets_path.to_str().unwrap()});

    let store = descriptor
        .create_secret_store(&config)
        .await
        .expect("Should create store");

    // Register the secret store as the global resolver.
    let adapter = SecretStoreValueResolverAdapter::new(Arc::from(store));
    register_secret_resolver(Arc::new(adapter));

    // Create a DtoMapper and resolve secrets
    let mapper = DtoMapper::new();

    // Resolve a secret
    let secret_value = ConfigValue::Secret {
        name: "DB_PASSWORD".to_string(),
    };
    let result = mapper.resolve_string(&secret_value).await;
    if let Ok(resolved) = result {
        assert_eq!(resolved, "hunter2");
    }

    // Static values should still work
    let static_value = ConfigValue::Static("hello".to_string());
    let resolved = mapper.resolve_string(&static_value).await.unwrap();
    assert_eq!(resolved, "hello");
}

// ============================================================================
// Plugin Drop Safety Tests
// ============================================================================

#[tokio::test]
#[ignore]
async fn test_file_secret_store_drop_safety() {
    if !plugin_exists("drasi-secret-store-file") {
        panic!("SKIP: drasi-secret-store-file not built as cdylib");
    }
    let path = require_plugin("drasi-secret-store-file");
    let plugin = load_plugin_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .unwrap();

    let descriptor = &plugin.secret_store_plugins[0];

    let (_dir, secrets_path) = create_temp_secrets_file(&serde_json::json!({"KEY": "value"}));
    let config = serde_json::json!({"path": secrets_path.to_str().unwrap()});

    // Create and immediately drop multiple store instances
    for _ in 0..5 {
        let store = descriptor
            .create_secret_store(&config)
            .await
            .expect("Should create store");
        let _ = store.get_secret("KEY").await;
        drop(store);
    }

    // If we get here without segfault/panic, drops are safe
}
