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

//! Shared plugin management types used by the host-sdk lifecycle layer
//! and consumed by host applications like drasi-server.

use std::path::PathBuf;

/// Category of a plugin descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PluginCategory {
    Source,
    Reaction,
    Bootstrap,
}

impl std::fmt::Display for PluginCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginCategory::Source => write!(f, "source"),
            PluginCategory::Reaction => write!(f, "reaction"),
            PluginCategory::Bootstrap => write!(f, "bootstrap"),
        }
    }
}

/// Lightweight representation of a single descriptor kind provided by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginKindEntry {
    pub category: PluginCategory,
    pub kind: String,
    pub config_version: String,
    pub config_schema_name: String,
}

/// Lifecycle status of a loaded plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PluginStatus {
    /// Library loaded, descriptors registered, no instances yet.
    Loaded,
    /// Has running component instances.
    Active,
    /// Being replaced: instances migrating to new version.
    Draining,
    /// No active instances, descriptors deregistered, library remains mapped.
    Retired,
    /// Load or initialization failed.
    Failed,
}

impl std::fmt::Display for PluginStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginStatus::Loaded => write!(f, "Loaded"),
            PluginStatus::Active => write!(f, "Active"),
            PluginStatus::Draining => write!(f, "Draining"),
            PluginStatus::Retired => write!(f, "Retired"),
            PluginStatus::Failed => write!(f, "Failed"),
        }
    }
}

/// Events emitted by the plugin lifecycle layer.
///
/// These are broadcast through a `tokio::sync::broadcast` channel and can be
/// consumed by server-level SSE, UI updates, or logging.
#[derive(Debug, Clone)]
pub enum PluginEvent {
    /// A new plugin was loaded (no prior version existed).
    Loaded {
        plugin_id: String,
        version: String,
        kinds: Vec<PluginKindEntry>,
    },
    /// A new version was loaded side-by-side with an existing version.
    LoadedSideBySide {
        plugin_id: String,
        version: String,
        incumbent_plugin_id: String,
        versioned_kinds: Vec<String>,
    },
    /// An upgrade (drain-then-replace) completed successfully.
    Upgraded {
        plugin_id: String,
        old_version: String,
        new_version: String,
        migrated_components: Vec<String>,
    },
    /// An upgrade completed but some components failed to migrate.
    UpgradePartialFailure {
        plugin_id: String,
        old_version: String,
        new_version: String,
        migrated: Vec<String>,
        failed: Vec<(String, String)>,
    },
    /// A plugin is being drained (upgrade in progress).
    Draining {
        plugin_id: String,
        affected_components: Vec<String>,
    },
    /// A side-by-side version was promoted to incumbent.
    Promoted {
        plugin_id: String,
        promoted_kinds: Vec<String>,
        previous_incumbent: String,
    },
    /// A plugin was retired.
    Retired { plugin_id: String },
    /// A plugin failed to load.
    LoadFailed { path: PathBuf, error: String },
}

/// Raw filesystem events emitted by the `PluginWatcher`.
///
/// These are policy-neutral: the watcher does not decide whether a file change
/// means load, upgrade, side-by-side, or retire. That decision belongs to the
/// host application's orchestrator layer.
#[derive(Debug, Clone)]
pub enum PluginFileEvent {
    Added(PathBuf),
    Changed(PathBuf),
    Removed(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_category_display() {
        assert_eq!(PluginCategory::Source.to_string(), "source");
        assert_eq!(PluginCategory::Reaction.to_string(), "reaction");
        assert_eq!(PluginCategory::Bootstrap.to_string(), "bootstrap");
    }

    #[test]
    fn plugin_status_display() {
        assert_eq!(PluginStatus::Loaded.to_string(), "Loaded");
        assert_eq!(PluginStatus::Active.to_string(), "Active");
        assert_eq!(PluginStatus::Draining.to_string(), "Draining");
        assert_eq!(PluginStatus::Retired.to_string(), "Retired");
        assert_eq!(PluginStatus::Failed.to_string(), "Failed");
    }

    #[test]
    fn plugin_kind_entry_serde_roundtrip() {
        let entry = PluginKindEntry {
            category: PluginCategory::Source,
            kind: "postgres".to_string(),
            config_version: "1.0.0".to_string(),
            config_schema_name: "PostgresSourceConfig".to_string(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let deserialized: PluginKindEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, deserialized);
    }
}
