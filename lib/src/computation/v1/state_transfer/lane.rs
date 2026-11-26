// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A private point-to-point lane. Only control, replay, and the sealed file
//! capability enter the bounded queue; baseline records never do.

use super::file::{DigestBytes, FileTransfer, FileWriter};
use super::SnapshotBatchLimits;
use crate::computation::v1::{
    BinaryEnvelopeCodec, BootstrapMarker, BootstrapSessionEvent as Event, BootstrapSessionId,
    BoundedPipeConfig, ChangeEnvelope, ControlMessage, Delivery, EnqueueReceipt, EnvelopeMessage,
    EnvelopeReceiver, EnvelopeSender, OutputReadError, Pipe, PipeCapabilities, PipeControl,
    PipeError, PipeProvider, ProvidedPipe, Schema, SendFailure,
};
use async_trait::async_trait;
use futures::{stream::BoxStream, StreamExt};
use sha2::{Digest, Sha256};
use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{mpsc, watch};

type PipeResult<T> = std::result::Result<T, PipeError>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Open,
    Draining,
    Cancelled,
}

struct Control {
    status: watch::Sender<Status>,
    session: Mutex<Option<Sending>>,
    // One guard for the open write session and one per accepted queue item,
    // retained by the importer until it yields the original completion.
    work: Arc<()>,
}

impl Control {
    fn session(&self) -> MutexGuard<'_, Option<Sending>> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl PipeControl for Control {
    fn close(&self) {
        self.status.send_if_modified(|s| {
            if *s != Status::Open {
                return false;
            }
            *s = Status::Draining;
            true
        });
        self.session().take(); // No terminal frame: never publish a partial file.
    }

    fn cancel(&self) {
        self.status.send_replace(Status::Cancelled);
        self.session().take();
    }

    async fn is_idle(&self) -> PipeResult<bool> {
        Ok(Arc::strong_count(&self.work) == 1)
    }
}

struct Sending {
    id: BootstrapSessionId,
    phase: Phase,
    _work: Arc<()>,
}

enum Phase {
    Begin,
    Writing(Box<FileWriter>),
    Replay,
}

struct Packet {
    message: EnvelopeMessage,
    // Present only on the terminal envelope of a successful baseline.
    file: Option<Box<FileTransfer>>,
    _work: Arc<()>,
}

struct Sender {
    tx: mpsc::Sender<Packet>,
    control: Arc<Control>,
    serial: tokio::sync::Mutex<()>,
    binding: uuid::Uuid,
    quota: NonZeroU64,
    schema: Arc<Schema>,
}

fn backend(e: impl Into<anyhow::Error>) -> PipeError {
    PipeError::Backend(e.into())
}

// Record framing is not a derived event: keep the exact identity, lineage
// and ordered annotations, including on a provider-supplied envelope.
fn record_control(
    frame: &ChangeEnvelope,
    control: Option<ControlMessage>,
) -> PipeResult<ChangeEnvelope> {
    let system = frame.system().as_ref().clone().without_control();
    let mut entries: Vec<_> = frame.annotations().entries().collect();
    entries.reverse();
    Ok(ChangeEnvelope::restore(
        frame.id().clone(),
        frame.changes().clone(),
        match control {
            Some(tag) => system.with_control(tag),
            None => system,
        },
        frame.lineage().cloned(),
        frame.context_identity(),
        entries,
    )?)
}

/// The session ID and cut (watermarks or output cursor) are in the terminal
/// control. The unforgeable local binding instance fixes the schema and
/// generation, including for empty views without a data frame to validate.
fn scope(binding: uuid::Uuid, end: &ChangeEnvelope) -> PipeResult<DigestBytes> {
    let control = end
        .system()
        .control()
        .ok_or_else(|| backend(anyhow::anyhow!("missing bootstrap completion")))?;
    let mut hash = Sha256::new();
    hash.update(binding.as_bytes());
    hash.update((control.kind().len() as u64).to_be_bytes());
    hash.update(control.kind());
    hash.update(control.payload());
    Ok(hash.finalize().into())
}

impl Sender {
    fn active(&self, session: BootstrapSessionId) -> PipeResult<Sending> {
        self.control
            .session()
            .take_if(|s| s.id == session)
            .ok_or_else(|| {
                backend(anyhow::anyhow!(
                    "bootstrap frame outside its active session"
                ))
            })
    }

