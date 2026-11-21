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

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

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
    /// Create a new watcher. Call [`Self::start`] or [`Self::start_polling`] to begin monitoring.
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

    /// Prepare a join-owned polling backend without spawning notify/debouncer
    /// threads, even with the `watcher` feature. Register this handle with the
    /// application's owner before awaiting `start`. The prepared handle is
    /// single-use: stopping or completing cleanup fences subsequent starts.
    pub fn prepare_owned_polling(&self) -> OwnedPollingWatcher {
        OwnedPollingWatcher {
            inner: Arc::new(PollingOwner {
                config: self.config.clone(),
                events: self.event_tx.clone(),
                join: tokio::sync::RwLock::new(None),
                state: Mutex::new(PollingState {
                    stop: None,
                    terminal: None,
                    started: false,
                    stopping: false,
                }),
            }),
        }
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

type PollingResult = Result<(), Arc<PollingWatcherError>>;

/// Actual ownership of one blocking polling loop. Legacy `start`/`stop` do
/// not provide this contract and are intentionally unchanged.
#[derive(Clone)]
pub struct OwnedPollingWatcher {
    inner: Arc<PollingOwner>,
}

struct PollingOwner {
    config: PluginWatcherConfig,
    events: broadcast::Sender<PluginFileEvent>,
    join: tokio::sync::RwLock<Option<tokio::task::JoinHandle<PollingResult>>>,
    state: Mutex<PollingState>,
}

// Start reservation and no-worker finalization share this lock. A vacant join
// slot during reserved submission is not evidence of terminal cleanup.
struct PollingState {
    stop: Option<std::sync::mpsc::Sender<()>>,
    terminal: Option<PollingResult>,
    started: bool,
    stopping: bool,
}

impl Drop for PollingOwner {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .stop
            .take();
    }
}

struct PollingStartWait {
    watcher: OwnedPollingWatcher,
    submitted: bool,
    completed: bool,
}

impl Drop for PollingStartWait {
    fn drop(&mut self) {
        if !self.completed {
            self.watcher.request_stop();
            if !self.submitted {
                self.watcher
                    .inner
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .started = false;
            }
        }
    }
}

#[derive(Debug)]
pub enum PollingWatcherError {
    Scan(std::io::Error),
    Join(tokio::task::JoinError),
    WaitExpired,
    AlreadyStarted,
    Finalized,
    StopRequested,
    InitializationClosed,
    SubmissionPending,
    Submission(drasi_computation_contracts::workers::WorkerAlreadyOwned),
    Runtime(tokio::runtime::TryCurrentError),
}

impl std::fmt::Display for PollingWatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Scan(_) => "Plugin polling directory scan failed",
            Self::Join(_) => "Plugin polling worker failed",
            Self::WaitExpired => "Plugin polling join deadline elapsed; backend remains owned",
            Self::AlreadyStarted => "Plugin polling backend is already owned",
            Self::Finalized => "Plugin polling backend cleanup is already finalized",
            Self::StopRequested => "Plugin polling backend stop has already been requested",
            Self::InitializationClosed => "Plugin polling initialization worker exited",
            Self::SubmissionPending => "Plugin polling submission has not installed its worker",
            Self::Submission(_) => "Plugin polling worker submission failed",
            Self::Runtime(_) => "Plugin polling requires a live Tokio runtime",
        })
    }
}

impl std::error::Error for PollingWatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Join(error) => Some(error),
            Self::Submission(error) => Some(error),
            Self::Runtime(error) => Some(error),
            _ => None,
        }
    }
}

