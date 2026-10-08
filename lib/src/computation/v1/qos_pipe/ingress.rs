// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use tokio::sync::{mpsc, oneshot, Notify};

const PENDING_REQUESTS: usize = 16;

type Reply<T> = oneshot::Sender<Result<T, PipeError>>;

enum Request {
    Admit {
        session: ProducerSession,
        sequence: u64,
        input: ChangeEnvelope,
        reply: Reply<AdmissionReceipt>,
    },
    Register(ComponentId, Reply<ProducerSession>),
    Retire(ProducerSession, Reply<()>),
    Status(ProducerSession, Reply<ProducerStatus>),
    Receipt(ProducerSession, u64, Reply<Option<AdmissionReceipt>>),
}

impl Request {
    fn cancelled(&self) -> bool {
        match self {
            Self::Admit { reply, .. } => reply.is_closed(),
            Self::Register(_, reply) => reply.is_closed(),
            Self::Retire(_, reply) => reply.is_closed(),
            Self::Status(_, reply) => reply.is_closed(),
            Self::Receipt(_, _, reply) => reply.is_closed(),
        }
    }

    fn reject(self) {
        match self {
            Self::Admit { reply, .. } => {
                let _ = reply.send(Err(PipeError::Closed));
            }
            Self::Register(_, reply) => {
                let _ = reply.send(Err(PipeError::Closed));
            }
            Self::Retire(_, reply) => {
                let _ = reply.send(Err(PipeError::Closed));
            }
            Self::Status(_, reply) => {
                let _ = reply.send(Err(PipeError::Closed));
            }
            Self::Receipt(_, _, reply) => {
                let _ = reply.send(Err(PipeError::Closed));
            }
        }
    }
}

/// A bounded, graph-driven publication handle, not another journal or worker.
/// A source exposing this handle delegates its data publication to the graph;
/// its ordinary `next` hook is not called. Every output must use this channel.
pub struct SourceAdmission {
    channel: Arc<QosChannel>,
    port: PortId,
    identity: GraphProducerIdentity,
    requests: StdMutex<Option<mpsc::Sender<Request>>>,
    failure: StdMutex<Option<String>>,
    failed: Notify,
}

impl SourceAdmission {
    pub async fn new(channel: Arc<QosChannel>, port: PortId) -> Result<Arc<Self>, PipeError> {
        let state = channel.state.lock().await;
        channel.check()?;
        let identity = state
            .metadata
            .admission
            .as_ref()
            .ok_or(AdmissionRejection::NotEnabled)?
            .identity()
            .clone();
        drop(state);
        Ok(Arc::new(Self {
            channel,
            port,
            identity,
            requests: StdMutex::new(None),
            failure: StdMutex::new(None),
            failed: Notify::new(),
        }))
    }

    pub fn channel(&self) -> &Arc<QosChannel> {
        &self.channel
    }

    pub fn port(&self) -> &PortId {
        &self.port
    }

    pub fn component_id(&self) -> &ComponentId {
        self.identity.component_id()
    }

    pub fn identity(&self) -> &GraphProducerIdentity {
        &self.identity
    }

    pub fn is_active(&self) -> Result<bool, PipeError> {
        self.channel.check()?;
        Ok(self
            .requests
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?
            .is_some())
    }

    /// Preserve the first listener/worker failure independently of admission
    /// capacity. The graph observes it even though it does not call `next`.
    pub fn report_failure(&self, reason: &str) -> Result<(), PipeError> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err(backend("source failure must contain 1..=4096 UTF-8 bytes"));
        }
        let mut failure = self
            .failure
            .lock()
            .map_err(|_| backend("source failure observation poisoned"))?;
        if failure.is_none() {
            *failure = Some(reason.to_owned());
            self.failed.notify_one();
        }
        Ok(())
    }

    /// Success follows graph validation and the outgoing QoS commit. A dropped
    /// response does not prove nonacceptance; resolve against the saved receipt.
    pub async fn admit(
        &self,
        session: &ProducerSession,
        sequence: u64,
        input: &ChangeEnvelope,
    ) -> Result<AdmissionReceipt, PipeError> {
        self.submit(
            |reply| {
                self.channel
                    .storage()?
                    .ok_or(AdmissionRejection::NotEnabled)?
                    .codec
                    .encode(input)
                    .map_err(|error| PipeError::Backend(error.into()))?;
                Ok(Request::Admit {
                    session: session.clone(),
                    sequence,
                    input: input.clone(),
                    reply,
                })
            },
            || PipeError::AcceptanceUnknown {
                source: anyhow::anyhow!("graph admission ended before its acceptance response"),
            },
        )
        .await
    }

    pub async fn register_producer(
        &self,
        producer: ComponentId,
    ) -> Result<ProducerSession, PipeError> {
        self.submit(
            |reply| Ok(Request::Register(producer, reply)),
            metadata_unknown,
        )
        .await
    }

    pub async fn retire_producer(&self, session: &ProducerSession) -> Result<(), PipeError> {
        self.submit(
            |reply| Ok(Request::Retire(session.clone(), reply)),
            metadata_unknown,
        )
        .await
    }

    pub async fn producer_status(
        &self,
        session: &ProducerSession,
    ) -> Result<ProducerStatus, PipeError> {
        self.submit(
            |reply| Ok(Request::Status(session.clone(), reply)),
            || PipeError::Closed,
        )
        .await
    }

    pub async fn admission_receipt(
        &self,
        session: &ProducerSession,
        sequence: u64,
    ) -> Result<Option<AdmissionReceipt>, PipeError> {
        self.submit(
            |reply| Ok(Request::Receipt(session.clone(), sequence, reply)),
            || PipeError::Closed,
        )
        .await
    }

    async fn submit<T>(
        &self,
        request: impl FnOnce(Reply<T>) -> Result<Request, PipeError>,
        lost_response: impl FnOnce() -> PipeError,
    ) -> Result<T, PipeError> {
        self.channel.check()?;
        let sender = self
            .requests
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?
            .clone()
            .ok_or(PipeError::Closed)?;
        let permit = sender.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => AdmissionRejection::Busy.into(),
            mpsc::error::TrySendError::Closed(_) => PipeError::Closed,
        })?;
        let (reply, receive) = oneshot::channel();
        permit.send(request(reply)?);
        receive.await.map_err(|_| lost_response())?
    }

    pub(crate) fn bind(self: &Arc<Self>) -> Result<AdmissionLease, PipeError> {
        let mut binding = self
            .requests
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?;
        self.channel.check()?;
        if binding.is_some() {
            return Err(backend("source admission already has a graph owner"));
        }
        self.channel
            .admission_bound
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend("QoS channel already has a graph admission owner"))?;
        let (send, receive) = mpsc::channel(PENDING_REQUESTS);
        *binding = Some(send);
        Ok(AdmissionLease {
            service: self.clone(),
            requests: receive,
        })
    }
}

