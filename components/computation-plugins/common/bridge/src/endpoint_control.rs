// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use drasi_lib::computation::v1::ComponentRole;

use crate::{
    protocol::{read_message, write_message, ControlRequest, ControlResponse, Lane, CONTROL_BYTES},
    transport::{connect, deadline, BoxIo},
    BridgeConfig, BridgeError, ErrorCode, IngressCut, IngressDrainStatus, Result, TlsResource,
};

/// Authenticated transport control, not remote component/lifecycle execution.
/// Observing connections neither register producers nor replace data generations.
pub struct BridgeEndpointControl {
    config: BridgeConfig,
    source: bool,
    tls: Option<Arc<TlsResource>>,
    io: Option<BoxIo>,
    receiver_epoch: Option<uuid::Uuid>,
}

impl BridgeEndpointControl {
    pub async fn connect(
        config: BridgeConfig,
        role: ComponentRole,
        tls: Option<Arc<TlsResource>>,
    ) -> Result<Self> {
        config.validate()?;
        let source = match role {
            ComponentRole::Source => true,
            ComponentRole::Sink => false,
            _ => {
                return Err(BridgeError::invalid(
                    "endpoint control needs a source or sink definition",
                ))
            }
        };
        if config.transport.is_tls() != tls.is_some() {
            return Err(BridgeError::invalid(
                "endpoint control TLS credentials do not match transport",
            ));
        }
        if let Some(tls) = &tls {
            tls.validate_role(false)?;
        }
        let mut client = Self {
            config,
            source,
            tls,
            io: None,
            receiver_epoch: None,
        };
        client.reconnect().await?;
        Ok(client)
    }

    async fn reconnect(&mut self) -> Result<()> {
        if self.io.is_some() {
            return Ok(());
        }
        let timeout = self.config.limits.timeout();
        let mut io = connect(&self.config.transport, true, self.tls.as_deref(), timeout).await?;
        let request = ControlRequest::Observe {
            contract: self.config.contract(self.source),
        };
        let response: ControlResponse = deadline(timeout, async {
            write_message(&mut io, Lane::Control, &request, CONTROL_BYTES).await?;
            read_message(&mut io, Lane::Control, CONTROL_BYTES).await
        })
        .await?;
        let epoch = match response {
            ControlResponse::Observed { receiver_epoch } => receiver_epoch,
            ControlResponse::Failure(error) => return Err(error),
            _ => {
                return Err(BridgeError::invalid(
                    "peer does not support observing/seal control",
                ))
            }
        };
        if self
            .receiver_epoch
            .is_some_and(|previous| previous != epoch)
        {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "receiver restarted during composition control",
            ));
        }
        self.receiver_epoch = Some(epoch);
        self.io = Some(io);
        Ok(())
    }

    async fn request(&mut self, request: ControlRequest) -> Result<ControlResponse> {
        self.reconnect().await?;
        let mut io = self.io.take().expect("connected observer");
        let response = deadline(self.config.limits.timeout(), async {
            write_message(&mut io, Lane::Control, &request, CONTROL_BYTES).await?;
            read_message(&mut io, Lane::Control, CONTROL_BYTES).await
        })
        .await?;
        self.io = Some(io);
        match response {
            ControlResponse::Failure(error) => Err(error),
            response => Ok(response),
        }
    }

    pub async fn seal(&mut self, timeout: Duration) -> Result<IngressCut> {
        deadline(timeout, async {
            let epoch = self.receiver_epoch.expect("connected receiver epoch");
            match self
                .request(ControlRequest::Seal {
                    receiver_epoch: epoch,
                })
                .await?
            {
                ControlResponse::Sealed(cut) => Ok(cut),
                _ => Err(BridgeError::invalid("unexpected ingress cut response")),
            }
        })
        .await
    }

    pub async fn drain_status(&mut self, cut: &IngressCut) -> Result<IngressDrainStatus> {
        match self
            .request(ControlRequest::DrainStatus { cut: cut.clone() })
            .await?
        {
            ControlResponse::DrainStatus(status) => Ok(status),
            _ => Err(BridgeError::invalid("unexpected drain response")),
        }
    }

    pub async fn wait_drained(
        &mut self,
        cut: &IngressCut,
        timeout: Duration,
    ) -> Result<IngressDrainStatus> {
        deadline(timeout, async {
            loop {
                let status = self.drain_status(cut).await?;
                if status.drained {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }
}