    async fn writer(&self) -> PipeResult<FileWriter> {
        let limits = SnapshotBatchLimits::default();
        let mut codec = BinaryEnvelopeCodec::new(limits.max_bytes);
        codec
            .register_schema(self.schema.clone())
            .map_err(backend)?;
        FileWriter::open(codec, limits, self.quota)
            .await
            .map_err(backend)
    }

    async fn accept(&self, message: &EnvelopeMessage) -> PipeResult<EnqueueReceipt> {
        let _serial = self.serial.lock().await;
        let _working = self.control.work.clone();
        let envelope = &message.envelope;
        let BootstrapMarker { session, event } = BootstrapMarker::of(envelope)?
            .ok_or_else(|| backend(anyhow::anyhow!("unframed envelope on bootstrap file lane")))?;
        let receipt = EnqueueReceipt::new(envelope.id().clone());
        let mut file = None;
        let mut forward = true;
        // Take ownership BEFORE asynchronous I/O. Dropping a partially written
        // append or a blocked seal discards the session, not just its last row.
        let failure = matches!(event, Event::Failed { .. } | Event::ReplayFailed(_));
        let next = match event {
            Event::Begin => {
                if self
                    .control
                    .session()
                    .as_ref()
                    .is_some_and(|s| s.id == session)
                {
                    return Err(backend(anyhow::anyhow!("duplicate bootstrap begin")));
                }
                Some(Sending {
                    id: session,
                    phase: Phase::Begin,
                    _work: self.control.work.clone(),
                })
            }
            Event::Failed { .. } | Event::ReplayFailed(_) => {
                // A stopped provider can fail before Begin, or after a write
                // was interrupted. Such a failure cannot retire a newer read.
                self.control.session().take_if(|s| s.id == session);
                None
            }
            // An intent is a request to commit ownership, not a snapshot row.
            // Spooling it behind the rows would deadlock a provider waiting
            // for that commit before it can finish the scan.
            Event::State { .. } => Some(self.active(session)?),
            Event::Record => {
                let mut active = self.active(session)?;
                if envelope.changes().schema() != self.schema.descriptor()
                    || !envelope.changes().is_append_only()
                {
                    return Err(backend(anyhow::anyhow!(
                        "invalid snapshot record for bound output"
                    )));
                }
                if !message.execution_context.is_empty() {
                    return Err(backend(anyhow::anyhow!(
                        "snapshot file lane does not transport transient execution capabilities"
                    )));
                }
                let writer = match &mut active.phase {
                    Phase::Begin => {
                        active.phase = Phase::Writing(Box::new(self.writer().await?));
                        let Phase::Writing(writer) = &mut active.phase else {
                            unreachable!("writer just installed")
                        };
                        writer
                    }
                    Phase::Writing(writer) => writer,
                    Phase::Replay => return Err(backend(anyhow::anyhow!("baseline after replay"))),
                };
                writer
                    .append(&record_control(envelope, None)?)
                    .await
                    .map_err(backend)?;
                forward = false;
                Some(active)
            }
            Event::Replay { .. } => {
                let mut active = self.active(session)?;
                if matches!(active.phase, Phase::Writing(_)) {
                    return Err(backend(anyhow::anyhow!("replay mixed with a snapshot")));
                }
                active.phase = Phase::Replay;
                Some(active)
            }
            Event::OutputComplete(_) | Event::Complete { .. } => {
                let writer = match self.active(session)?.phase {
                    Phase::Replay => None,
                    Phase::Writing(w) => Some(*w),
                    Phase::Begin => Some(self.writer().await?), // authoritative empty view
                };
                if let Some(writer) = writer {
                    file = Some(Box::new(
                        writer
                            .finish(scope(self.binding, envelope)?)
                            .await
                            .map_err(backend)?,
                    ));
                }
                None
            }
        };
        let permit = if forward {
            Some(self.tx.reserve().await.map_err(|_| PipeError::Closed)?)
        } else {
            None
        };
        // Linearize the receipt, writer replacement and queue publication with
        // close/cancel. Never return a nonacceptance for a published marker.
        let status = self.control.status.borrow();
        if *status != Status::Open {
            return Err(PipeError::Closed);
        }
        if !failure {
            *self.control.session() = next;
        }
        if let Some(permit) = permit {
            permit.send(Packet {
                message: message.clone(),
                file,
                _work: self.control.work.clone(),
            });
        }
        Ok(receipt)
    }
}

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send_message(
        &self,
        message: EnvelopeMessage,
    ) -> std::result::Result<EnqueueReceipt, SendFailure> {
        let mut closure = self.control.status.subscribe();
        let result = tokio::select! {
            biased;
            _ = closure.wait_for(|s| *s != Status::Open) => Err(PipeError::Closed),
            result = self.accept(&message) => result,
        };
        result.map_err(|e| SendFailure::new(message, e))
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.control.close();
    }
}

