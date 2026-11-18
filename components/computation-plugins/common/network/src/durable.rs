// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Durable admission through the host's QoS admission service.
//!
//! Two producer models share one channel:
//!
//! * **Session protocol** (`/admission/v1`, `AdmissionService`): the client
//!   registers its own producer and supplies consecutive sequences. Retrying
//!   the same session and sequence is idempotent.
//! * **Server-sequenced** (standard routes, webhooks, `SubmitEvent`,
//!   `StreamEvents`): the source registers a producer named with its own
//!   component UUID and assigns sequences itself. A success response is sent
//!   only after the event is committed. Delivery is at-least-once: when a
//!   response is lost or a write's outcome is uncertain, a client retry is
//!   admitted as a new event. After any uncertain or out-of-sequence result
//!   the source re-reads the producer's committed head before admitting more.
//!
//! Clients using the session protocol must not register the source's own
//! component UUID as their producer name.
//!
//! Both models admit with back-pressure ([`NativeAdmission::admit_wait`]): a
//! momentarily full graph mailbox or journal makes the request wait, bounded
//! by `timeoutMs`, instead of failing part-way through a batch.
//!
//! **Batches are not atomic.** Each event is committed on its own, so a batch
//! that fails part-way leaves a committed prefix. Every response states that
//! prefix exactly: session-protocol responses list a receipt per committed
//! event, and server-sequenced responses carry the committed event count.
//! When the failure is uncertain, only the event after that prefix may also
//! have been committed. A batch stops early only on Pause (at an event
//! boundary), an aborting Stop, the request deadline or a definite rejection.
//!
//! A draining Stop refuses new admissions but lets those in flight finish, so
//! their clients receive the real outcome; only an aborting Stop interrupts
//! them with an uncertain result.

use crate::ingress::{AdmitFailure, Ingress, IngressError, IngressErrorKind};
use drasi_computation_contracts::{
    AdmissionReceipt, ChangeEnvelope, GraphChangeCodec, ProducerSession,
};
use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind, NativeAdmission};
use drasi_core_types::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::future::Future;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchOutcome {
    pub receipts: Vec<AdmissionReceipt>,
    pub error: Option<AdmissionError>,
}

pub fn invalid(error: impl std::fmt::Display) -> AdmissionError {
    AdmissionError {
        kind: AdmissionErrorKind::Invalid,
        message: error.to_string(),
    }
}

/// The source's own producer session and its next sequence.
pub(crate) struct ImplicitSession {
    session: ProducerSession,
    next: u64,
}

fn paused_admission() -> AdmissionError {
    AdmissionError {
        kind: AdmissionErrorKind::Busy,
        message: "source paused; retry after resume".into(),
    }
}

fn admission_error(error: IngressError) -> AdmissionError {
    let kind = match error.kind {
        IngressErrorKind::Stopped => AdmissionErrorKind::Closed,
        IngressErrorKind::Paused | IngressErrorKind::Overloaded => AdmissionErrorKind::Busy,
        _ => AdmissionErrorKind::Invalid,
    };
    AdmissionError {
        kind,
        message: error.message,
    }
}

/// Maps a host admission failure for a server-sequenced write.
fn implicit_error(error: AdmissionError) -> IngressError {
    use AdmissionErrorKind::*;
    let kind = match error.kind {
        Busy | CapacityExhausted => IngressErrorKind::Overloaded,
        Closed => IngressErrorKind::Stopped,
        AcceptanceUnknown | MetadataUnknown | Sequence | SessionExpired => {
            IngressErrorKind::Uncertain
        }
        Invalid | IdentifierTooLong | PayloadConflict => IngressErrorKind::Invalid,
        NotEnabled | ReceiptExpired | PendingObligations => IngressErrorKind::Failed,
    };
    IngressError::new(kind, error.message)
}

/// Whether the producer head must be re-read before admitting again.
fn requires_resync(kind: AdmissionErrorKind) -> bool {
    !matches!(
        kind,
        AdmissionErrorKind::Busy
            | AdmissionErrorKind::CapacityExhausted
            | AdmissionErrorKind::Invalid
    )
}

impl Ingress {
    pub fn admission_service(&self) -> Result<&NativeAdmission, AdmissionError> {
        self.durable().ok_or_else(|| AdmissionError {
            kind: AdmissionErrorKind::NotEnabled,
            message: "source has no durable admission binding".into(),
        })
    }

    /// Checks that a session-protocol request addresses this source and that
    /// durable admission is bound.
    pub fn check_source(&self, source: &str) -> Result<(), AdmissionError> {
        let source = drasi_computation_contracts::ComponentId::try_new(source).map_err(invalid)?;
        if source != self.source_id {
            return Err(invalid("source ID mismatch"));
        }
        self.admission_service()?;
        Ok(())
    }

