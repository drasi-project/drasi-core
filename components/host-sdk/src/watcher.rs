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

//! Policy-neutral plugin filesystem watcher.
//!
//! The [`PluginWatcher`] monitors a plugins directory and emits raw
//! [`PluginFileEvent`]s. It does not decide whether a file change means load
//! or reload — that policy belongs in the host application's orchestrator layer.
//!
//! ## Backends
//!
//! - **`watcher` feature enabled**: Uses the `notify` crate with
//!   `notify-debouncer-mini` for efficient, event-driven filesystem watching.
//! - **Without `watcher` feature**: Falls back to a polling implementation
//!   that periodically scans the directory. Suitable for development and
//!   environments where native fs-event support is unavailable.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::loader::is_plugin_binary;
use crate::plugin_types::PluginFileEvent;

/// Configuration for the plugin filesystem watcher.
#[derive(Debug, Clone)]
pub struct PluginWatcherConfig {
    /// Directory to watch for plugin changes.
    pub plugins_dir: PathBuf,
    /// Debounce duration to coalesce rapid filesystem events.
    pub debounce: Duration,
}

impl Default for PluginWatcherConfig {
    fn default() -> Self {
        Self {
            plugins_dir: PathBuf::from("plugins"),
            debounce: Duration::from_secs(2),
        }
    }
}

/// Watches a plugins directory and emits raw filesystem events.
///
/// The watcher is policy-neutral: it only emits [`PluginFileEvent`]s and does
/// not make any loading decisions. The host application's orchestrator
/// subscribes to these events and applies its own policy.
pub struct PluginWatcher {
    config: PluginWatcherConfig,
    event_tx: broadcast::Sender<PluginFileEvent>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    #[cfg(feature = "watcher")]
    #[allow(dead_code)]
    notify_watcher: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
}

