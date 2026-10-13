// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use drasi_lib::computation::v1::{
    emission_id, AdmissionReceipt, ChangeEnvelope, ComponentId, ContextEntry, ContextValue,
    GraphProducerIdentity, GraphProducerProgress, OutputEnvelope, PortId, ProducerSession,
    ProducerStatus, SourceAdmission, StreamId, SystemMetadata,
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_util::sync::CancellationToken;

use crate::{
    config::LinkContract,
    protocol::{
        read_message, write_message, AcceptanceBoundary, ControlRequest, ControlResponse,
        DataHello, DataRequest, DataResponse, Lane, LinkStatus, Receipt, Welcome, CONTROL_BYTES,
        PROTOCOL_VERSION,
    },
    transport::{deadline, BoxIo, Listener},
    BridgeError, ErrorCode, IngressCut, IngressDrainStatus, PortableEnvelopeCodec, Profile, Result,
};

pub(crate) struct Observations {
    pub generation: AtomicU64,
    pub connected_generation: AtomicU64,
    pub in_flight: AtomicBool,
    pub unknown_sequence: AtomicU64,
    pub accepted: AtomicU64,
    pub stopping: AtomicBool,
}

impl Default for Observations {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            connected_generation: AtomicU64::new(0),
            in_flight: AtomicBool::new(false),
            unknown_sequence: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        }
    }
}

impl Observations {
    pub(crate) fn snapshot(&self) -> LinkStatus {
        let generation = self.generation.load(Ordering::Acquire);
        LinkStatus {
            generation,
            connected: generation != 0
                && self.connected_generation.load(Ordering::Acquire) == generation,
            in_flight: self.in_flight.load(Ordering::Acquire),
            acceptance_unknown: self.unknown_sequence.load(Ordering::Acquire) != 0,
            accepted: self.accepted.load(Ordering::Acquire),
            stopping: self.stopping.load(Ordering::Acquire),
        }
    }
}

struct ConnectionObservation {
    observations: Arc<Observations>,
    generation: u64,
}