    pub async fn admission_call<T>(
        &self,
        uncertainty: AdmissionErrorKind,
        operation: impl Future<Output = Result<T, AdmissionError>>,
    ) -> Result<T, AdmissionError> {
        if self.shutdown.is_cancelled() {
            return Err(AdmissionError {
                kind: AdmissionErrorKind::Closed,
                message: "source stopped".into(),
            });
        }
        let resolution = match uncertainty {
            AdmissionErrorKind::MetadataUnknown => {
                "retry registration with the same producer name or retirement with the same session"
            }
            AdmissionErrorKind::AcceptanceUnknown => {
                "resolve the same session and sequence before advancing"
            }
            _ => "retry the read after the source is available",
        };
        tokio::select! {
            biased;
            _ = self.abort.cancelled() => Err(AdmissionError { kind: uncertainty,
                message: format!("source aborted during request; {resolution}") }),
            result = tokio::time::timeout(self.live().timeout, operation) =>
                result.unwrap_or_else(|_| Err(AdmissionError { kind: uncertainty,
                    message: format!("admission deadline elapsed; {resolution}") })),
        }
    }

    pub fn durable_envelope(
        &self,
        change: SourceChange,
        sequence: u64,
    ) -> Result<ChangeEnvelope, AdmissionError> {
        // No receiving-side clock or volatile producer identity participates in
        // the retry digest. The client event's effective timestamp is preserved.
        let timestamp = chrono::DateTime::from_timestamp_millis(
            i64::try_from(change.get_transaction_time()).map_err(invalid)?,
        )
        .ok_or_else(|| invalid("event timestamp is out of range"))?;
        GraphChangeCodec::encode_change(
            change,
            self.config.stream.clone(),
            sequence,
            Some(timestamp),
        )
        .map_err(invalid)
    }

    /// Session-protocol batch admission. Events are admitted individually;
    /// the receipts for the committed prefix are returned with any error.
    /// A Pause stops the batch at the next event boundary.
    pub async fn admit_durable(
        &self,
        session: &ProducerSession,
        first_sequence: u64,
        changes: Vec<SourceChange>,
    ) -> Result<BatchOutcome, AdmissionError> {
        let service = self.admission_service()?;
        if changes.is_empty()
            || first_sequence == 0
            || first_sequence
                .checked_add(u64::try_from(changes.len() - 1).map_err(invalid)?)
                .is_none()
        {
            return Err(invalid(
                "nonempty batch and nonzero, non-overflowing sequence range required",
            ));
        }
        let envelopes = changes
            .into_iter()
            .enumerate()
            .map(|(offset, change)| self.durable_envelope(change, first_sequence + offset as u64))
            .collect::<Result<Vec<_>, _>>()?;
        let _admission = self.enter_admission().await.map_err(admission_error)?;
        let mut receipts = Vec::with_capacity(envelopes.len());
        let result = self
            .admission_call(AdmissionErrorKind::AcceptanceUnknown, async {
                for (offset, envelope) in envelopes.iter().enumerate() {
                    if self.is_paused() {
                        return Err(paused_admission());
                    }
                    receipts.push(
                        service
                            .admit_wait(session, first_sequence + offset as u64, envelope)
                            .await?,
                    );
                }
                Ok(())
            })
            .await;
        Ok(BatchOutcome {
            receipts,
            error: result.err(),
        })
    }