struct Import {
    stream: BoxStream<'static, std::result::Result<ChangeEnvelope, OutputReadError>>,
    session: BootstrapSessionId,
    end: Packet,
}

struct Receiver {
    rx: mpsc::Receiver<Packet>,
    control: Arc<Control>,
    binding: uuid::Uuid,
    // EnvelopeReceiver is Sync; only its exclusive receive method polls this stream.
    importing: Mutex<Option<Import>>,
}

impl Receiver {
    async fn next(&mut self) -> PipeResult<Option<Delivery>> {
        let importing = self.importing.get_mut().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(import) = importing {
                match import.stream.next().await {
                    Some(Ok(frame)) => {
                        let frame = record_control(
                            &frame,
                            Some(BootstrapMarker::new(import.session, Event::Record).to_control()?),
                        )?;
                        return Ok(Some(Delivery::new(frame, None)));
                    }
                    Some(Err(e)) => {
                        importing.take(); // Never forward its completion, even on another receive.
                        return Err(backend(e));
                    }
                    None => {
                        let end = importing.take().expect("active file import").end;
                        return Ok(Some(Delivery::from_message(end.message, None)));
                    }
                }
            }
            let Some(mut packet) = self.rx.recv().await else {
                return Ok(None);
            };
            let Some(file) = packet.file.take() else {
                return Ok(Some(Delivery::from_message(packet.message, None)));
            };
            let end = &packet.message.envelope;
            let session = BootstrapMarker::of(end)?
                .ok_or_else(|| backend(anyhow::anyhow!("missing file completion")))?
                .session;
            *importing = Some(Import {
                stream: file.import(scope(self.binding, end)?),
                session,
                end: packet,
            });
        }
    }
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> PipeResult<Option<Delivery>> {
        let mut status = self.control.status.subscribe();
        loop {
            match *status.borrow_and_update() {
                Status::Open => (),
                Status::Draining => self.rx.close(),
                Status::Cancelled => {
                    self.rx.close();
                    while self.rx.try_recv().is_ok() {}
                    self.importing
                        .get_mut()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    return Ok(None);
                }
            }
            tokio::select! {
                biased;
                _ = status.changed() => continue,
                result = self.next() => return result,
            }
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

struct Lane {
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
    capabilities: PipeCapabilities,
}

impl Pipe for Lane {
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }
    fn take_receiver(&mut self) -> PipeResult<Box<dyn EnvelopeReceiver>> {
        self.receiver
            .take()
            .map(|r| Box::new(r) as Box<_>)
            .ok_or(PipeError::ReceiverTaken)
    }
}

fn endpoints(
    capacity: usize,
    max_file_bytes: NonZeroU64,
    schema: Arc<Schema>,
) -> PipeResult<(Lane, Arc<Control>)> {
    let capabilities = BoundedPipeConfig { capacity }.capabilities()?;
    let (tx, rx) = mpsc::channel(capacity);
    let control = Arc::new(Control {
        status: watch::channel(Status::Open).0,
        session: Mutex::new(None),
        work: Arc::new(()),
    });
    let binding = uuid::Uuid::new_v4();
    Ok((
        Lane {
            sender: Arc::new(Sender {
                tx,
                control: control.clone(),
                serial: Default::default(),
                binding,
                quota: max_file_bytes,
                schema,
            }),
            receiver: Some(Receiver {
                rx,
                control: control.clone(),
                binding,
                importing: Mutex::new(None),
            }),
            capabilities,
        },
        control,
    ))
}

/// A fresh volatile bootstrap binding. Capacity counts controls, replay and
/// sealed references; `max_file_bytes` bounds each session's complete spool.
pub(in super::super) fn file_lane(
    capacity: usize,
    max_file_bytes: NonZeroU64,
    schema: Arc<Schema>,
) -> PipeResult<ProvidedPipe> {
    let (lane, control) = endpoints(capacity, max_file_bytes, schema)?;
    Ok(ProvidedPipe {
        pipe: Box::new(lane),
        control,
    })
}

#[cfg(test)]
mod tests;
