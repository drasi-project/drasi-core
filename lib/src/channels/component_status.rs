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

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
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

/// Reports lifecycle observations to the host.
///
/// Graph-owned observers coalesce intermediate statuses while preserving readiness,
/// the first unobserved failure, and the latest status. A sender converted from an
/// ordinary MPSC channel retains that channel's backpressure and `try_send` behavior.
#[derive(Clone)]
pub struct ComponentUpdateSender {
    transport: ComponentUpdateTransport,
}

#[derive(Clone)]
enum ComponentUpdateTransport {
    Queue(mpsc::Sender<ComponentUpdate>),
    Observed {
        pending: Arc<Mutex<PendingComponentUpdates>>,
        wakeup: mpsc::Sender<()>,
    },
}

struct PendingComponentUpdates {
    component_id: String,
    updates: VecDeque<ComponentUpdate>,
}

pub(crate) const MAX_COMPONENT_OBSERVATIONS: usize = 3;

impl PendingComponentUpdates {
    fn push(&mut self, update: ComponentUpdate) {
        self.updates.push_back(update);
        let last = self.updates.len() - 1;
        let mut index = 0;
        let mut saw_running = false;
        let mut saw_failure = false;
        self.updates.retain(|update| {
            let ComponentUpdate::Status {
                component_id,
                status,
                ..
            } = update;
            let running = component_id == &self.component_id && *status == ComponentStatus::Running;
            let failure = component_id != &self.component_id || *status == ComponentStatus::Error;
            let keep = index == last || (running && !saw_running) || (failure && !saw_failure);
            saw_running |= running;
            saw_failure |= failure;
            index += 1;
            keep
        });
    }
}

pub(crate) struct ComponentUpdateObserver {
    pending: Arc<Mutex<PendingComponentUpdates>>,
    wakeup: mpsc::Receiver<()>,
}

impl ComponentUpdateObserver {
    pub(crate) fn try_recv(&mut self) -> anyhow::Result<Option<ComponentUpdate>> {
        Ok(self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("plugin lifecycle mailbox is poisoned"))?
            .updates
            .pop_front())
    }

    pub(crate) async fn recv(&mut self) -> anyhow::Result<Option<ComponentUpdate>> {
        loop {
            if let Some(update) = self.try_recv()? {
                return Ok(Some(update));
            }
            if self.wakeup.recv().await.is_none() {
                return self.try_recv();
            }
        }
    }

    pub(crate) fn reset(&mut self) -> anyhow::Result<()> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("plugin lifecycle mailbox is poisoned"))?;
        pending.updates.clear();
        while self.wakeup.try_recv().is_ok() {}
        Ok(())
    }
}

impl From<mpsc::Sender<ComponentUpdate>> for ComponentUpdateSender {
    fn from(sender: mpsc::Sender<ComponentUpdate>) -> Self {
        Self {
            transport: ComponentUpdateTransport::Queue(sender),
        }
    }
}