impl PluginWatcher {
    /// Create a new watcher. Call [`start`] or [`start_polling`] to begin monitoring.
    pub fn new(config: PluginWatcherConfig) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            config,
            event_tx,
            shutdown_tx: None,
            #[cfg(feature = "watcher")]
            notify_watcher: None,
        }
    }

    /// Subscribe to plugin file events.
    pub fn subscribe(&self) -> broadcast::Receiver<PluginFileEvent> {
        self.event_tx.subscribe()
    }

    /// The event sender, so the host can forward events programmatically.
    pub fn event_sender(&self) -> &broadcast::Sender<PluginFileEvent> {
        &self.event_tx
    }

    /// Start watching the plugins directory using the best available backend.
    ///
    /// With the `watcher` feature enabled, this uses the `notify` crate for
    /// efficient, event-driven filesystem watching. Without it, falls back to
    /// polling.
    #[cfg(feature = "watcher")]
    pub fn start(&mut self) -> anyhow::Result<()> {
        use notify_debouncer_mini::new_debouncer;
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel();
        let debounce = self.config.debounce;

        let mut debouncer = new_debouncer(debounce, tx)?;
        debouncer.watcher().watch(
            &self.config.plugins_dir,
            notify::RecursiveMode::NonRecursive,
        )?;

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.shutdown_tx = Some(shutdown_tx);

        let event_tx = self.event_tx.clone();
        let dir = self.config.plugins_dir.clone();

        // Track known files for detecting adds vs changes
        let mut known_files: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if is_plugin_binary(&name.to_string_lossy()) {
                    known_files.insert(entry.path());
                }
            }
        }

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        log::debug!("Plugin watcher (notify) shutting down");
                        break;
                    }
                    // Check for fs events on a short interval
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        while let Ok(result) = rx.try_recv() {
                            match result {
                                Ok(events) => {
                                    for event in events {
                                        let path = event.path;
                                        let name = path.file_name()
                                            .map(|n| n.to_string_lossy().to_string())
                                            .unwrap_or_default();

                                        if !is_plugin_binary(&name) {
                                            continue;
                                        }

                                        if path.exists() {
                                            if known_files.contains(&path) {
                                                let _ = event_tx.send(PluginFileEvent::Changed(path));
                                            } else {
                                                known_files.insert(path.clone());
                                                let _ = event_tx.send(PluginFileEvent::Added(path));
                                            }
                                        } else {
                                            known_files.remove(&path);
                                            let _ = event_tx.send(PluginFileEvent::Removed(path));
                                        }
                                    }
                                }
                                Err(err) => {
                                    log::warn!("Filesystem watcher error: {err}");
                                }
                            }
                        }
                    }
                }
            }
        });

        // Keep the debouncer alive
        self.notify_watcher = Some(debouncer);

        Ok(())
    }

    /// Start watching using the notify backend.
    ///
    /// Without the `watcher` feature, this delegates to `start_polling`.
    #[cfg(not(feature = "watcher"))]
    pub fn start(&mut self) -> anyhow::Result<()> {
        self.start_polling()
    }

    /// Start watching the plugins directory using a polling loop.
    ///
    /// This is the fallback implementation that works without the `notify` crate.
    /// It periodically scans the directory and compares file sizes to detect changes.
    pub fn start_polling(&mut self) -> anyhow::Result<()> {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        self.shutdown_tx = Some(shutdown_tx);

        let event_tx = self.event_tx.clone();
        let dir = self.config.plugins_dir.clone();
        let debounce = self.config.debounce;

        tokio::spawn(async move {
            // Track both size and mtime to detect changes even when file size stays the same
            // (common during rebuilds/strip operations).
            let mut known_files: std::collections::HashMap<PathBuf, (u64, std::time::SystemTime)> =
                std::collections::HashMap::new();

            // Initial scan
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = entry.file_name();
                    if is_plugin_binary(&name.to_string_lossy()) {
                        if let Ok(meta) = entry.metadata() {
                            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                            known_files.insert(path, (meta.len(), mtime));
                        }
                    }
                }
            }

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        log::debug!("Plugin watcher shutting down");
                        break;
                    }
                    _ = tokio::time::sleep(debounce) => {
                        // Poll for changes
                        let mut current_files: std::collections::HashMap<PathBuf, (u64, std::time::SystemTime)> =
                            std::collections::HashMap::new();

                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for entry in entries.flatten() {
                                let path = entry.path();
                                let name = entry.file_name();
                                if is_plugin_binary(&name.to_string_lossy()) {
                                    if let Ok(meta) = entry.metadata() {
                                        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                                        current_files.insert(path, (meta.len(), mtime));
                                    }
                                }
                            }
                        }

                        // Detect additions and changes (by size or mtime)
                        for (path, (size, mtime)) in &current_files {
                            match known_files.get(path) {
                                None => {
                                    let _ = event_tx.send(PluginFileEvent::Added(path.clone()));
                                }
                                Some((old_size, old_mtime)) if old_size != size || old_mtime != mtime => {
                                    let _ = event_tx.send(PluginFileEvent::Changed(path.clone()));
                                }
                                _ => {}
                            }
                        }

                        // Detect removals
                        for path in known_files.keys() {
                            if !current_files.contains_key(path) {
                                let _ = event_tx.send(PluginFileEvent::Removed(path.clone()));
                            }
                        }

                        known_files = current_files;
                    }
                }
            }
        });

        Ok(())
    }

    /// Stop the watcher.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        #[cfg(feature = "watcher")]
        {
            self.notify_watcher = None;
        }
    }
}

impl Drop for PluginWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watcher_config_default() {
        let config = PluginWatcherConfig::default();
        assert_eq!(config.debounce, Duration::from_secs(2));
    }

    #[test]
    fn test_watcher_creation() {
        let config = PluginWatcherConfig {
            plugins_dir: PathBuf::from("/tmp/plugins"),
            debounce: Duration::from_millis(500),
        };
        let watcher = PluginWatcher::new(config);
        let _rx = watcher.subscribe();
    }

    #[tokio::test]
    async fn test_watcher_detects_new_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = PluginWatcherConfig {
            plugins_dir: dir.path().to_path_buf(),
            debounce: Duration::from_millis(100),
        };

        let mut watcher = PluginWatcher::new(config);
        let mut rx = watcher.subscribe();
        watcher.start_polling().expect("start");

        // Wait for initial scan
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Add a plugin file
        std::fs::write(
            dir.path().join("libdrasi_source_test.dylib"),
            b"fake plugin",
        )
        .expect("write");

        // Wait for detection
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should have received an Added event
        let event = rx.try_recv();
        assert!(event.is_ok(), "expected event, got {event:?}");
        match event.expect("event") {
            PluginFileEvent::Added(path) => {
                assert!(path.to_string_lossy().contains("libdrasi_source_test"));
            }
            other => panic!("expected Added, got {other:?}"),
        }

        watcher.stop();
    }
}