    /// Server-sequenced durable admission for the standard routes. Returns
    /// the number of leading events committed.
    pub(crate) async fn admit_implicit(
        &self,
        service: &NativeAdmission,
        changes: Vec<SourceChange>,
    ) -> Result<usize, AdmitFailure> {
        let _admission = self.enter_admission().await.map_err(AdmitFailure::before)?;
        // One server-sequenced writer at a time keeps sequences consecutive.
        let mut slot = self.implicit.lock().await;
        // Taken out of the slot: if this future is cancelled or times out the
        // session stays absent, forcing a resync on the next request.
        let mut current = slot.take();
        let mut accepted = 0;
        // Each failure states whether the producer head must be re-read.
        let work = async {
            if current.is_none() {
                let session = service
                    .register_producer(&self.source_id)
                    .await
                    .map_err(|error| (implicit_error(error), true))?;
                let status = service
                    .producer_status(&session)
                    .await
                    .map_err(|error| (implicit_error(error), true))?;
                let next = status.next_sequence.ok_or_else(|| {
                    (
                        IngressError::new(IngressErrorKind::Failed, "producer sequence exhausted"),
                        true,
                    )
                })?;
                current = Some(ImplicitSession { session, next });
            }
            let Some(state) = current.as_mut() else {
                return Err((
                    IngressError::new(IngressErrorKind::Failed, "producer session unavailable"),
                    true,
                ));
            };
            for change in changes {
                if self.is_paused() {
                    return Err((IngressError::paused(), false));
                }
                let envelope = self
                    .durable_envelope(change, state.next)
                    .map_err(|error| (IngressError::invalid(error.message), false))?;
                match service
                    .admit_wait(&state.session, state.next, &envelope)
                    .await
                {
                    Ok(_) => {
                        state.next = state.next.checked_add(1).ok_or_else(|| {
                            (
                                IngressError::new(
                                    IngressErrorKind::Failed,
                                    "producer sequence exhausted",
                                ),
                                true,
                            )
                        })?;
                        accepted += 1;
                    }
                    Err(error) => {
                        let resync = requires_resync(error.kind);
                        return Err((implicit_error(error), resync));
                    }
                }
            }
            Ok(())
        };
        let timeout = self.live().timeout;
        let result = tokio::select! {
            biased;
            _ = self.abort.cancelled() => Err((IngressError::new(
                IngressErrorKind::Uncertain,
                "source aborted during admission; the in-flight event may have been committed",
            ), true)),
            result = tokio::time::timeout(timeout, work) => result.unwrap_or_else(|_| Err((IngressError::new(
                IngressErrorKind::Uncertain,
                "admission deadline elapsed; the in-flight event may have been committed",
            ), true))),
        };
        let result = match result {
            Ok(()) => Ok(()),
            Err((error, resync)) => {
                if resync {
                    current = None;
                }
                Err(error)
            }
        };
        *slot = current;
        result
            .map(|()| accepted)
            .map_err(|error| AdmitFailure { accepted, error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IngressConfig, IngressSecurity};
    use crate::ingress::SourceRuntime;
    use drasi_computation_contracts::{ComponentDescriptor, ComponentId, GraphChangeCodec};
    use drasi_computation_plugin_sdk::{
        abi,
        lifecycle::{CancellationToken as LifecycleCancellation, StopMode},
        transport::{self, Failure},
        wire,
    };
    use drasi_core_types::models::{Element, ElementMetadata, ElementReference};
    use serde_json::json;
    use std::{ffi::c_void, sync::Arc, time::Duration};
    use tokio::sync::Notify;

    const SOURCE_ID: &str = "495412a1-7a8f-505f-a9ca-a0da3eeadc36";

    /// A host admission service whose ADMIT is held until released.
    struct HeldHost {
        entered: Notify,
        release: Notify,
    }

    fn session() -> ProducerSession {
        serde_json::from_value(json!({
            "incarnation":"00000000-0000-0000-0000-000000000001","producer":SOURCE_ID,"epoch":1
        }))
        .expect("session")
    }

    unsafe extern "C" fn retain(context: *mut c_void) {
        unsafe { Arc::increment_strong_count(context.cast::<HeldHost>()) };
    }
    unsafe extern "C" fn release(context: *mut c_void) {
        unsafe { Arc::decrement_strong_count(context.cast::<HeldHost>()) };
    }
    unsafe extern "C" fn report_failure(_: *mut c_void, _: abi::BorrowedBytes) -> abi::Status {
        abi::Status::ok()
    }
    unsafe extern "C" fn describe(_: *mut c_void) -> abi::Reply {
        transport::reply_boundary(|| {
            let identity: drasi_computation_contracts::GraphProducerIdentity =
                serde_json::from_value(json!({
                    "construction_scope":"scope","graph_id":"graph","component_id":SOURCE_ID,
                    "stream":"test","incarnation":"00000000-0000-0000-0000-000000000001",
                    "persistent":true
                }))
                .map_err(|error| Failure::failed(error.to_string()))?;
            wire::encode(&identity).map_err(Failure::from)
        })
    }
    unsafe extern "C" fn request(
        context: *mut c_void,
        code: u32,
        _: abi::BorrowedBytes,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        transport::status_boundary(|| {
            unsafe { Arc::increment_strong_count(context.cast::<HeldHost>()) };
            let host = unsafe { Arc::from_raw(context.cast::<HeldHost>()) };
            let operation = transport::export_operation(
                async move {
                    use abi::services::admission::*;
                    let session = session();
                    match code {
                        REGISTER => wire::encode(&Ok::<_, AdmissionError>(session)),
                        STATUS => wire::encode(&Ok::<_, AdmissionError>(
                            drasi_computation_contracts::ProducerStatus {
                                session,
                                next_sequence: Some(1),
                                earliest_receipt: None,
                            },
                        )),
                        ADMIT => {
                            host.entered.notify_one();
                            host.release.notified().await;
                            wire::encode(&Ok::<_, AdmissionError>(AdmissionReceipt {
                                session,
                                sequence: 1,
                                position: 1,
                            }))
                        }
                        _ => return Err(Failure::failed("unexpected admission request")),
                    }
                    .map_err(Failure::from)
                },
                None,
            );
            unsafe { transport::write_out(out, operation) }
        })
    }

    type Admitted = Result<usize, AdmitFailure>;

    /// Starts a durable runtime over a held host. Like a request handler, the
    /// listener owns one admission, whose result it reports.
    async fn held_runtime(
        host: &Arc<HeldHost>,
    ) -> anyhow::Result<(
        SourceRuntime,
        Arc<Ingress>,
        tokio::sync::oneshot::Receiver<Admitted>,
    )> {
        let table = abi::services::SourceAdmissionV1 {
            header: abi::Header::new::<abi::services::SourceAdmissionV1>(),
            context: Arc::as_ptr(host) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            request: Some(request),
            report_failure: Some(report_failure),
            describe: Some(describe),
        };
        let codec = Arc::new(wire::codec(&[GraphChangeCodec::schema()])?);
        let admission = unsafe { NativeAdmission::from_borrowed(&table, codec) }
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let config = IngressConfig {
            stream: drasi_computation_contracts::StreamId::try_new("test")?,
            host: "127.0.0.1".into(),
            port: 0,
            timeout_ms: 60_000,
            ingress_capacity: 8,
            max_message_bytes: 1024 * 1024,
            max_connections: 8,
            security: IngressSecurity::default(),
        };
        let descriptor = ComponentDescriptor::try_new(ComponentId::try_new(SOURCE_ID)?, vec![])?;
        let mut runtime = SourceRuntime::new(descriptor, config, None, Some(admission))?;
        let (ingress_sender, ingress_receiver) = tokio::sync::oneshot::channel();
        let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
        runtime
            .start(
                &LifecycleCancellation::new(),
                None,
                move |_listener, ingress| async move {
                    let _ = ingress_sender.send(ingress.clone());
                    let _ = result_sender.send(ingress.admit(vec![change()]).await);
                    ingress.shutdown.cancelled().await;
                    Ok(())
                },
            )
            .await
            .map_err(|error| error.into_source())?;
        Ok((runtime, ingress_receiver.await?, result_receiver))
    }

    fn change() -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "a"),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: 1,
                },
                properties: Default::default(),
            },
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drain_answers_in_flight_commits_and_abort_interrupts_them() -> anyhow::Result<()> {
        let deadline = || Some(tokio::time::Instant::now() + Duration::from_secs(5));
        let host = Arc::new(HeldHost {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let (mut runtime, ingress, result) = held_runtime(&host).await?;
        host.entered.notified().await;
        let stopping = runtime.stop(StopMode::Drain, deadline());
        tokio::pin!(stopping);
        tokio::select! {
            // Polled first: the stop begins before the new request arrives.
            biased;
            outcome = &mut stopping => panic!("drain passed the in-flight commit: {outcome:?}"),
            refused = ingress.admit(vec![change()]) => {
                // New work is refused as soon as the stop begins.
                assert_eq!(refused.expect_err("stopping").error.kind, IngressErrorKind::Stopped);
            }
        }
        host.release.notify_one();
        stopping.await?;
        assert_eq!(result.await?.expect("drained commit is answered"), 1);

        let host = Arc::new(HeldHost {
            entered: Notify::new(),
            release: Notify::new(),
        });
        let (mut runtime, _ingress, result) = held_runtime(&host).await?;
        host.entered.notified().await;
        // Abort does not wait for the held commit; its outcome is never
        // reported as success.
        runtime.stop(StopMode::Abort, deadline()).await?;
        assert!(!matches!(result.await, Ok(Ok(_))));
        Ok(())
    }

    #[test]
    fn only_clean_rejections_keep_the_server_sequence() {
        for kind in [
            AdmissionErrorKind::Busy,
            AdmissionErrorKind::CapacityExhausted,
            AdmissionErrorKind::Invalid,
        ] {
            assert!(!requires_resync(kind), "{kind:?}");
        }
        for kind in [
            AdmissionErrorKind::AcceptanceUnknown,
            AdmissionErrorKind::Sequence,
            AdmissionErrorKind::SessionExpired,
            AdmissionErrorKind::Closed,
            AdmissionErrorKind::PayloadConflict,
        ] {
            assert!(requires_resync(kind), "{kind:?}");
        }
        assert_eq!(
            implicit_error(AdmissionError {
                kind: AdmissionErrorKind::AcceptanceUnknown,
                message: "x".into()
            })
            .kind,
            IngressErrorKind::Uncertain
        );
    }
}
