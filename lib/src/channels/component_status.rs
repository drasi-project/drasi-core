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
/// Both runtime backends use this contract. It does not own graph membership or
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
