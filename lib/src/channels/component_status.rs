// Copyright 2026 The Drasi Authors.
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

//! Plugin status reporting, independent of the selected graph implementation.

use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use super::ComponentStatus;

/// A status update delivered to the host's selected runtime observer.
#[derive(Debug, Clone)]
pub enum ComponentUpdate {
    Status {
        component_id: String,
        status: ComponentStatus,
        message: Option<String>,
    },
}

/// Cloned by component bases. Sends await capacity rather than dropping status.
pub type ComponentUpdateSender = mpsc::Sender<ComponentUpdate>;
pub type ComponentUpdateReceiver = mpsc::Receiver<ComponentUpdate>;

/// Local plugin status plus its optional host-reporting channel.
///
/// Plugins and their graph-owned adapters use this contract. It does not own graph membership or
/// make the plugin's local status the native controller's lifecycle authority.
#[derive(Clone)]
pub struct ComponentStatusHandle {
    component_id: String,
    status: Arc<RwLock<ComponentStatus>>,
    update_tx: Arc<tokio::sync::OnceCell<ComponentUpdateSender>>,
    status_watch_tx: Arc<tokio::sync::watch::Sender<ComponentStatus>>,
}

impl ComponentStatusHandle {
    pub fn new(component_id: impl Into<String>) -> Self {
        let (watch_tx, _) = tokio::sync::watch::channel(ComponentStatus::Stopped);
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(tokio::sync::OnceCell::new()),
            status_watch_tx: Arc::new(watch_tx),
        }
    }

    pub fn new_wired(component_id: impl Into<String>, update_tx: ComponentUpdateSender) -> Self {
        let cell = tokio::sync::OnceCell::new();
        let _ = cell.set(update_tx);
        let (watch_tx, _) = tokio::sync::watch::channel(ComponentStatus::Stopped);
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(cell),
            status_watch_tx: Arc::new(watch_tx),
        }
    }

    /// Connect the handle to its host observer once.
    pub async fn wire(&self, update_tx: ComponentUpdateSender) {
        let _ = self.update_tx.set(update_tx);
    }

    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        {
            *self.status.write().await = status;
        }
        self.status_watch_tx
            .send_modify(|current| *current = status);
        if let Some(tx) = self.update_tx.get() {
            if let Err(error) = tx
                .send(ComponentUpdate::Status {
                    component_id: self.component_id.clone(),
                    status,
                    message,
                })
                .await
            {
                log::warn!(
                    "Status update for '{}' dropped (channel closed): {error}",
                    self.component_id
                );
            }
        }
    }

    pub async fn get_status(&self) -> ComponentStatus {
        *self.status.read().await
    }

    pub fn subscribe_status(&self) -> tokio::sync::watch::Receiver<ComponentStatus> {
        self.status_watch_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_status_handle_new_defaults_to_stopped() {
        let handle = ComponentStatusHandle::new("comp-1");
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_handle_set_and_get() {
        let handle = ComponentStatusHandle::new("comp-1");
        handle.set_status(ComponentStatus::Running, None).await;
        assert_eq!(handle.get_status().await, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_status_handle_new_wired_sends_update() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = ComponentStatusHandle::new_wired("comp-1", tx);
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
        handle
            .set_status(ComponentStatus::Running, Some("started".into()))
            .await;
        assert_eq!(handle.get_status().await, ComponentStatus::Running);
        let ComponentUpdate::Status {
            component_id,
            status,
            message,
        } = rx.try_recv().unwrap();
        assert_eq!(component_id, "comp-1");
        assert_eq!(status, ComponentStatus::Running);
        assert_eq!(message.as_deref(), Some("started"));
    }

    #[tokio::test]
    async fn test_status_handle_unwired_does_not_send() {
        let handle = ComponentStatusHandle::new("comp-1");
        handle.set_status(ComponentStatus::Error, None).await;
        assert_eq!(handle.get_status().await, ComponentStatus::Error);
    }

    #[tokio::test]
    async fn test_status_handle_wire_after_creation() {
        let (tx, mut rx) = mpsc::channel(16);
        let handle = ComponentStatusHandle::new("comp-1");
        handle.wire(tx).await;
        handle.set_status(ComponentStatus::Starting, None).await;
        let ComponentUpdate::Status {
            component_id,
            status,
            ..
        } = rx.try_recv().unwrap();
        assert_eq!(component_id, "comp-1");
        assert_eq!(status, ComponentStatus::Starting);
    }

    #[tokio::test]
    async fn test_status_handle_wire_only_first_call_takes_effect() {
        let (tx1, mut rx1) = mpsc::channel(16);
        let (tx2, mut rx2) = mpsc::channel(16);
        let handle = ComponentStatusHandle::new("comp-1");
        handle.wire(tx1).await;
        handle.wire(tx2).await;
        handle.set_status(ComponentStatus::Running, None).await;
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_status_handle_clone_shares_state() {
        let handle = ComponentStatusHandle::new("comp-1");
        let clone = handle.clone();
        handle.set_status(ComponentStatus::Running, None).await;
        assert_eq!(clone.get_status().await, ComponentStatus::Running);
    }
}