impl OwnedPollingWatcher {
    /// Initial scan, all later scans and destruction run off the Tokio executor.
    /// Size/mtime deltas emit Added/Changed/Removed once per debounce interval;
    /// the initial snapshot emits nothing. Failed scans never invent removals.
    /// Rejects finalized handles and stop intent before reserving submission.
    pub async fn start(&self) -> PollingResult {
        tokio::runtime::Handle::try_current()
            .map_err(|error| Arc::new(PollingWatcherError::Runtime(error)))?;
        let (stop, stopping) = std::sync::mpsc::channel();
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.terminal.is_some() {
                return Err(Arc::new(PollingWatcherError::Finalized));
            }
            if state.stopping {
                return Err(Arc::new(PollingWatcherError::StopRequested));
            }
            if state.started {
                return Err(Arc::new(PollingWatcherError::AlreadyStarted));
            }
            state.started = true;
            state.stop = Some(stop);
        }
        let mut waiter = PollingStartWait {
            watcher: self.clone(),
            submitted: false,
            completed: false,
        };
        let (ready, initialized) = tokio::sync::oneshot::channel();
        let config = self.inner.config.clone();
        let events = self.inner.events.clone();
        drasi_computation_contracts::workers::spawn_owned_blocking_worker(
            &self.inner.join,
            move || {
                let mut known = match scan_plugins(&config.plugins_dir) {
                    Ok(files) => files,
                    Err(error) => {
                        let error = Arc::new(PollingWatcherError::Scan(error));
                        let _ = ready.send(Err(error.clone()));
                        return Err(error);
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return Ok(());
                }
                let interval = config.debounce.max(Duration::from_millis(1));
                loop {
                    match stopping.recv_timeout(interval) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return Ok(())
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let current = scan_plugins(&config.plugins_dir)
                        .map_err(|e| Arc::new(PollingWatcherError::Scan(e)))?;
                    for (path, metadata) in &current {
                        let event = match known.get(path) {
                            None => Some(PluginFileEvent::Added(path.clone())),
                            Some(old) if old != metadata => {
                                Some(PluginFileEvent::Changed(path.clone()))
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            let _ = events.send(event);
                        }
                    }
                    for path in known.keys().filter(|path| !current.contains_key(*path)) {
                        let _ = events.send(PluginFileEvent::Removed(path.clone()));
                    }
                    known = current;
                }
            },
        )
        .await
        .map_err(|error| Arc::new(PollingWatcherError::Submission(error)))?;
        waiter.submitted = true;
        let result = initialized
            .await
            .map_err(|_| Arc::new(PollingWatcherError::InitializationClosed))?;
        waiter.completed = true;
        result
    }

    pub fn request_stop(&self) {
        let stop = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopping = true;
            state.stop.take()
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
    }

    /// Timeout/cancellation retain the original join. Never aborts a scan.
    pub async fn wait_until(&self, deadline: tokio::time::Instant) -> PollingResult {
        if let Some(result) = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
            .clone()
        {
            return result;
        }
        let mut slot = tokio::time::timeout_at(deadline, self.inner.join.write())
            .await
            .map_err(|_| Arc::new(PollingWatcherError::WaitExpired))?;
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(result) = &state.terminal {
                return result.clone();
            }
            if slot.is_none() {
                if state.started {
                    return Err(Arc::new(PollingWatcherError::SubmissionPending));
                }
                state.terminal = Some(Ok(()));
                return Ok(());
            }
        }
        let join = slot
            .as_mut()
            .ok_or_else(|| Arc::new(PollingWatcherError::SubmissionPending))?;
        let result = match tokio::time::timeout_at(deadline, join).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(Arc::new(PollingWatcherError::Join(error))),
            Err(_) => return Err(Arc::new(PollingWatcherError::WaitExpired)),
        };
        *slot = None;
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal = Some(result.clone());
        result
    }

    /// Await physical completion, not a fresh stop/flush allowance.
    pub async fn wait_for_completion(&self) -> PollingResult {
        let mut slot = self.inner.join.write().await;
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(result) = &state.terminal {
                return result.clone();
            }
            if slot.is_none() {
                if state.started {
                    return Err(Arc::new(PollingWatcherError::SubmissionPending));
                }
                state.terminal = Some(Ok(()));
                return Ok(());
            }
        }
        let join = slot
            .as_mut()
            .ok_or_else(|| Arc::new(PollingWatcherError::SubmissionPending))?;
        let result = match join.await {
            Ok(result) => result,
            Err(error) => Err(Arc::new(PollingWatcherError::Join(error))),
        };
        *slot = None;
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal = Some(result.clone());
        result
    }

    pub fn is_joined(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
            .is_some()
    }
}

