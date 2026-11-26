// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A joining subscriber's volatile suffix. Capture starts before its state
//! read; the first live receive drains through the *original* pipe, after
//! installation. Durable pipes keep their own acceptance and replay protocol.

use super::{
    BinaryEnvelopeCodec, BootstrapWindowLimits, Delivery, EnqueueReceipt, EnvelopeMessage,
    EnvelopeReceiver, EnvelopeSender, PipeControl, PipeError, PipeMetricsSnapshot, PortId,
    SendFailure, SinkBatching,
};
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Loading,
    Live,
    Gap,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
#[error("joining data edge {edge}: bootstrap live suffix exceeded its {count}-envelope or {bytes}-byte limit; rebind to capture a new cut")]
pub(super) struct WindowGap {
    edge: usize,
    count: usize,
    bytes: usize,
}

#[derive(Default)]
struct Held {
    queue: VecDeque<(EnvelopeMessage, usize)>,
    bytes: usize,
    closing: bool,
}

pub(super) struct Window {
    edge: usize,
    phase: watch::Sender<Phase>,
    held: Mutex<Held>,
    limits: BootstrapWindowLimits,
    changed: watch::Sender<()>,
    pipe: Arc<dyn PipeControl>,
}

impl Window {
    pub(super) fn new(
        edge: usize,
        limits: BootstrapWindowLimits,
        changed: watch::Sender<()>,
        pipe: Arc<dyn PipeControl>,
    ) -> Arc<Self> {
        Arc::new(Self {
            edge,
            phase: watch::channel(Phase::Loading).0,
            held: Mutex::default(),
            limits,
            changed,
            pipe,
        })
    }

    pub(super) fn endpoints(
        self: &Arc<Self>,
        sender: Arc<dyn EnvelopeSender>,
        receiver: Box<dyn EnvelopeReceiver>,
    ) -> (Arc<dyn EnvelopeSender>, Box<dyn EnvelopeReceiver>) {
        (
            Arc::new(Sender {
                window: self.clone(),
                inner: sender.clone(),
            }),
            Box::new(Receiver {
                window: self.clone(),
                sender,
                inner: receiver,
            }),
        )
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn gap(&self) -> WindowGap {
        WindowGap {
            edge: self.edge,
            count: self.limits.max_envelopes.get(),
            bytes: self.limits.max_bytes.get(),
        }
    }

    pub(super) fn check(&self) -> Result<(), WindowGap> {
        if *self.phase.borrow() == Phase::Gap {
            Err(self.gap())
        } else {
            Ok(())
        }
    }

    pub(super) fn arm(&self) {
        let held = self.held();
        let phase = *self.phase.borrow();
        if !held.closing && phase == Phase::Live {
            self.phase.send_replace(Phase::Loading);
        }
    }

    fn capture(&self, message: &EnvelopeMessage) -> Result<bool, PipeError> {
        let mut held = self.held();
        if held.closing {
            return Err(PipeError::Closed);
        }
        let phase = *self.phase.borrow();
        match phase {
            Phase::Live => return Ok(false),
            Phase::Cancelled => return Err(PipeError::Closed),
            Phase::Gap => return Err(PipeError::Backend(self.gap().into())),
            Phase::Loading => (),
        }
        let size = BinaryEnvelopeCodec::encoded_size(&message.envelope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        if held.queue.len() >= self.limits.max_envelopes.get()
            || size > self.limits.max_bytes.get().saturating_sub(held.bytes)
        {
            held.queue.clear();
            held.bytes = 0;
            self.phase.send_replace(Phase::Gap);
            self.changed.send_replace(());
            return Err(PipeError::Backend(self.gap().into()));
        }
        held.queue.push_back((message.clone(), size));
        held.bytes += size;
        Ok(true)
    }

    async fn flush(&self, sender: &dyn EnvelopeSender) -> Result<(), PipeError> {
        loop {
            let message = {
                let held = self.held();
                let phase = *self.phase.borrow();
                match phase {
                    Phase::Cancelled => return Err(PipeError::Closed),
                    Phase::Gap => return Err(PipeError::Backend(self.gap().into())),
                    Phase::Live => return Ok(()),
                    Phase::Loading => (),
                }
                let Some((head, _)) = held.queue.front() else {
                    self.phase.send_replace(Phase::Live);
                    if held.closing {
                        self.pipe.close();
                    }
                    return Ok(());
                };
                head.clone()
            };
            // Only wrap known cancellation-safe volatile senders. Keep the
            // head until acceptance; a cancelled receive can resume this flush.
            sender
                .send_message(message)
                .await
                .map_err(|failure| failure.error)?;
            let mut held = self.held();
            if let Some((_, size)) = held.queue.pop_front() {
                held.bytes -= size;
            }
        }
    }
}

#[async_trait]
impl PipeControl for Window {
    fn close(&self) {
        let mut held = self.held();
        held.closing = true;
        if held.queue.is_empty() {
            self.pipe.close();
        }
    }
    fn cancel(&self) {
        let mut held = self.held();
        *held = Held {
            closing: true,
            ..Default::default()
        };
        self.phase.send_replace(Phase::Cancelled);
        self.pipe.cancel();
    }
    async fn is_idle(&self) -> Result<bool, PipeError> {
        if !self.held().queue.is_empty() {
            return Ok(false);
        }
        self.pipe.is_idle().await
    }
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        self.pipe.metrics()
    }
    fn observations(&self) -> Option<Arc<crate::observability::QueueObservations>> {
        self.pipe.observations()
    }
}

struct Sender {
    window: Arc<Window>,
    inner: Arc<dyn EnvelopeSender>,
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.window.close();
    }
}

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send_message(&self, message: EnvelopeMessage) -> Result<EnqueueReceipt, SendFailure> {
        let mut phase = self.window.phase.subscribe();
        loop {
            match self.window.capture(&message) {
                Ok(true) => return Ok(EnqueueReceipt::new(message.envelope.id().clone())),
                Err(error) => return Err(SendFailure::new(message, error)),
                Ok(false) => (),
            }
            tokio::select! {
                biased;
                _ = phase.wait_for(|p| *p != Phase::Live) => continue,
                result = self.inner.send_message(message.clone()) => return result,
            }
        }
    }
}

struct Receiver {
    window: Arc<Window>,
    sender: Arc<dyn EnvelopeSender>,
    inner: Box<dyn EnvelopeReceiver>,
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        let phase = *self.window.phase.borrow();
        if phase == Phase::Live {
            return self.inner.receive().await;
        }
        let mut changes = self.window.phase.subscribe();
        let receive = self.inner.receive();
        tokio::pin!(receive);
        tokio::select! {
            biased;
            _ = changes.wait_for(|p| matches!(p, Phase::Gap | Phase::Cancelled)) => {
                self.window.check().map_err(|error| PipeError::Backend(error.into()))?;
                return Err(PipeError::Closed);
            }
            result = self.window.flush(self.sender.as_ref()) => result?,
            result = &mut receive => return result,
        }
        receive.await
    }

    fn supports_batch_receive(&self) -> bool {
        *self.window.phase.borrow() == Phase::Live && self.inner.supports_batch_receive()
    }
    async fn receive_batch(
        &mut self,
        limits: SinkBatching,
        port: &PortId,
    ) -> Result<Vec<Delivery>, PipeError> {
        self.inner.receive_batch(limits, port).await
    }
    fn take_pending_batch(&mut self) -> Result<Vec<Delivery>, PipeError> {
        self.inner.take_pending_batch()
    }
}