impl std::fmt::Debug for ComponentUpdateSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentUpdateSender")
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl ComponentUpdateSender {
    pub(crate) fn observed(component_id: &str) -> (Self, ComponentUpdateObserver) {
        let pending = Arc::new(Mutex::new(PendingComponentUpdates {
            component_id: component_id.to_owned(),
            updates: VecDeque::with_capacity(MAX_COMPONENT_OBSERVATIONS + 1),
        }));
        let (wakeup, receiver) = mpsc::channel(1);
        (
            Self {
                transport: ComponentUpdateTransport::Observed {
                    pending: pending.clone(),
                    wakeup,
                },
            },
            ComponentUpdateObserver {
                pending,
                wakeup: receiver,
            },
        )
    }

    pub async fn send(
        &self,
        update: ComponentUpdate,
    ) -> Result<(), mpsc::error::SendError<ComponentUpdate>> {
        match &self.transport {
            ComponentUpdateTransport::Queue(sender) => sender.send(update).await,
            ComponentUpdateTransport::Observed { .. } => {
                tokio::task::consume_budget().await;
                self.try_send(update)
                    .map_err(|error| mpsc::error::SendError(error.into_inner()))
            }
        }
    }

    pub fn try_send(
        &self,
        update: ComponentUpdate,
    ) -> Result<(), mpsc::error::TrySendError<ComponentUpdate>> {
        match &self.transport {
            ComponentUpdateTransport::Queue(sender) => sender.try_send(update),
            ComponentUpdateTransport::Observed { pending, wakeup } => {
                let mut pending = match pending.lock() {
                    Ok(pending) => pending,
                    Err(_) => {
                        log::error!("Cannot report status: plugin lifecycle mailbox is poisoned");
                        let _ = wakeup.try_send(());
                        return Err(mpsc::error::TrySendError::Closed(update));
                    }
                };
                if wakeup.is_closed() {
                    return Err(mpsc::error::TrySendError::Closed(update));
                }
                pending.push(update);
                match wakeup.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => Ok(()),
                    Err(mpsc::error::TrySendError::Closed(())) => {
                        Err(mpsc::error::TrySendError::Closed(
                            pending.updates.pop_back().expect("latest status retained"),
                        ))
                    }
                }
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        match &self.transport {
            ComponentUpdateTransport::Queue(sender) => sender.is_closed(),
            ComponentUpdateTransport::Observed { wakeup, .. } => wakeup.is_closed(),
        }
    }

    pub async fn closed(&self) {
        match &self.transport {
            ComponentUpdateTransport::Queue(sender) => sender.closed().await,
            ComponentUpdateTransport::Observed { wakeup, .. } => wakeup.closed().await,
        }
    }
}

pub type ComponentUpdateReceiver = mpsc::Receiver<ComponentUpdate>;

/// Local plugin status plus its optional host-reporting channel.
///
/// Plugins and their graph-owned adapters use this contract. It does not own graph membership or
/// make the plugin's local status the native controller's lifecycle authority.
#[derive(Clone)]
pub struct ComponentStatusHandle {
    component_id: String,
    status: Arc<RwLock<ComponentStatus>>,
    update_tx: Arc<RwLock<Option<ComponentUpdateSender>>>,
    status_watch_tx: Arc<tokio::sync::watch::Sender<ComponentStatus>>,
}

impl ComponentStatusHandle {
    pub fn new(component_id: impl Into<String>) -> Self {
        let (watch_tx, _) = tokio::sync::watch::channel(ComponentStatus::Stopped);
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(RwLock::new(None)),
            status_watch_tx: Arc::new(watch_tx),
        }
    }

    pub fn new_wired(
        component_id: impl Into<String>,
        update_tx: impl Into<ComponentUpdateSender>,
    ) -> Self {
        let (watch_tx, _) = tokio::sync::watch::channel(ComponentStatus::Stopped);
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(RwLock::new(Some(update_tx.into()))),
            status_watch_tx: Arc::new(watch_tx),
        }
    }

    /// Connect an unwired handle or replace a closed host observer.
    ///
    /// Before rewiring, the owner must stop and join the previous run's workers.
    pub async fn wire(&self, update_tx: impl Into<ComponentUpdateSender>) {
        let mut current = self.update_tx.write().await;
        if current
            .as_ref()
            .map_or(true, ComponentUpdateSender::is_closed)
        {
            *current = Some(update_tx.into());
        }
    }

    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        {
            *self.status.write().await = status;
        }
        self.status_watch_tx
            .send_modify(|current| *current = status);
        let update_tx = self.update_tx.read().await.clone();
        if let Some(tx) = update_tx {
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
    async fn rewiring_status_handle_replaces_a_revoked_observer() {
        let (old, old_observer) = ComponentUpdateSender::observed("component");
        let handle = ComponentStatusHandle::new_wired("component", old.clone());
        let reporter = handle.clone();
        drop(old_observer);
        assert!(old.is_closed());

        let (current, mut observer) = ComponentUpdateSender::observed("component");
        handle.wire(current).await;
        reporter
            .set_status(ComponentStatus::Error, Some("current failure".into()))
            .await;
        assert_update(
            observer.try_recv().unwrap().unwrap(),
            ComponentStatus::Error,
            "current failure",
        );
    }

    fn update(component_id: &str, status: ComponentStatus, message: &str) -> ComponentUpdate {
        ComponentUpdate::Status {
            component_id: component_id.to_owned(),
            status,
            message: Some(message.to_owned()),
        }
    }

    fn assert_update(update: ComponentUpdate, expected_status: ComponentStatus, expected: &str) {
        let ComponentUpdate::Status {
            status, message, ..
        } = update;
        assert_eq!(status, expected_status);
        assert_eq!(message.as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn observed_flood_preserves_readiness_first_failure_and_latest_status() {
        let (sender, mut receiver) = ComponentUpdateSender::observed("source");
        sender
            .try_send(update("source", ComponentStatus::Running, "ready"))
            .unwrap();
        sender
            .try_send(update("source", ComponentStatus::Error, "first failure"))
            .unwrap();
        for index in 0..10_000 {
            sender
                .try_send(update("source", ComponentStatus::Error, &index.to_string()))
                .unwrap();
            assert!(receiver.pending.lock().unwrap().updates.len() <= MAX_COMPONENT_OBSERVATIONS);
        }
        sender
            .try_send(update("source", ComponentStatus::Stopped, "last status"))
            .unwrap();
        assert_update(
            receiver.recv().await.unwrap().unwrap(),
            ComponentStatus::Running,
            "ready",
        );
        assert_update(
            receiver.recv().await.unwrap().unwrap(),
            ComponentStatus::Error,
            "first failure",
        );
        assert_update(
            receiver.recv().await.unwrap().unwrap(),
            ComponentStatus::Stopped,
            "last status",
        );
        assert!(receiver.try_recv().unwrap().is_none());
        drop(sender);
        assert!(receiver.recv().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn observed_wrong_component_status_cannot_be_hidden_by_later_updates() {
        let (sender, mut receiver) = ComponentUpdateSender::observed("source");
        sender
            .try_send(update(
                "another-source",
                ComponentStatus::Stopped,
                "wrong id",
            ))
            .unwrap();
        for _ in 0..1_000 {
            sender
                .try_send(update("source", ComponentStatus::Starting, "latest"))
                .unwrap();
        }
        let ComponentUpdate::Status { component_id, .. } = receiver.recv().await.unwrap().unwrap();
        assert_eq!(component_id, "another-source");
    }

    #[tokio::test]
    async fn observed_reset_discards_only_prior_pending_updates() {
        let (sender, mut receiver) = ComponentUpdateSender::observed("source");
        sender
            .try_send(update("source", ComponentStatus::Error, "old"))
            .unwrap();
        receiver.reset().unwrap();
        {
            let read = receiver.recv();
            tokio::pin!(read);
            assert!(futures::poll!(&mut read).is_pending());
        }
        sender
            .try_send(update("source", ComponentStatus::Running, "new"))
            .unwrap();
        assert_update(
            receiver.recv().await.unwrap().unwrap(),
            ComponentStatus::Running,
            "new",
        );
    }

    #[tokio::test]
    async fn closing_observer_revokes_all_callback_clones() {
        let (sender, receiver) = ComponentUpdateSender::observed("source");
        let callback = sender.clone();
        drop(receiver);
        sender.closed().await;
        assert!(callback.is_closed());
        let error = callback
            .try_send(update("source", ComponentStatus::Error, "late callback"))
            .unwrap_err();
        assert_update(error.into_inner(), ComponentStatus::Error, "late callback");
    }

    #[tokio::test]
    async fn asynchronous_observed_flood_yields_to_other_current_thread_work() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (sender, _receiver) = ComponentUpdateSender::observed("source");
        let other_work_ran = AtomicBool::new(false);
        tokio::join!(
            async {
                for index in 0..4_096 {
                    sender
                        .send(update("source", ComponentStatus::Starting, "busy"))
                        .await
                        .unwrap();
                    if index == 1_024 {
                        assert!(other_work_ran.load(Ordering::Acquire));
                    }
                }
            },
            async { other_work_ran.store(true, Ordering::Release) },
        );
    }

    #[tokio::test]
    async fn poisoned_observation_mailbox_fails_explicitly() {
        let (sender, mut receiver) = ComponentUpdateSender::observed("source");
        let pending = receiver.pending.clone();
        assert!(std::thread::spawn(move || {
            let _guard = pending.lock().unwrap();
            panic!("injected mailbox poison");
        })
        .join()
        .is_err());
        assert!(sender
            .try_send(update("source", ComponentStatus::Error, "failed"))
            .is_err());
        assert!(receiver
            .recv()
            .await
            .unwrap_err()
            .to_string()
            .contains("poisoned"));
    }

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
