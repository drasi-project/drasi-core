// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use drasi_lib::computation::v1::{ProducerSession, ProducerStatus};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::{
    config::LinkContract,
    protocol::{
        read_message, write_message, AcceptanceBoundary, ControlRequest, ControlResponse,
        DataHello, DataResponse, Lane, LinkStatus, Receipt, Welcome, CONTROL_BYTES,
        PROTOCOL_VERSION,
    },
    transport::{connect, deadline, BoxIo},
    BridgeConfig, BridgeError, ErrorCode, Result, TlsResource,
};

struct ControlConnection {
    io: BoxIo,
    generation: u64,
}

/// Read-only link control, on a separate socket and independently usable while
/// a data call is waiting. This is not a process-management capability.
#[derive(Clone)]
pub struct BridgeControl {
    connection: Arc<Mutex<Option<ControlConnection>>>,
    timeout: std::time::Duration,
}

impl BridgeControl {
    async fn request(
        &self,
        request: impl FnOnce(u64) -> ControlRequest,
    ) -> Result<ControlResponse> {
        let mut connection = self.connection.try_lock().map_err(|_| {
            BridgeError::new(ErrorCode::Busy, "another link control request is active")
        })?;
        let mut current = connection
            .take()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "bridge control is disconnected"))?;
        let request = request(current.generation);
        let result = deadline(self.timeout, async {
            write_message(&mut current.io, Lane::Control, &request, CONTROL_BYTES).await?;
            read_message(&mut current.io, Lane::Control, CONTROL_BYTES).await
        })
        .await;
        // Dropping this future drops the taken socket. A cancelled/partial
        // exchange can never be resumed as the next framed message.
        if result.is_ok() {
            *connection = Some(current);
        }
        match result? {
            ControlResponse::Failure(error) => Err(error),
            value => Ok(value),
        }
    }

    pub async fn status(&self) -> Result<LinkStatus> {
        match self
            .request(|generation| ControlRequest::Status { generation })
            .await?
        {
            ControlResponse::Status(status) => Ok(status),
            _ => Err(BridgeError::invalid("unexpected bridge status response")),
        }
    }

    pub async fn receipt(
        &self,
        session: &ProducerSession,
        sequence: u64,
    ) -> Result<Option<Receipt>> {
        match self
            .request(|generation| ControlRequest::Receipt {
                generation,
                session: session.clone(),
                sequence,
            })
            .await?
        {
            ControlResponse::Receipt(receipt) => Ok(receipt),
            _ => Err(BridgeError::invalid("unexpected bridge receipt response")),
        }
    }

    pub async fn producer_status(&self, session: &ProducerSession) -> Result<ProducerStatus> {
        match self
            .request(|generation| ControlRequest::ProducerStatus {
                generation,
                session: session.clone(),
            })
            .await?
        {
            ControlResponse::ProducerStatus(status) => Ok(status),
            _ => Err(BridgeError::invalid("unexpected producer status response")),
        }
    }
}

pub struct Client {
    config: BridgeConfig,
    contract: LinkContract,
    tls: Option<Arc<TlsResource>>,
    control: BridgeControl,
    data: Option<BoxIo>,
    welcome: Option<Welcome>,
}

impl Client {
    pub fn new(config: BridgeConfig, tls: Option<Arc<TlsResource>>) -> Self {
        let contract = config.contract(false);
        let control = BridgeControl {
            connection: Arc::new(Mutex::new(None)),
            timeout: config.limits.timeout(),
        };
        Self {
            config,
            contract,
            tls,
            control,
            data: None,
            welcome: None,
        }
    }

    pub fn control(&self) -> BridgeControl {
        self.control.clone()
    }

    pub async fn invalidate(&mut self) {
        self.data = None;
        self.welcome = None;
        *self.control.connection.lock().await = None;
    }

