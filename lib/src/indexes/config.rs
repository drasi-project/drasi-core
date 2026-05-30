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

use serde::{Deserialize, Serialize};

/// Configuration for a named storage backend that can be referenced by queries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackendConfig {
    /// Unique identifier for this storage backend
    pub id: String,
    /// Storage backend specification
    #[serde(flatten)]
    pub spec: StorageBackendSpec,
}

/// Storage backend specification defining the type and parameters.
///
/// In-memory storage is handled natively by drasi-lib. All persistent backends
/// (e.g. `rocksdb`, `redis`) are first-class config plugins: drasi-lib does not
/// carry backend-specific serialization for them. Instead a persistent backend is
/// declared generically by its `kind` plus an opaque `config` payload, and the
/// concrete provider is supplied at runtime via
/// `DrasiLibBuilder::with_index_provider(name, provider)` (the provider name must
/// match the backend id / referenced name).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum StorageBackendSpec {
    /// In-memory storage backend (volatile, fast, no persistence)
    ///
    /// # Example
    /// ```yaml
    /// kind: memory
    /// enableArchive: true
    /// ```
    #[serde(rename = "memory")]
    Memory {
        /// Enable archive index for past() function support
        #[serde(default)]
        enable_archive: bool,
    },

    /// A pluggable persistent storage backend identified by `kind`
    /// (e.g. `rocksdb`, `redis`).
    ///
    /// The `config` payload is opaque to drasi-lib and is interpreted by the
    /// backend's config plugin / injected provider. In embedded mode the backend
    /// is satisfied by a named provider injected via `with_index_provider`.
    ///
    /// # Example
    /// ```yaml
    /// kind: rocksdb
    /// path: /data/drasi
    /// enableArchive: true
    /// ```
    #[serde(untagged)]
    Plugin {
        /// Backend kind discriminator (e.g. "rocksdb", "redis")
        kind: String,
        /// Opaque, backend-specific configuration payload
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

/// Reference to a storage backend, either by name or inline specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StorageBackendRef {
    /// Reference to a named storage backend defined in storage_backends
    Named(String),
    /// Inline storage backend specification
    Inline(StorageBackendSpec),
}

impl StorageBackendSpec {
    /// Validate the storage backend configuration
    pub fn validate(&self) -> Result<(), String> {
        match self {
            StorageBackendSpec::Memory { .. } => Ok(()),
            StorageBackendSpec::Plugin { kind, .. } => {
                if kind.trim().is_empty() {
                    return Err("Storage backend 'kind' must not be empty".to_string());
                }
                Ok(())
            }
        }
    }

