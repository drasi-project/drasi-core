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
    binding: StdMutex<Binding>,
    activation_changed: Notify,
    failure: StdMutex<Option<String>>,
    failed: Notify,
}

#[derive(Default)]
struct Binding {
    generation: u64,
    opening: bool,
    requests: Option<mpsc::Sender<Request>>,
    failure: Option<ActivationFailure>,
}

enum ActivationFailure {
    Worker(String),
    Graph(Arc<GraphError>),
}

impl ActivationFailure {
    fn error(&self) -> PipeError {
        match self {
            Self::Worker(reason) => backend(format!("source worker failed: {reason}")),
            Self::Graph(cause) => PipeError::Backend(
                GraphError::Reported {
                    cause: cause.clone(),
                }
                .into(),
            ),
        }
    }
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
            binding: StdMutex::new(Binding::default()),
            activation_changed: Notify::new(),
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
            .binding
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?
            .requests
            .is_some())
    }

    /// Wait for the graph's actual admission mailbox after requesting startup.
    ///
    /// Successful graph/component startup alone does not imply that its processing
    /// task has bound this mailbox. This wait observes the current activation,
    /// including an already bound mailbox, without publishing or acknowledging data.
    /// Stop, cancellation or a superseding activation returns [`PipeError::Closed`];
    /// reported failures retain their cause. Before activation is scheduled this
    /// returns `Closed`, rather than waiting for an unspecified future start.
    ///
    /// Cancellation of this future has no side effects. Callers own their deadline.
    /// Success is not a lease against concurrent stop, a capacity reservation, or
    /// evidence of downstream processing. Subsequent operations must still handle
    /// their original errors, including uncertain acceptance.
    pub async fn wait_active(&self) -> Result<(), PipeError> {
        let mut generation = None;
        loop {
            let activated = self.activation_changed.notified();
            let channel_changed = self.channel.changed.notified();
            tokio::pin!(activated, channel_changed);
            activated.as_mut().enable();
            channel_changed.as_mut().enable();
            self.channel.check()?;
            {
                let binding = self
                    .binding
                    .lock()
                    .map_err(|_| backend("source admission binding poisoned"))?;
                if *generation.get_or_insert(binding.generation) != binding.generation {
                    return Err(PipeError::Closed);
                }
                if let Some(failure) = &binding.failure {
                    return Err(failure.error());
                }
                if !binding.opening {
                    return Err(PipeError::Closed);
                }
                if binding
                    .requests
                    .as_ref()
                    .is_some_and(|sender| !sender.is_closed())
                {
                    return Ok(());
                }
            }
            tokio::select! {
                _ = &mut activated => {},
                result = self.channel.wait_changed(channel_changed) => result?,
            }
        }
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
            self.binding
                .lock()
                .map_err(|_| backend("source admission binding poisoned"))?
                .failure = Some(ActivationFailure::Worker(reason.to_owned()));
            self.activation_changed.notify_waiters();
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
            .binding
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?
            .requests
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

    pub(crate) fn begin_activation(self: &Arc<Self>) -> Result<AdmissionActivation, PipeError> {
        let failure = self
            .failure
            .lock()
            .map_err(|_| backend("source failure observation poisoned"))?;
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?;
        self.channel.check()?;
        if binding.opening || binding.requests.is_some() {
            return Err(backend(
                "source admission activation already has a graph owner",
            ));
        }
        binding.generation = binding
            .generation
            .checked_add(1)
            .ok_or_else(|| backend("source admission activation generation exhausted"))?;
        binding.opening = true;
        binding.failure = failure.clone().map(ActivationFailure::Worker);
        self.activation_changed.notify_waiters();
        Ok(AdmissionActivation {
            service: self.clone(),
            generation: binding.generation,
        })
    }

    pub(crate) fn activation_failed(&self, cause: Arc<GraphError>) {
        let mut binding = self.binding.lock().unwrap_or_else(|error| {
            log::error!("source admission binding poisoned while reporting failure");
            error.into_inner()
        });
        if binding.failure.is_none() {
            binding.failure = Some(ActivationFailure::Graph(cause));
        }
        self.activation_changed.notify_waiters();
    }

    pub(crate) fn bind(self: &Arc<Self>) -> Result<AdmissionLease, PipeError> {
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| backend("source admission binding poisoned"))?;
        self.channel.check()?;
        if !binding.opening {
            return Err(PipeError::Closed);
        }
        if binding.requests.is_some() {
            return Err(backend("source admission already has a graph owner"));
        }
        self.channel
            .admission_bound
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| backend("QoS channel already has a graph admission owner"))?;
        let (send, receive) = mpsc::channel(PENDING_REQUESTS);
        binding.requests = Some(send);
        self.activation_changed.notify_waiters();
        Ok(AdmissionLease {
            service: self.clone(),
            generation: binding.generation,
            requests: receive,
        })
    }

    fn end_activation(&self, generation: u64) {
        let mut binding = self.binding.lock().unwrap_or_else(|error| {
            log::error!("source admission binding poisoned during revocation");
            error.into_inner()
        });
        if binding.generation == generation {
            binding.requests.take();
            binding.opening = false;
            self.activation_changed.notify_waiters();
        }
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
    generation: u64,
    requests: mpsc::Receiver<Request>,
}

