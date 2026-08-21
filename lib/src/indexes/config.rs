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
/// In-memory storage is handled natively by drasi-lib. Persistent backends
/// (e.g. `rocksdb`, `redis`) are declared by `kind`, then supplied as configured
/// provider instances through `DrasiLibBuilder::with_index_provider(name, provider)`.
/// Backend-specific settings belong to the provider constructor, not this
/// specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
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
    /// In embedded mode the backend is satisfied by a named provider injected via
    /// `with_index_provider`. Additional configuration properties are rejected
    /// because drasi-lib cannot apply them to an already-constructed provider.
    ///
    /// # Example
    /// ```yaml
    /// kind: rocksdb
    /// ```
    #[serde(untagged)]
    Plugin {
        /// Backend kind discriminator (e.g. "rocksdb", "redis")
        kind: String,
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
            StorageBackendSpec::Plugin { kind } => {
                if kind.trim().is_empty() {
                    return Err("Storage backend 'kind' must not be empty".to_string());
                }
                if kind.trim() == "memory" {
                    return Err(
                        "Storage backend kind 'memory' is reserved for the in-memory backend"
                            .to_string(),
                    );
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
    fn test_json_roundtrip_both_variants() {
        // JSON is the wire format used by StorageBackendConfig in the factory,
        // so assert to_value -> from_value preserves both variants. This pins
        // the (enum-level tag + per-variant untagged) serde behavior so a future
        // serde change can't silently break the JSON round-trip.
        let memory = StorageBackendSpec::Memory {
            enable_archive: true,
        };
        let memory_value = serde_json::to_value(&memory).unwrap();
        assert_eq!(memory_value["kind"], "memory");
        match serde_json::from_value::<StorageBackendSpec>(memory_value).unwrap() {
            StorageBackendSpec::Memory { enable_archive } => assert!(enable_archive),
            _ => panic!("Expected Memory variant after JSON round-trip"),
        }

        let plugin = StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
        };
        let plugin_value = serde_json::to_value(&plugin).unwrap();
        assert_eq!(plugin_value, serde_json::json!({ "kind": "rocksdb" }));
        match serde_json::from_value::<StorageBackendSpec>(plugin_value).unwrap() {
            StorageBackendSpec::Plugin { kind } => assert_eq!(kind, "rocksdb"),
            _ => panic!("Expected Plugin variant after JSON round-trip"),
        }
    }

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
    fn test_plugin_properties_rejected() {
        let yaml = r#"
id: rocks
kind: rocksdb
path: /data/drasi
enableArchive: true
directIo: false
"#;
        assert!(serde_yaml::from_str::<StorageBackendConfig>(yaml).is_err());

        let json = serde_json::json!({
            "id": "rocks",
            "kind": "rocksdb",
            "path": "/data/drasi"
        });
        assert!(serde_json::from_value::<StorageBackendConfig>(json).is_err());
    }

    #[test]
    fn test_plugin_without_config_serde() {
        let spec: StorageBackendSpec = serde_yaml::from_str("kind: rocksdb").unwrap();
        match &spec {
            StorageBackendSpec::Plugin { kind } => assert_eq!(kind, "rocksdb"),
            _ => panic!("Expected Plugin variant"),
        }
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_storage_backend_config_serde() {
        let yaml = r#"
id: rocks_persistent
kind: rocksdb
"#;
        let config: StorageBackendConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "rocks_persistent");
        match config.spec {
            StorageBackendSpec::Plugin { kind } => assert_eq!(kind, "rocksdb"),
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
    fn test_validate_plugin_without_config() {
        let spec = StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
        };
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_plugin_memory_kind() {
        let spec = StorageBackendSpec::Plugin {
            kind: "memory".to_string(),
        };
        let err = spec.validate().unwrap_err();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn test_validate_plugin_empty_kind() {
        let spec = StorageBackendSpec::Plugin {
            kind: "   ".to_string(),
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
        };
        assert!(!plugin_spec.is_volatile());
    }
}