fn scan_plugins(
    dir: &std::path::Path,
) -> std::io::Result<std::collections::HashMap<PathBuf, (u64, std::time::SystemTime)>> {
    let mut files = std::collections::HashMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !is_plugin_binary(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() {
            files.insert(entry.path(), (meta.len(), meta.modified()?));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Break caught: finalizing an unused handle caches success but still allows
    // its first start to register a backend whose join is hidden by that cache.
    #[tokio::test(flavor = "current_thread")]
    async fn owned_polling_prepared_completion_rejects_start() -> anyhow::Result<()> {
        for bounded in [false, true] {
            let watcher = PluginWatcher::new(PluginWatcherConfig::default());
            let owned = watcher.prepare_owned_polling();
            if bounded {
                owned
                    .wait_until(tokio::time::Instant::now() + Duration::from_secs(5))
                    .await?;
            } else {
                owned.wait_for_completion().await?;
            }
            let error = owned
                .start()
                .await
                .expect_err("finalized handle cannot start");
            assert!(matches!(error.as_ref(), PollingWatcherError::Finalized));
            assert!(owned.is_joined());
            assert!(owned.inner.join.read().await.is_none());
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_polling_stop_intent_rejects_start() -> anyhow::Result<()> {
        let watcher = PluginWatcher::new(PluginWatcherConfig::default());
        let owned = watcher.prepare_owned_polling();
        owned.request_stop();
        let error = owned.start().await.expect_err("stop intent fences startup");
        assert!(matches!(error.as_ref(), PollingWatcherError::StopRequested));
        owned.wait_for_completion().await?;
        assert!(owned.inner.join.read().await.is_none());
        Ok(())
    }

    // Hold the real registration slot, not a fabricated worker-completion bit.
    // Expiry/cancellation must not finalize a start awaiting registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_polling_pending_start_and_wait_keep_the_actual_join() -> anyhow::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("owned-watcher-start-")
            .tempdir_in(".")?;
        let watcher = PluginWatcher::new(PluginWatcherConfig {
            plugins_dir: dir.path().into(),
            debounce: Duration::from_secs(60),
        });
        let owned = watcher.prepare_owned_polling();
        let registration = owned.inner.join.read().await;
        let start = owned.start();
        tokio::pin!(start);
        assert!(futures_util::poll!(&mut start).is_pending());
        let error = owned
            .wait_until(tokio::time::Instant::now() + Duration::from_millis(20))
            .await
            .expect_err("registration remains pending");
        assert!(matches!(error.as_ref(), PollingWatcherError::WaitExpired));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), owned.wait_for_completion(),)
                .await
                .is_err()
        );
        assert!(!owned.is_joined());
        owned.request_stop();
        drop(registration);
        let (started, joined) = tokio::join!(start, owned.wait_for_completion());
        started?;
        joined?;
        assert!(owned.is_joined());
        assert!(owned.inner.join.read().await.is_none());
        let error = owned
            .start()
            .await
            .expect_err("joined handle cannot restart");
        assert!(matches!(error.as_ref(), PollingWatcherError::Finalized));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_polling_cancelled_unsubmitted_start_cannot_restart() -> anyhow::Result<()> {
        let watcher = PluginWatcher::new(PluginWatcherConfig::default());
        let owned = watcher.prepare_owned_polling();
        let registration = owned.inner.join.read().await;
        {
            let start = owned.start();
            tokio::pin!(start);
            assert!(futures_util::poll!(&mut start).is_pending());
        }
        drop(registration);
        owned.wait_for_completion().await?;
        let error = owned
            .start()
            .await
            .expect_err("cancelled start is terminal");
        assert!(matches!(error.as_ref(), PollingWatcherError::Finalized));
        assert!(owned.inner.join.read().await.is_none());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_polling_joins_and_detects_same_size_changes() -> anyhow::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("owned-watcher-")
            .tempdir_in(".")?;
        let watcher = PluginWatcher::new(PluginWatcherConfig {
            plugins_dir: dir.path().into(),
            debounce: Duration::from_millis(20),
        });
        let mut events = watcher.subscribe();
        let owned = watcher.prepare_owned_polling();
        owned.start().await?;
        let path = dir.path().join("libdrasi_source_owned.dylib");
        std::fs::write(&path, b"one")?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await??,
            PluginFileEvent::Added(_)
        ));
        std::fs::write(&path, b"two")?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await??,
            PluginFileEvent::Changed(_)
        ));
        std::fs::remove_file(&path)?;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), events.recv()).await??,
            PluginFileEvent::Removed(_)
        ));
        owned.request_stop();
        owned
            .wait_until(tokio::time::Instant::now() + Duration::from_secs(5))
            .await?;
        owned.wait_until(tokio::time::Instant::now()).await?;
        assert!(owned.is_joined());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owned_polling_expired_wait_retains_backend() -> anyhow::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("owned-watcher-wait-")
            .tempdir_in(".")?;
        let watcher = PluginWatcher::new(PluginWatcherConfig {
            plugins_dir: dir.path().into(),
            debounce: Duration::from_secs(60),
        });
        let owned = watcher.prepare_owned_polling();
        owned.start().await?;
        assert!(owned.wait_until(tokio::time::Instant::now()).await.is_err());
        assert!(!owned.is_joined());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), owned.wait_for_completion(),)
                .await
                .is_err()
        );
        assert!(!owned.is_joined());
        owned.request_stop();
        owned
            .wait_until(tokio::time::Instant::now() + Duration::from_secs(5))
            .await?;
        assert!(owned.is_joined());
        Ok(())
    }

    #[test]
    fn test_watcher_config_default() {
        let config = PluginWatcherConfig::default();
        assert_eq!(config.debounce, Duration::from_secs(2));
    }

    #[test]
    fn test_watcher_creation() {
        let config = PluginWatcherConfig {
            plugins_dir: PathBuf::from("plugins"),
            debounce: Duration::from_millis(500),
        };
        let watcher = PluginWatcher::new(config);
        let _rx = watcher.subscribe();
    }

    #[tokio::test]
    async fn test_watcher_detects_new_file() {
        let dir = tempfile::Builder::new()
            .prefix("polling-watcher-")
            .tempdir_in(".")
            .expect("temp dir");
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