pub(crate) struct AdmissionActivation {
    service: Arc<SourceAdmission>,
    generation: u64,
}

impl Drop for AdmissionActivation {
    fn drop(&mut self) {
        self.service.end_activation(self.generation);
    }
}

impl AdmissionLease {
    pub(crate) async fn process(
        &mut self,
        quiesce: &mut tokio::sync::watch::Receiver<bool>,
        validate: impl FnOnce(&ChangeEnvelope) -> anyhow::Result<()> + Send,
    ) -> Result<Option<bool>, PipeError> {
        let request = tokio::select! {
            biased;
            _ = quiesce.wait_for(|paused| *paused) => return Ok(None),
            _ = self.service.failed.notified() => {
                let mut failure = self.service.failure.lock().map_err(|_| backend("source failure observation poisoned"))?;
                return match failure.take() {
                    Some(reason) => Err(backend(format!("source worker failed: {reason}"))),
                    None => Ok(Some(false)),
                };
            }
            request = self.requests.recv() => request.ok_or(PipeError::Closed)?,
        };
        if request.cancelled() {
            return Ok(Some(false));
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
        Ok(Some(accepted))
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
        self.service.end_activation(self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Arc<SourceAdmission> {
        let channel = QosChannel::volatile(QosChannelDefinition {
            stream: StreamId::try_new("source/out").expect("stream"),
            capacity: NonZeroUsize::new(1).expect("capacity"),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        })
        .expect("channel");
        Arc::new(SourceAdmission {
            identity: GraphProducerIdentity::volatile(
                "scope".into(),
                "graph".into(),
                ComponentId::try_new("source").expect("component"),
                channel.definition().stream.clone(),
            )
            .expect("identity"),
            channel,
            port: PortId::try_new("out").expect("port"),
            binding: StdMutex::new(Binding::default()),
            activation_changed: Notify::new(),
            failure: StdMutex::new(None),
            failed: Notify::new(),
        })
    }

    #[tokio::test]
    async fn activation_wait_covers_pending_and_late_subscribers_without_lost_wakes() {
        let service = service();
        assert!(matches!(
            service.wait_active().await,
            Err(PipeError::Closed)
        ));
        let _activation = service.begin_activation().expect("scheduled processing");
        let first = service.wait_active();
        let second = service.wait_active();
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert!(!service.is_active().expect("inactive mailbox"));
        let _lease = service.bind().expect("bind actual mailbox");
        first.await.expect("first waiter");
        second.await.expect("second waiter");
        service.wait_active().await.expect("late waiter");
        assert!(service.is_active().expect("active mailbox"));
        assert_eq!(
            service
                .channel()
                .progress()
                .await
                .expect("progress")
                .accepted,
            0
        );
    }

    #[tokio::test]
    async fn stopped_activation_cannot_be_satisfied_by_an_immediate_restart() {
        let service = service();
        let old_activation = service.begin_activation().expect("first activation");
        let old_wait = service.wait_active();
        tokio::pin!(old_wait);
        assert!(futures::poll!(&mut old_wait).is_pending());
        let lease = service.bind().expect("first mailbox");
        let old_sender = service
            .binding
            .lock()
            .expect("binding")
            .requests
            .clone()
            .expect("sender");
        drop(lease);
        let _next_activation = service.begin_activation().expect("new activation");
        let _next_lease = service.bind().expect("new mailbox");
        drop(old_activation);
        assert!(matches!(old_wait.await, Err(PipeError::Closed)));
        assert!(old_sender.is_closed());
        service
            .wait_active()
            .await
            .expect("new waiter targets new activation");
    }

    #[tokio::test]
    async fn cancelled_processing_before_binding_wakes_waiters_closed() {
        let service = service();
        let activation = service.begin_activation().expect("scheduled processing");
        let wait = service.wait_active();
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());
        drop(activation);
        assert!(matches!(wait.await, Err(PipeError::Closed)));
        assert!(matches!(service.bind(), Err(PipeError::Closed)));
        assert!(matches!(
            service.wait_active().await,
            Err(PipeError::Closed)
        ));
    }

    #[tokio::test]
    async fn cancelling_a_wait_does_not_cancel_activation() {
        let service = service();
        let _activation = service.begin_activation().expect("scheduled processing");
        {
            let wait = service.wait_active();
            tokio::pin!(wait);
            assert!(futures::poll!(&mut wait).is_pending());
        }
        let _lease = service.bind().expect("mailbox still binds");
        service.wait_active().await.expect("another waiter");
    }

    #[tokio::test]
    async fn startup_worker_failure_is_retained_for_activation_waiters() {
        let service = service();
        let _activation = service.begin_activation().expect("scheduled processing");
        let wait = service.wait_active();
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());
        service
            .report_failure("startup worker failed")
            .expect("report failure");
        let error = wait.await.expect_err("failure must not be readiness");
        assert!(error.to_string().contains("startup worker failed"));
        let _lease = service.bind().expect("graph still observes worker failure");
        assert!(service.wait_active().await.is_err());
    }

    #[tokio::test]
    async fn graph_failure_before_activation_retains_its_typed_cause() {
        for scheduled in [false, true] {
            let service = service();
            let activation =
                scheduled.then(|| service.begin_activation().expect("scheduled processing"));
            let wait = service.wait_active();
            tokio::pin!(wait);
            if scheduled {
                assert!(futures::poll!(&mut wait).is_pending());
            }
            let cause = Arc::new(GraphError::StaleGeneration);
            service.activation_failed(cause.clone());
            drop(activation);
            let PipeError::Backend(error) = wait.await.expect_err("failed start") else {
                panic!("expected retained backend cause");
            };
            let Some(GraphError::Reported { cause: actual }) = error.downcast_ref::<GraphError>()
            else {
                panic!("expected original typed graph cause");
            };
            assert!(Arc::ptr_eq(actual, &cause));
        }
    }

    #[tokio::test]
    async fn channel_shutdown_wakes_an_unbound_activation_waiter() {
        let service = service();
        let _activation = service.begin_activation().expect("scheduled processing");
        let wait = service.wait_active();
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());
        service
            .channel()
            .shutdown()
            .await
            .expect("shutdown channel");
        assert!(matches!(wait.await, Err(PipeError::Closed)));
    }

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
                binding: StdMutex::new(Binding::default()),
                activation_changed: Notify::new(),
                failure: StdMutex::new(None),
                failed: Notify::new(),
            })
        };
        let first = service();
        let second = service();
        let _first_activation = first.begin_activation().unwrap();
        let _second_activation = second.begin_activation().unwrap();
        let lease = first.bind().unwrap();
        let old = first
            .binding
            .lock()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()
            .clone();
        assert!(first.bind().is_err());
        assert!(second.bind().is_err());
        assert!(first.is_active().unwrap());
        assert!(!second.is_active().unwrap());
        drop(lease);
        let replacement = second.bind().unwrap();
        assert!(!first.is_active().unwrap());
        assert!(second.is_active().unwrap());
        assert!(matches!(first.wait_active().await, Err(PipeError::Closed)));
        second.wait_active().await.expect("replacement admission");
        assert!(old.is_closed());
        assert!(first.bind().is_err());
        drop(replacement);
        let _restarted = first.begin_activation().unwrap();
        let mut lease = first.bind().unwrap();
        let sender = first
            .binding
            .lock()
            .unwrap()
            .requests
            .as_ref()
            .unwrap()
            .clone();
        let permits = (0..PENDING_REQUESTS)
            .map(|_| sender.clone().try_reserve_owned().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            sender.try_reserve(),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        first.report_failure("first worker failure").unwrap();
        first.report_failure("later worker failure").unwrap();
        let (_pause, mut quiesce) = tokio::sync::watch::channel(false);
        let error = lease
            .process(&mut quiesce, |_| panic!("failure must not validate data"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("first worker failure"));
        drop(permits);
        drop(lease);
        first
            .report_failure("worker failed during startup")
            .unwrap();
        let _restarted = first.begin_activation().unwrap();
        assert!(first
            .wait_active()
            .await
            .expect_err("startup worker failure")
            .to_string()
            .contains("worker failed during startup"));
        let mut lease = first.bind().unwrap();
        let error = lease
            .process(&mut quiesce, |_| {
                panic!("startup failure must not validate data")
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker failed during startup"));
    }
}