impl std::fmt::Debug for SourceAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SourceAdmission")
            .field("identity", &self.identity)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

fn metadata_unknown() -> PipeError {
    PipeError::AcknowledgementUnknown {
        source: anyhow::anyhow!("graph admission ended before its producer metadata response"),
    }
}

pub(crate) struct AdmissionLease {
    service: Arc<SourceAdmission>,
    requests: mpsc::Receiver<Request>,
}

impl AdmissionLease {
    pub(crate) async fn process(
        &mut self,
        validate: impl FnOnce(&ChangeEnvelope) -> anyhow::Result<()> + Send,
    ) -> Result<bool, PipeError> {
        let request = tokio::select! {
            biased;
            _ = self.service.failed.notified() => {
                let mut failure = self.service.failure.lock().map_err(|_| backend("source failure observation poisoned"))?;
                return match failure.take() {
                    Some(reason) => Err(backend(format!("source worker failed: {reason}"))),
                    None => Ok(false),
                };
            }
            request = self.requests.recv() => request.ok_or(PipeError::Closed)?,
        };
        if request.cancelled() {
            return Ok(false);
        }
        let channel = &self.service.channel;
        let mut accepted = false;
        match request {
            Request::Admit {
                session,
                sequence,
                input,
                reply,
            } => {
                let result = channel
                    .admit_validated(&session, sequence, &input, validate)
                    .await;
                accepted = result.is_ok();
                let _ = reply.send(result);
            }
            Request::Register(producer, reply) => {
                let _ = reply.send(channel.register_producer(producer).await);
            }
            Request::Retire(session, reply) => {
                let _ = reply.send(channel.retire_producer(&session).await);
            }
            Request::Status(session, reply) => {
                let _ = reply.send(channel.producer_status(&session).await);
            }
            Request::Receipt(session, sequence, reply) => {
                let _ = reply.send(channel.admission_receipt(&session, sequence).await);
            }
        }
        self.service.channel.check()?;
        Ok(accepted)
    }
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        self.requests.close();
        while let Ok(request) = self.requests.try_recv() {
            request.reject();
        }
        self.service
            .channel
            .admission_bound
            .store(false, Ordering::Release);
        match self.service.requests.lock() {
            Ok(mut binding) => {
                binding.take();
            }
            Err(error) => {
                log::error!("source admission binding poisoned during revocation");
                error.into_inner().take();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn binding_is_channel_exclusive_and_old_mailboxes_stay_closed() {
        let channel = QosChannel::volatile(QosChannelDefinition {
            stream: StreamId::try_new("source/out").unwrap(),
            capacity: NonZeroUsize::new(1).unwrap(),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        })
        .unwrap();
        let service = || {
            Arc::new(SourceAdmission {
                channel: channel.clone(),
                port: PortId::try_new("out").unwrap(),
                identity: GraphProducerIdentity::volatile(
                    "scope".into(),
                    "graph".into(),
                    ComponentId::try_new("source").unwrap(),
                    channel.definition().stream.clone(),
                )
                .unwrap(),
                requests: StdMutex::new(None),
                failure: StdMutex::new(None),
                failed: Notify::new(),
            })
        };
        let first = service();
        let second = service();
        let lease = first.bind().unwrap();
        let old = first.requests.lock().unwrap().as_ref().unwrap().clone();
        assert!(first.bind().is_err());
        assert!(second.bind().is_err());
        assert!(first.is_active().unwrap());
        assert!(!second.is_active().unwrap());
        drop(lease);
        let replacement = second.bind().unwrap();
        assert!(!first.is_active().unwrap());
        assert!(second.is_active().unwrap());
        assert!(old.is_closed());
        assert!(first.bind().is_err());
        drop(replacement);
        let mut lease = first.bind().unwrap();
        let sender = first.requests.lock().unwrap().as_ref().unwrap().clone();
        let permits = (0..PENDING_REQUESTS)
            .map(|_| sender.clone().try_reserve_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            sender.try_reserve(),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        first.report_failure("first worker failure").unwrap();
        first.report_failure("later worker failure").unwrap();
        let error = lease
            .process(|_| panic!("failure must not validate data"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("first worker failure"));
        drop(permits);
        drop(lease);
        first
            .report_failure("worker failed during startup")
            .unwrap();
        let mut lease = first.bind().unwrap();
        let error = lease
            .process(|_| panic!("startup failure must not validate data"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker failed during startup"));
    }
}