impl Drop for ConnectionObservation {
    fn drop(&mut self) {
        let _ = self.observations.connected_generation.compare_exchange(
            self.generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[derive(Default)]
struct VolatileState {
    head: u64,
    last: Option<([u8; 32], AdmissionReceipt)>,
}

pub(crate) struct Receiver {
    pub contract: LinkContract,
    pub codec: PortableEnvelopeCodec,
    pub cancel: CancellationToken,
    pub observations: Arc<Observations>,
    admission: Option<Arc<SourceAdmission>>,
    queue: Option<mpsc::Sender<OutputEnvelope>>,
    producer_identity: Option<GraphProducerIdentity>,
    volatile: Mutex<VolatileState>,
    session: Mutex<Option<ProducerSession>>,
    hello_gate: Mutex<()>,
    generation: watch::Sender<u64>,
    receiver_epoch: uuid::Uuid,
    sealed: AtomicBool,
    admission_gate: Mutex<()>,
    cut: Mutex<Option<IngressCut>>,
    pub source_dequeued: AtomicU64,
}

impl Receiver {
    pub(crate) fn new(
        contract: LinkContract,
        graph_id: &str,
        admission: Option<Arc<SourceAdmission>>,
        queue: Option<mpsc::Sender<OutputEnvelope>>,
        observations: Arc<Observations>,
    ) -> anyhow::Result<Arc<Self>> {
        let codec = PortableEnvelopeCodec::new(
            contract.schema,
            contract.namespace.clone(),
            contract.limits.clone(),
        )?;
        let producer_identity = (contract.profile == Profile::Volatile)
            .then(|| {
                GraphProducerIdentity::volatile(
                    contract.receiver_instance.clone(),
                    graph_id.into(),
                    ComponentId::try_new(contract.receiver_endpoint.as_str())?,
                    StreamId::try_new(contract.receiver_stream.as_str())?,
                )
            })
            .transpose()?;
        Ok(Arc::new(Self {
            contract,
            codec,
            cancel: CancellationToken::new(),
            observations,
            admission,
            queue,
            producer_identity,
            volatile: Mutex::new(VolatileState::default()),
            session: Mutex::new(None),
            hello_gate: Mutex::new(()),
            generation: watch::channel(0).0,
            receiver_epoch: uuid::Uuid::new_v4(),
            sealed: AtomicBool::new(false),
            admission_gate: Mutex::new(()),
            cut: Mutex::new(None),
            source_dequeued: AtomicU64::new(0),
        }))
    }

    pub(crate) async fn seal(&self) -> Result<IngressCut> {
        self.sealed.store(true, Ordering::Release);
        let _data = self.admission_gate.lock().await;
        let _hello = self.hello_gate.lock().await;
        if self.cancel.is_cancelled() {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "receiver stopped before resolving its cut",
            ));
        }
        let mut cut = self.cut.lock().await;
        if let Some(cut) = cut.as_ref() {
            return Ok(cut.clone());
        }
        let (session, sequence, position, required_subscribers) =
            if let Some(service) = &self.admission {
                let session = service
                    .register_producer(self.producer())
                    .await
                    .map_err(BridgeError::from)?;
                let status = service
                    .producer_status(&session)
                    .await
                    .map_err(BridgeError::from)?;
                let sequence = status.next_sequence.map_or(u64::MAX, |next| next - 1);
                let position = if sequence == 0 {
                    0
                } else {
                    service
                        .admission_receipt(&session, sequence)
                        .await
                        .map_err(BridgeError::from)?
                        .ok_or_else(|| {
                            BridgeError::unknown("accepted cut has no retained admission receipt")
                        })?
                        .position
                };
                let progress = service
                    .channel()
                    .progress()
                    .await
                    .map_err(BridgeError::from)?;
                if progress.accepted != position {
                    return Err(BridgeError::new(
                        ErrorCode::Conflict,
                        "journal includes another producer's history",
                    ));
                }
                let required = progress
                    .processed
                    .keys()
                    .filter(|id| !progress.retired.contains(id))
                    .cloned()
                    .collect();
                *self.session.lock().await = Some(session.clone());
                (Some(session), sequence, position, required)
            } else {
                let state = self.volatile.lock().await;
                (
                    self.session.lock().await.clone(),
                    state.head,
                    state.head,
                    Vec::new(),
                )
            };
        self.observations
            .unknown_sequence
            .store(0, Ordering::Release);
        let resolved = IngressCut {
            receiver_epoch: self.receiver_epoch,
            contract: self.contract.fingerprint(),
            boundary: AcceptanceBoundary::for_contract(&self.contract),
            session,
            client_sequence: sequence,
            position,
            required_subscribers,
        };
        *cut = Some(resolved.clone());
        Ok(resolved)
    }

    pub(crate) async fn drain_status(&self, expected: &IngressCut) -> Result<IngressDrainStatus> {
        if self.cancel.is_cancelled() || expected.receiver_epoch != self.receiver_epoch {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "receiver lifetime changed after seal",
            ));
        }
        if !self.sealed.load(Ordering::Acquire) || self.cut.lock().await.as_ref() != Some(expected)
        {
            return Err(BridgeError::new(
                ErrorCode::Conflict,
                "cut was not resolved by this sealed receiver",
            ));
        }
        let dequeued = self.source_dequeued.load(Ordering::Acquire);
        let queued = self
            .queue
            .as_ref()
            .map_or(0, |queue| queue.max_capacity() - queue.capacity());
        let drained = if let Some(service) = &self.admission {
            let progress = service
                .channel()
                .progress()
                .await
                .map_err(BridgeError::from)?;
            if progress.accepted != expected.position
                || expected
                    .required_subscribers
                    .iter()
                    .any(|id| progress.retired.contains(id))
            {
                return Err(BridgeError::new(
                    ErrorCode::Conflict,
                    "journal or required membership changed after seal",
                ));
            }
            expected.required_subscribers.iter().all(|id| {
                progress
                    .processed
                    .get(id)
                    .is_some_and(|position| *position >= expected.position)
            })
        } else {
            queued == 0 && dequeued == expected.client_sequence
        };
        Ok(IngressDrainStatus {
            cut: expected.clone(),
            drained,
            queued,
            source_dequeued_sequence: dequeued,
        })
    }

    pub(crate) async fn sealed_cut(&self) -> Result<Option<IngressCut>> {
        if self.cancel.is_cancelled() {
            return Err(BridgeError::new(ErrorCode::Closed, "receiver stopped"));
        }
        Ok(self.cut.lock().await.clone())
    }

    fn producer(&self) -> ComponentId {
        ComponentId::try_new(format!(
            "drasi-bridge-{:x}",
            Sha256::digest(self.contract.fingerprint())
        ))
        .expect("bounded bridge producer fingerprint")
    }

    fn check_generation(&self, generation: u64) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "bridge receiver is stopping",
            ));
        }
        if generation == 0 || generation != *self.generation.borrow() {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "connection generation has been replaced",
            ));
        }
        Ok(())
    }

    async fn check_session(&self, session: &ProducerSession) -> Result<()> {
        if self.session.lock().await.as_ref() != Some(session) {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "receiver session is unknown or expired",
            ));
        }
        Ok(())
    }

    async fn hello(
        &self,
        contract: LinkContract,
        expected: Option<ProducerSession>,
    ) -> Result<Welcome> {
        self.contract.check(&contract)?;
        let _gate = self.hello_gate.lock().await;
        if self.cancel.is_cancelled() {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "bridge receiver is stopping",
            ));
        }
        let producer = self.producer();
        let session =
            match (&self.admission, expected.as_ref()) {
                (_, Some(session)) if session.producer != producer => {
                    return Err(BridgeError::new(
                        ErrorCode::Incompatible,
                        "saved session belongs to another link contract",
                    ));
                }
                (Some(_), Some(session)) => session.clone(),
                (Some(service), None) => service
                    .register_producer(producer.clone())
                    .await
                    .map_err(|error| {
                        if matches!(
                            error,
                            drasi_lib::computation::v1::PipeError::CapacityExhausted
                        ) {
                            BridgeError::new(
                                ErrorCode::Incompatible,
                                "receiver journal is already bound to another bridge contract",
                            )
                        } else {
                            error.into()
                        }
                    })?,
                (None, _) => ProducerSession {
                    incarnation: self
                        .producer_identity
                        .as_ref()
                        .expect("volatile identity")
                        .incarnation(),
                    producer,
                    epoch: 1,
                },
            };
        if expected
            .as_ref()
            .is_some_and(|expected| expected != &session)
        {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "receiver incarnation changed",
            ));
        }
        let status = if let Some(service) = &self.admission {
            service
                .producer_status(&session)
                .await
                .map_err(BridgeError::from)?
        } else {
            let state = self.volatile.lock().await;
            ProducerStatus {
                session: session.clone(),
                next_sequence: state.head.checked_add(1),
                earliest_receipt: state.last.as_ref().map(|(_, receipt)| receipt.sequence),
            }
        };
        if expected.is_none() && status.next_sequence != Some(1) {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "sender mapping is missing but receiver history exists",
            ));
        }
        let generation = self
            .generation
            .borrow()
            .checked_add(1)
            .ok_or_else(|| BridgeError::invalid("connection generations exhausted"))?;
        *self.session.lock().await = Some(session.clone());
        self.generation.send_replace(generation);
        self.observations
            .generation
            .store(generation, Ordering::Release);
        Ok(Welcome {
            version: PROTOCOL_VERSION,
            generation,
            session,
            next_sequence: status.next_sequence,
            earliest_receipt: status.earliest_receipt,
            boundary: AcceptanceBoundary::for_contract(&self.contract),
            contract: self.contract.fingerprint(),
        })
    }

    fn receipt(&self, admission: AdmissionReceipt) -> Receipt {
        Receipt {
            admission,
            boundary: AcceptanceBoundary::for_contract(&self.contract),
            contract: self.contract.fingerprint(),
        }
    }

    async fn lookup(
        &self,
        generation: u64,
        session: &ProducerSession,
        sequence: u64,
    ) -> Result<Option<Receipt>> {
        self.check_generation(generation)?;
        self.check_session(session).await?;
        let receipt = if let Some(service) = &self.admission {
            service
                .admission_receipt(session, sequence)
                .await
                .map_err(BridgeError::from)?
        } else {
            let state = self.volatile.lock().await;
            match &state.last {
                Some((_, receipt)) if receipt.sequence == sequence => Some(receipt.clone()),
                _ if sequence > 0 && state.head.checked_add(1) == Some(sequence) => None,
                _ => {
                    return Err(BridgeError::new(
                        if sequence <= state.head && sequence > 0 {
                            ErrorCode::Expired
                        } else {
                            ErrorCode::Gap
                        },
                        "receipt is expired or client sequence has a gap",
                    ))
                }
            }
        };
        self.check_generation(generation)?;
        let _ = self.observations.unknown_sequence.compare_exchange(
            sequence,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(receipt.map(|receipt| self.receipt(receipt)))
    }

    async fn producer_status(
        &self,
        generation: u64,
        session: &ProducerSession,
    ) -> Result<ProducerStatus> {
        self.check_generation(generation)?;
        self.check_session(session).await?;
        let status = if let Some(service) = &self.admission {
            service
                .producer_status(session)
                .await
                .map_err(BridgeError::from)?
        } else {
            let state = self.volatile.lock().await;
            ProducerStatus {
                session: session.clone(),
                next_sequence: state.head.checked_add(1),
                earliest_receipt: state.last.as_ref().map(|(_, receipt)| receipt.sequence),
            }
        };
        self.check_generation(generation)?;
        Ok(status)
    }

    fn annotate(
        &self,
        mut envelope: ChangeEnvelope,
        request: &DataRequest,
    ) -> Result<ChangeEnvelope> {
        if envelope.system().stream().as_str() != self.contract.sender_stream {
            return Err(BridgeError::invalid(
                "incoming envelope does not own the declared sender stream",
            ));
        }
        // The original full context and logical producer marker remain intact.
        // The receiving source assigns a distinct local stream emission.
        let origin = serde_json::to_vec(&(
            1u32,
            &self.contract.link_id,
            self.contract.fingerprint(),
            &request.session,
            request.sequence,
        ))
        .map_err(|error| BridgeError::invalid(error.to_string()))?;
        envelope
            .append_annotation(
                ContextEntry::try_new(
                    ComponentId::try_new(self.contract.receiver_endpoint.as_str())
                        .map_err(|error| BridgeError::invalid(error.to_string()))?,
                    "drasi.bridge.origin.v1",
                    ContextValue::Bytes(Arc::from(origin)),
                )
                .map_err(|error| BridgeError::invalid(error.to_string()))?,
            )
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        Ok(envelope)
    }

    async fn admit(&self, request: &DataRequest) -> Result<Receipt> {
        let _data = self.admission_gate.lock().await;
        if self.sealed.load(Ordering::Acquire) {
            // Receipt lookup stays live; sealed ingress cannot admit fresh work.
            return Err(BridgeError::new(
                ErrorCode::Sealed,
                "ingress is sealed; resolve existing receipts over control",
            ));
        }
        self.check_generation(request.generation)?;
        self.check_session(&request.session).await?;
        if request.sequence == 0 || request.envelope.len() > self.contract.limits.max_envelope_bytes
        {
            return Err(BridgeError::invalid(
                "invalid client sequence or oversized envelope",
            ));
        }
        let input = self.codec.decode(&request.envelope)?;
        let input = self.annotate(input, request)?;
        let receipt = if let Some(service) = &self.admission {
            service
                .admit(&request.session, request.sequence, &input)
                .await
                .map_err(BridgeError::from)?
        } else {
            let digest: [u8; 32] = Sha256::digest(&request.envelope).into();
            let mut state = self.volatile.lock().await;
            if let Some((saved, receipt)) = &state.last {
                if receipt.sequence == request.sequence {
                    if saved != &digest {
                        return Err(BridgeError::new(
                            ErrorCode::Conflict,
                            "volatile retry changed content",
                        ));
                    }
                    return Ok(self.receipt(receipt.clone()));
                }
            }
            if state.head.checked_add(1) != Some(request.sequence) {
                return Err(BridgeError::new(
                    if request.sequence <= state.head {
                        ErrorCode::Expired
                    } else {
                        ErrorCode::Gap
                    },
                    "volatile client sequence is expired or has a gap",
                ));
            }
            let permit = self
                .queue
                .as_ref()
                .expect("volatile queue")
                .try_reserve()
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => {
                        BridgeError::new(ErrorCode::Busy, "volatile ingress queue is full")
                    }
                    mpsc::error::TrySendError::Closed(_) => {
                        BridgeError::new(ErrorCode::Closed, "volatile ingress queue is closed")
                    }
                })?;
            let stream = StreamId::try_new(self.contract.receiver_stream.as_str())
                .map_err(|error| BridgeError::invalid(error.to_string()))?;
            let mut system = SystemMetadata::new(stream.clone(), request.sequence);
            if let Some(timestamp) = input.system().timestamp() {
                system = system.with_timestamp(timestamp);
            }
            if let Some(position) = input.system().source_position() {
                system = system.with_source_position(position.clone());
            }
            let mut output = input.derive(
                emission_id(&stream, request.sequence)
                    .map_err(|error| BridgeError::invalid(error.to_string()))?,
                input.changes().clone(),
                system,
            );
            GraphProducerProgress::annotate(
                &mut output,
                self.producer_identity.as_ref().expect("volatile identity"),
                request.sequence,
            )
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
            let receipt = AdmissionReceipt {
                session: request.session.clone(),
                sequence: request.sequence,
                position: request.sequence,
            };
            permit.send(OutputEnvelope {
                port: PortId::try_new("out").expect("constant port"),
                envelope: output,
            });
            state.head = request.sequence;
            state.last = Some((digest, receipt.clone()));
            receipt
        };
        self.observations
            .accepted
            .store(receipt.position, Ordering::Release);
        // A stale connection may have committed; it must resolve the same session
        // and sequence, not acknowledge on a replacement generation's behalf.
        self.check_generation(request.generation)?;
        Ok(self.receipt(receipt))
    }

    async fn control_connection(self: Arc<Self>, mut io: BoxIo) -> Result<()> {
        let timeout = self.contract.limits.timeout();
        let request =
            deadline(timeout, read_message(&mut io, Lane::Control, CONTROL_BYTES)).await?;
        if let ControlRequest::Observe { contract } = request {
            return self.observe_connection(io, contract).await;
        }
        let ControlRequest::Hello {
            contract,
            expected_session,
        } = request
        else {
            return Err(BridgeError::invalid(
                "control connection must negotiate before requests",
            ));
        };
        let welcome = deadline(timeout, self.hello(contract, expected_session)).await;
        let _connection = welcome.as_ref().ok().map(|welcome| {
            self.observations
                .connected_generation
                .store(welcome.generation, Ordering::Release);
            ConnectionObservation {
                observations: self.observations.clone(),
                generation: welcome.generation,
            }
        });
        let generation = welcome
            .as_ref()
            .map(|welcome| welcome.generation)
            .map_err(Clone::clone);
        let response = match welcome {
            Ok(welcome) => ControlResponse::Welcome(welcome),
            Err(error) => ControlResponse::Failure(error),
        };
        deadline(
            timeout,
            write_message(&mut io, Lane::Control, &response, CONTROL_BYTES),
        )
        .await?;
        let generation = generation?;
        let mut changes = self.generation.subscribe();
        loop {
            self.check_generation(generation)?;
            let request = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                _ = changes.changed() => {
                    self.check_generation(generation)?;
                    continue;
                }
                request = read_message(&mut io, Lane::Control, CONTROL_BYTES) => request?,
            };
            let requested_generation = match &request {
                ControlRequest::Hello { .. } => generation,
                ControlRequest::Status { generation }
                | ControlRequest::ProducerStatus { generation, .. }
                | ControlRequest::Receipt { generation, .. } => *generation,
                _ => {
                    return Err(BridgeError::invalid(
                        "observer control requires a separate observing connection",
                    ))
                }
            };
            if requested_generation != generation {
                return Err(BridgeError::new(
                    ErrorCode::StaleGeneration,
                    "control request does not belong to this connection generation",
                ));
            }
            let response = match request {
                ControlRequest::Status { generation } => self
                    .check_generation(generation)
                    .map(|()| ControlResponse::Status(self.observations.snapshot())),
                ControlRequest::ProducerStatus {
                    generation,
                    session,
                } => deadline(timeout, self.producer_status(generation, &session))
                    .await
                    .map(ControlResponse::ProducerStatus),
                ControlRequest::Receipt {
                    generation,
                    session,
                    sequence,
                } => deadline(timeout, self.lookup(generation, &session, sequence))
                    .await
                    .map(ControlResponse::Receipt),
                ControlRequest::Hello { .. } => Err(BridgeError::invalid(
                    "duplicate handshake on control connection",
                )),
                _ => Err(BridgeError::invalid("unsupported sender control request")),
            }
            .unwrap_or_else(ControlResponse::Failure);
            deadline(
                timeout,
                write_message(&mut io, Lane::Control, &response, CONTROL_BYTES),
            )
            .await?;
        }
    }

    async fn observe_connection(&self, mut io: BoxIo, contract: LinkContract) -> Result<()> {
        let timeout = self.contract.limits.timeout();
        let response = match self.contract.check(&contract) {
            Ok(()) => ControlResponse::Observed {
                receiver_epoch: self.receiver_epoch,
            },
            Err(error) => ControlResponse::Failure(error),
        };
        deadline(
            timeout,
            write_message(&mut io, Lane::Control, &response, CONTROL_BYTES),
        )
        .await?;
        if matches!(response, ControlResponse::Failure(_)) {
            return Ok(());
        }
        loop {
            let request = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                request = deadline(timeout, read_message(&mut io, Lane::Control, CONTROL_BYTES)) => request?,
            };
            let response = match request {
                ControlRequest::Seal { receiver_epoch }
                    if receiver_epoch == self.receiver_epoch =>
                {
                    deadline(timeout, self.seal())
                        .await
                        .map(ControlResponse::Sealed)
                }
                ControlRequest::DrainStatus { cut } => deadline(timeout, self.drain_status(&cut))
                    .await
                    .map(ControlResponse::DrainStatus),
                _ => Err(BridgeError::new(
                    ErrorCode::StaleGeneration,
                    "observer request has another receiver lifetime or authority",
                )),
            }
            .unwrap_or_else(ControlResponse::Failure);
            deadline(
                timeout,
                write_message(&mut io, Lane::Control, &response, CONTROL_BYTES),
            )
            .await?;
        }
    }

    async fn data_connection(&self, mut io: BoxIo) -> Result<()> {
        let timeout = self.contract.limits.timeout();
        let hello: DataHello = deadline(
            timeout,
            read_message(&mut io, Lane::DataHello, CONTROL_BYTES),
        )
        .await?;
        let valid = async {
            self.check_generation(hello.generation)?;
            self.check_session(&hello.session).await?;
            if hello.version != PROTOCOL_VERSION || hello.contract != self.contract.fingerprint() {
                return Err(BridgeError::new(
                    ErrorCode::Incompatible,
                    "data connection contract differs",
                ));
            }
            Ok(())
        }
        .await;
        let response = match valid {
            Ok(()) => DataResponse::Ready {
                generation: hello.generation,
            },
            Err(error) => {
                deadline(
                    timeout,
                    write_message(
                        &mut io,
                        Lane::Receipt,
                        &DataResponse::Failure(error.clone()),
                        CONTROL_BYTES,
                    ),
                )
                .await?;
                return Err(error);
            }
        };
        deadline(
            timeout,
            write_message(&mut io, Lane::Receipt, &response, CONTROL_BYTES),
        )
        .await?;
        let mut changes = self.generation.subscribe();
        loop {
            let request: DataRequest = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                _ = changes.changed() => {
                    self.check_generation(hello.generation)?;
                    continue;
                }
                request = read_message(&mut io, Lane::Data, self.contract.limits.max_envelope_bytes + CONTROL_BYTES) => request?,
            };
            if request.generation != hello.generation || request.session != hello.session {
                return Err(BridgeError::new(
                    ErrorCode::StaleGeneration,
                    "data frame is not bound to its negotiated connection",
                ));
            }
            self.observations.in_flight.store(true, Ordering::Release);
            let result = deadline(timeout, self.admit(&request)).await;
            if result.as_ref().is_err_and(|error| {
                matches!(
                    error.code,
                    ErrorCode::AcceptanceUnknown | ErrorCode::Storage
                )
            }) {
                self.observations
                    .unknown_sequence
                    .store(request.sequence, Ordering::Release);
            }
            self.observations.in_flight.store(false, Ordering::Release);
            self.check_generation(hello.generation)?;
            let response = result
                .map(DataResponse::Receipt)
                .unwrap_or_else(DataResponse::Failure);
            deadline(
                timeout,
                write_message(&mut io, Lane::Receipt, &response, CONTROL_BYTES),
            )
            .await?;
        }
    }

    pub(crate) async fn listen_control(self: Arc<Self>, mut listener: Listener) -> Result<()> {
        let mut connections = FuturesUnordered::new();
        loop {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                Some(_) = connections.next(), if !connections.is_empty() => {}
                connection = listener.accept(), if connections.len() < 2 => {
                    match connection {
                        Ok(connection) => connections.push(self.clone().control_connection(connection)),
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
                    }
                }
            }
        }
    }

    pub(crate) async fn listen_data(self: Arc<Self>, mut listener: Listener) -> Result<()> {
        loop {
            let connection = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                connection = listener.accept() => connection,
            };
            let Ok(connection) = connection else {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            };
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Ok(()),
                _ = self.data_connection(connection) => {}
            }
        }
    }
}