    /// Check if this storage backend is volatile (requires re-bootstrap after restart)
    ///
    /// Only in-memory backends are known to be volatile here. For plugin backends the
    /// authoritative answer comes from the injected provider (see
    /// [`crate::indexes::IndexFactory::is_volatile`]); absent a provider we
    /// conservatively assume persistence (not volatile).
    pub fn is_volatile(&self) -> bool {
        matches!(self, StorageBackendSpec::Memory { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_serde() {
        let yaml = r#"
kind: memory
enableArchive: true
"#;
        let spec: StorageBackendSpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            StorageBackendSpec::Memory { enable_archive } => {
                assert!(enable_archive);
            }
            _ => panic!("Expected Memory variant"),
        }

        // Test serialization round-trip
        let serialized = serde_yaml::to_string(&spec).unwrap();
        let deserialized: StorageBackendSpec = serde_yaml::from_str(&serialized).unwrap();
        match deserialized {
            StorageBackendSpec::Memory { enable_archive } => {
                assert!(enable_archive);
            }
            _ => panic!("Expected Memory variant after round-trip"),
        }
    }

    #[test]
    fn test_plugin_rocksdb_serde() {
        let yaml = r#"
kind: rocksdb
path: /data/drasi
enableArchive: true
directIo: false
"#;
        let spec: StorageBackendSpec = serde_yaml::from_str(yaml).unwrap();
        match &spec {
            StorageBackendSpec::Plugin { kind, config } => {
                assert_eq!(kind, "rocksdb");
                assert_eq!(config["path"], "/data/drasi");
                assert_eq!(config["enableArchive"], true);
                assert_eq!(config["directIo"], false);
            }
            _ => panic!("Expected Plugin variant"),
        }

        // Round-trip: Plugin variant preserves kind + opaque config
        let serialized = serde_yaml::to_string(&spec).unwrap();
        let deserialized: StorageBackendSpec = serde_yaml::from_str(&serialized).unwrap();
        match deserialized {
            StorageBackendSpec::Plugin { kind, config } => {
                assert_eq!(kind, "rocksdb");
                assert_eq!(config["path"], "/data/drasi");
            }
            _ => panic!("Expected Plugin variant after round-trip"),
        }
    }

    #[test]
    fn test_plugin_redis_serde() {
        let yaml = r#"
kind: redis
connectionString: "redis://localhost:6379"
cacheSize: 10000
"#;
        let spec: StorageBackendSpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            StorageBackendSpec::Plugin { kind, config } => {
                assert_eq!(kind, "redis");
                assert_eq!(config["connectionString"], "redis://localhost:6379");
                assert_eq!(config["cacheSize"], 10000);
            }
            _ => panic!("Expected Plugin variant"),
        }
    }

    #[test]
    fn test_storage_backend_config_serde() {
        let yaml = r#"
id: rocks_persistent
kind: rocksdb
path: /data/drasi
enableArchive: true
"#;
        let config: StorageBackendConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "rocks_persistent");
        match config.spec {
            StorageBackendSpec::Plugin { kind, config } => {
                assert_eq!(kind, "rocksdb");
                assert_eq!(config["path"], "/data/drasi");
                assert_eq!(config["enableArchive"], true);
            }
            _ => panic!("Expected Plugin variant"),
        }
    }

    #[test]
    fn test_storage_backend_config_memory_serde() {
        let yaml = r#"
id: mem
kind: memory
enableArchive: true
"#;
        let config: StorageBackendConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "mem");
        match config.spec {
            StorageBackendSpec::Memory { enable_archive } => assert!(enable_archive),
            _ => panic!("Expected Memory variant"),
        }
    }

    #[test]
    fn test_storage_backend_ref_named() {
        let yaml = r#""rocks_persistent""#;
        let ref_val: StorageBackendRef = serde_yaml::from_str(yaml).unwrap();
        match ref_val {
            StorageBackendRef::Named(name) => {
                assert_eq!(name, "rocks_persistent");
            }
            _ => panic!("Expected Named variant"),
        }
    }

    #[test]
    fn test_storage_backend_ref_inline() {
        let yaml = r#"
kind: memory
enableArchive: false
"#;
        let ref_val: StorageBackendRef = serde_yaml::from_str(yaml).unwrap();
        match ref_val {
            StorageBackendRef::Inline(spec) => match spec {
                StorageBackendSpec::Memory { enable_archive } => {
                    assert!(!enable_archive);
                }
                _ => panic!("Expected Memory variant"),
            },
            _ => panic!("Expected Inline variant"),
        }
    }

    #[test]
    fn test_validate_memory() {
        let spec = StorageBackendSpec::Memory {
            enable_archive: true,
        };
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_plugin_ok() {
        let spec = StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
            config: serde_json::json!({ "path": "/data/drasi" }),
        };
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_plugin_empty_kind() {
        let spec = StorageBackendSpec::Plugin {
            kind: "   ".to_string(),
            config: serde_json::json!({}),
        };
        assert!(spec.validate().is_err());
        let err = spec.validate().unwrap_err();
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn test_is_volatile() {
        let memory_spec = StorageBackendSpec::Memory {
            enable_archive: false,
        };
        assert!(memory_spec.is_volatile());

        let plugin_spec = StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
            config: serde_json::json!({ "path": "/data/drasi" }),
        };
        assert!(!plugin_spec.is_volatile());
    }
}