    pub async fn connect(&mut self, expected: Option<&ProducerSession>) -> Result<Welcome> {
        if let Some(welcome) = &mut self.welcome {
            if let Ok(status) = self.control.producer_status(&welcome.session).await {
                welcome.next_sequence = status.next_sequence;
                welcome.earliest_receipt = status.earliest_receipt;
                return Ok(welcome.clone());
            }
        }
        self.invalidate().await;
        let timeout = self.config.limits.timeout();
        let mut io = connect(&self.config.transport, true, self.tls.as_deref(), timeout).await?;
        let request = ControlRequest::Hello {
            contract: self.contract.clone(),
            expected_session: expected.cloned(),
        };
        let response = deadline(timeout, async {
            write_message(&mut io, Lane::Control, &request, CONTROL_BYTES).await?;
            read_message(&mut io, Lane::Control, CONTROL_BYTES).await
        })
        .await?;
        let welcome = match response {
            ControlResponse::Welcome(welcome) => welcome,
            ControlResponse::Failure(error) => return Err(error),
            _ => return Err(BridgeError::invalid("expected bridge handshake response")),
        };
        if welcome.version != PROTOCOL_VERSION
            || welcome.generation == 0
            || welcome.contract != self.contract.fingerprint()
            || welcome.boundary != AcceptanceBoundary::for_contract(&self.contract)
        {
            return Err(BridgeError::new(
                ErrorCode::Incompatible,
                "receiver negotiated a different bridge boundary",
            ));
        }
        if expected.is_some_and(|expected| expected != &welcome.session) {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "receiver changed its session during reconnect",
            ));
        }
        *self.control.connection.lock().await = Some(ControlConnection {
            io,
            generation: welcome.generation,
        });
        self.welcome = Some(welcome.clone());
        Ok(welcome)
    }

    pub async fn send(
        &mut self,
        session: &ProducerSession,
        sequence: u64,
        envelope: &[u8],
    ) -> Result<Receipt> {
        let timeout = self.config.limits.timeout();
        let welcome = self
            .welcome
            .as_ref()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "bridge is not connected"))?;
        if &welcome.session != session {
            return Err(BridgeError::new(
                ErrorCode::StateLost,
                "sender mapping belongs to another receiver session",
            ));
        }
        let generation = welcome.generation;
        if self.data.is_none() {
            let mut io =
                connect(&self.config.transport, false, self.tls.as_deref(), timeout).await?;
            let hello = DataHello {
                version: PROTOCOL_VERSION,
                generation,
                session: session.clone(),
                contract: self.contract.fingerprint(),
            };
            let response = deadline(timeout, async {
                write_message(&mut io, Lane::DataHello, &hello, CONTROL_BYTES).await?;
                read_message(&mut io, Lane::Receipt, CONTROL_BYTES).await
            })
            .await?;
            match response {
                DataResponse::Ready {
                    generation: returned,
                } if returned == generation => self.data = Some(io),
                DataResponse::Failure(error) => return Err(error),
                _ => {
                    return Err(BridgeError::invalid(
                        "invalid data-channel handshake response",
                    ))
                }
            }
        }
        #[derive(Serialize)]
        struct Request<'a> {
            generation: u64,
            session: &'a ProducerSession,
            sequence: u64,
            #[serde(with = "serde_bytes")]
            envelope: &'a [u8],
        }
        let request = Request {
            generation,
            session,
            sequence,
            envelope,
        };
        let mut io = self.data.take().expect("connected data lane");
        let response = deadline(timeout, async {
            write_message(
                &mut io,
                Lane::Data,
                &request,
                self.config.limits.max_envelope_bytes + CONTROL_BYTES,
            )
            .await?;
            read_message(&mut io, Lane::Receipt, CONTROL_BYTES).await
        })
        .await?;
        self.data = Some(io);
        match response {
            DataResponse::Receipt(receipt) => Ok(receipt),
            DataResponse::Failure(error) => Err(error),
            _ => Err(BridgeError::invalid("unexpected data admission response")),
        }
    }
}
