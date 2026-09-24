// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::proxy::RemoteComponent;
use async_trait::async_trait;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::{
    self as sdk,
    transport::{self, Failure},
    wire,
};
use drasi_lib::computation::v1::{
    ComponentControl, ControlDirection, ControlHandler, ControlNotification, PeerMessage,
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

struct Context {
    control: ComponentControl,
    active: AtomicBool,
}
#[derive(Default)]
pub(super) struct ControlBinding {
    context: Option<Arc<Context>>,
    pub(super) error: Option<Failure>,
}
impl ControlBinding {
    pub(super) fn revoke(&mut self) {
        if let Some(context) = self.context.take() {
            context.active.store(false, Ordering::Release);
        }
    }
}

pub(super) fn bind(component: &RemoteComponent, control: ComponentControl) {
    let context = Arc::new(Context {
        control,
        active: AtomicBool::new(true),
    });
    let table = abi::Control {
        header: abi::Header::new::<abi::Control>(),
        context: Arc::as_ptr(&context).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
        send: Some(send),
    };
    let result = component.bind(&table);
    let mut binding = component
        .control
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    binding.revoke();
    binding.error = result.err();
    if binding.error.is_some() {
        context.active.store(false, Ordering::Release);
    }
    binding.context = Some(context);
}
unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<Context>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<Context>()) });
}
unsafe extern "C" fn send(context: *mut c_void, input: abi::BorrowedBytes) -> abi::Status {
    transport::status_boundary(|| {
        let context = unsafe { &*context.cast::<Context>() };
        if !context.active.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        let request: sdk::SendControl =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_METADATA_BYTES)? })
                .map_err(Failure::from)?;
        let notification = incoming(request.notification);
        let result = match request.target {
            sdk::ControlTarget::Upstream => context.control.notify_upstream(notification),
            sdk::ControlTarget::Downstream => context.control.notify_downstream(notification),
            sdk::ControlTarget::Neighbor(neighbor) => {
                context.control.notify_neighbor(&neighbor, notification)
            }
        };
        result.map_err(anyhow::Error::from).map_err(Failure::from)
    })
}
fn incoming(notification: sdk::ControlNotification) -> ControlNotification {
    match notification {
        sdk::ControlNotification::Ready => ControlNotification::Ready,
        sdk::ControlNotification::NotReady => ControlNotification::NotReady,
        sdk::ControlNotification::Available => ControlNotification::Available,
        sdk::ControlNotification::Unavailable { reason } => {
            ControlNotification::Unavailable { reason }
        }
        sdk::ControlNotification::Custom { kind, payload } => {
            ControlNotification::Custom { kind, payload }
        }
    }
}
fn outgoing(notification: ControlNotification) -> sdk::ControlNotification {
    match notification {
        ControlNotification::Ready => sdk::ControlNotification::Ready,
        ControlNotification::NotReady => sdk::ControlNotification::NotReady,
        ControlNotification::Available => sdk::ControlNotification::Available,
        ControlNotification::Unavailable { reason } => {
            sdk::ControlNotification::Unavailable { reason }
        }
        ControlNotification::Custom { kind, payload } => {
            sdk::ControlNotification::Custom { kind, payload }
        }
    }
}
pub(super) struct Handler(pub(super) Arc<RemoteComponent>);
#[async_trait]
impl ControlHandler for Handler {
    async fn on_message(
        &self,
        message: PeerMessage,
        _control: ComponentControl,
    ) -> anyhow::Result<()> {
        let message = sdk::ControlMessage {
            from: message.from,
            generation: message.generation.0,
            direction: match message.direction {
                ControlDirection::Upstream => sdk::ControlDirection::Upstream,
                ControlDirection::Downstream => sdk::ControlDirection::Downstream,
            },
            notification: outgoing(message.notification),
        };
        self.0
            .unit(abi::operation::CONTROL, &wire::encode(&message)?)
            .await
    }
}
