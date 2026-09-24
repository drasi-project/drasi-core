// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    abi,
    transport::{checked_table, take_status, Failure},
    wire,
};
use async_trait::async_trait;
use drasi_lib::computation::v1::ComponentId;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlDirection {
    Upstream,
    Downstream,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlTarget {
    Upstream,
    Downstream,
    Neighbor(ComponentId),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlNotification {
    Ready,
    NotReady,
    Available,
    Unavailable {
        reason: String,
    },
    Custom {
        kind: String,
        payload: serde_json::Value,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlMessage {
    pub from: ComponentId,
    pub generation: u64,
    pub direction: ControlDirection,
    pub notification: ControlNotification,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendControl {
    pub target: ControlTarget,
    pub notification: ControlNotification,
}

/// Shared handler polled independently of the component's mutable data call.
/// Generation/neighbor validation remains exclusively host-owned.
#[async_trait]
pub trait NativeControlHandler: Send + Sync {
    async fn on_message(
        &self,
        message: ControlMessage,
        control: ControlSender,
    ) -> anyhow::Result<()>;
}

struct RetainedControl(abi::Control);
unsafe impl Send for RetainedControl {}
unsafe impl Sync for RetainedControl {}
impl Drop for RetainedControl {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

/// Constructed before activation and bound by the host. Clones do not keep a graph
/// alive. Queue-full/stale/closed failures are explicit and never wait for data.
#[derive(Clone, Default)]
pub struct ControlSender(Arc<Mutex<Option<Arc<RetainedControl>>>>);
impl ControlSender {
    /// # Safety
    /// `control` is a valid borrowed callback table. Its retain/release functions
    /// keep the producing-side context callable even after component cancellation.
    pub(crate) unsafe fn bind(&self, control: *const abi::Control) -> Result<(), Failure> {
        let table = *unsafe { checked_table(control)? };
        if table.context.is_null()
            || table.retain.is_none()
            || table.release.is_none()
            || table.send.is_none()
        {
            return Err(Failure::protocol(
                "incomplete native control callback table",
            ));
        }
        unsafe { table.retain.expect("validated")(table.context) };
        let next = Arc::new(RetainedControl(table));
        let old = self
            .0
            .lock()
            .map_err(|_| Failure::failed("native control binding poisoned"))?
            .replace(next);
        drop(old);
        Ok(())
    }
    pub fn send(
        &self,
        target: ControlTarget,
        notification: ControlNotification,
    ) -> anyhow::Result<()> {
        let binding = self
            .0
            .lock()
            .map_err(|_| Failure::failed("native control binding poisoned"))?
            .clone()
            .ok_or_else(Failure::closed)?;
        let bytes = wire::encode(&SendControl {
            target,
            notification,
        })?;
        anyhow::ensure!(
            bytes.len() <= abi::MAX_METADATA_BYTES,
            "native control message too large"
        );
        unsafe {
            take_status(binding.0.send.expect("validated")(
                binding.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))?
        };
        Ok(())
    }
    pub fn ready(&self) -> anyhow::Result<()> {
        self.send(ControlTarget::Upstream, ControlNotification::Ready)
    }
    pub fn not_ready(&self) -> anyhow::Result<()> {
        self.send(ControlTarget::Upstream, ControlNotification::NotReady)
    }
}
