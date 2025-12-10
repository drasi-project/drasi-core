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
use std::path::Path;

/// Configuration for a named storage backend that can be referenced by queries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackendConfig {
    /// Unique identifier for this storage backend
    pub id: String,
    /// Storage backend specification
    #[serde(flatten)]
    pub spec: StorageBackendSpec,
}

/// Storage backend specification defining the type and parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "backend_type")]
pub enum StorageBackendSpec {
    /// In-memory storage backend (volatile, fast, no persistence)
    ///
    /// # Example
    /// ```yaml
    /// backend_type: memory
    /// enable_archive: true
    /// ```
    #[serde(rename = "memory")]
    Memory {
        /// Enable archive index for past() function support
        #[serde(default)]
        enable_archive: bool,
    },

    /// RocksDB storage backend (persistent, local, production-ready)
    ///
    /// # Example
    /// ```yaml
    /// backend_type: rocksdb
    /// path: /data/drasi
    /// enable_archive: true
    /// direct_io: false
    /// ```
    #[serde(rename = "rocksdb")]
    RocksDb {
        /// Base directory path for RocksDB files (must be absolute)
        path: String,
        /// Enable archive index for past() function support
        #[serde(default)]
        enable_archive: bool,
        /// Use direct I/O (bypasses OS cache, may improve performance in some cases)
        #[serde(default)]
        direct_io: bool,
    },

    /// Redis/Garnet storage backend (persistent, distributed, scalable)
    ///
    /// # Example
    /// ```yaml
    /// backend_type: redis
    /// connection_string: "redis://localhost:6379"
    /// cache_size: 10000
    /// ```
    #[serde(rename = "redis")]
    Redis {
        /// Redis connection URL (e.g., "redis://localhost:6379")
        connection_string: String,
        /// Optional local cache size (number of elements to cache)
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_size: Option<usize>,
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
            StorageBackendSpec::RocksDb { path, .. } => {
                // Validate path is absolute
                let path_obj = Path::new(path);
                if !path_obj.is_absolute() {
                    return Err(format!("RocksDB path must be absolute, got: {path}"));
                }
                Ok(())
            }
            StorageBackendSpec::Redis {
                connection_string,
                cache_size,
            } => {
                // Validate connection string format
                if !connection_string.starts_with("redis://")
                    && !connection_string.starts_with("rediss://")
                {
                    return Err(format!(
                        "Redis connection string must start with 'redis://' or 'rediss://', got: {connection_string}"
                    ));
                }

                // Warn if cache size seems unreasonably large
                if let Some(size) = cache_size {
                    if *size > 10_000_000 {
                        log::warn!(
                            "Redis cache_size is very large ({size}), this may consume significant memory"
                        );
                    }
                }
                Ok(())
            }
        }
    }

    /// Check if this storage backend is volatile (requires re-bootstrap after restart)
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
backend_type: memory
enable_archive: true
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
    fn test_rocksdb_serde() {
        let yaml = r#"
backend_type: rocksdb
path: /data/drasi
enable_archive: true
direct_io: false
"#;
        let spec: StorageBackendSpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            StorageBackendSpec::RocksDb {
                path,
                enable_archive,
                direct_io,
            } => {
                assert_eq!(path, "/data/drasi");
                assert!(enable_archive);
                assert!(!direct_io);
            }
            _ => panic!("Expected RocksDb variant"),
        }
    }

    #[test]
    fn test_redis_serde() {
        let yaml = r#"
backend_type: redis
connection_string: "redis://localhost:6379"
cache_size: 10000
"#;
        let spec: StorageBackendSpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            StorageBackendSpec::Redis {
                connection_string,
                cache_size,
            } => {
                assert_eq!(connection_string, "redis://localhost:6379");
                assert_eq!(cache_size, Some(10000));
            }
            _ => panic!("Expected Redis variant"),
        }
    }

    #[test]
    fn test_storage_backend_config_serde() {
        let yaml = r#"
id: rocks_persistent
backend_type: rocksdb
path: /data/drasi
enable_archive: true
"#;
        let config: StorageBackendConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "rocks_persistent");
        match config.spec {
            StorageBackendSpec::RocksDb {
                path,
                enable_archive,
                ..
            } => {
                assert_eq!(path, "/data/drasi");
                assert!(enable_archive);
            }
            _ => panic!("Expected RocksDb variant"),
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
backend_type: memory
enable_archive: false
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
    fn test_validate_rocksdb_absolute_path() {
        let spec = StorageBackendSpec::RocksDb {
            path: "/data/drasi".to_string(),
            enable_archive: true,
            direct_io: false,
        };
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_rocksdb_relative_path() {
        let spec = StorageBackendSpec::RocksDb {
            path: "data/drasi".to_string(),
            enable_archive: true,
            direct_io: false,
        };
        assert!(spec.validate().is_err());
        let err = spec.validate().unwrap_err();
        assert!(err.contains("must be absolute"));
    }

    #[test]
    fn test_validate_redis_valid_url() {
        let spec = StorageBackendSpec::Redis {
            connection_string: "redis://localhost:6379".to_string(),
            cache_size: Some(1000),
        };
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn test_validate_redis_invalid_url() {
        let spec = StorageBackendSpec::Redis {
            connection_string: "localhost:6379".to_string(),
            cache_size: Some(1000),
        };
        assert!(spec.validate().is_err());
        let err = spec.validate().unwrap_err();
        assert!(err.contains("must start with"));
    }

    #[test]
    fn test_is_volatile() {
        let memory_spec = StorageBackendSpec::Memory {
            enable_archive: false,
        };
        assert!(memory_spec.is_volatile());

        let rocks_spec = StorageBackendSpec::RocksDb {
            path: "/data/drasi".to_string(),
            enable_archive: false,
            direct_io: false,
        };
        assert!(!rocks_spec.is_volatile());

        let redis_spec = StorageBackendSpec::Redis {
            connection_string: "redis://localhost:6379".to_string(),
            cache_size: None,
        };
        assert!(!redis_spec.is_volatile());
    }
}
