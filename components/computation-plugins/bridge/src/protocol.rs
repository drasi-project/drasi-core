// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_core::interface::FailureMode;
use drasi_lib::computation::v1::{
    encode_bounded_messagepack, AdmissionReceipt, ProducerSession, ProducerStatus,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    codec::preflight_messagepack, config::LinkContract, BridgeError, IngressCut,
    IngressDrainStatus, Profile, Result,
};

pub const PROTOCOL_VERSION: u16 = 1;
pub const CONTROL_BYTES: usize = 16 * 1024;
const MAGIC: &[u8; 4] = b"DRBG";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AcceptanceBoundary {
    VolatileQueue,
    DurableAdmission { failure_scope: FailureMode },
}

impl AcceptanceBoundary {
    pub(crate) fn for_contract(contract: &LinkContract) -> Self {
        match contract.profile {
            Profile::Volatile => Self::VolatileQueue,
            Profile::Durable => Self::DurableAdmission {
                failure_scope: contract.failure_scope,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub admission: AdmissionReceipt,
    pub boundary: AcceptanceBoundary,
    pub contract: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkStatus {
    pub generation: u64,
    pub connected: bool,
    /// A currently serviced data request, not a graph-wide processing count.
    pub in_flight: bool,
    pub acceptance_unknown: bool,
    pub accepted: u64,
    pub stopping: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum ControlRequest {
    Observe {
        contract: LinkContract,
    },
    Seal {
        receiver_epoch: uuid::Uuid,
    },
    DrainStatus {
        cut: IngressCut,
    },
    Hello {
        contract: LinkContract,
        expected_session: Option<ProducerSession>,
    },
    Status {
        generation: u64,
    },
    ProducerStatus {
        generation: u64,
        session: ProducerSession,
    },
    Receipt {
        generation: u64,
        session: ProducerSession,
        sequence: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Welcome {
    pub version: u16,
    pub generation: u64,
    pub session: ProducerSession,
    pub next_sequence: Option<u64>,
    pub earliest_receipt: Option<u64>,
    pub boundary: AcceptanceBoundary,
    pub contract: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum ControlResponse {
    Observed { receiver_epoch: uuid::Uuid },
    Sealed(IngressCut),
    DrainStatus(IngressDrainStatus),
    Welcome(Welcome),
    Status(LinkStatus),
    ProducerStatus(ProducerStatus),
    Receipt(Option<Receipt>),
    Failure(BridgeError),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DataHello {
    pub version: u16,
    pub generation: u64,
    pub session: ProducerSession,
    pub contract: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DataRequest {
    pub generation: u64,
    pub session: ProducerSession,
    pub sequence: u64,
    #[serde(with = "serde_bytes")]
    pub envelope: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum DataResponse {
    Ready { generation: u64 },
    Receipt(Receipt),
    Failure(BridgeError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Lane {
    Control = 1,
    DataHello = 2,
    Data = 3,
    Receipt = 4,
}

pub(crate) async fn write_message<W: AsyncWrite + Unpin + ?Sized, T: Serialize>(
    stream: &mut W,
    lane: Lane,
    value: &T,
    limit: usize,
) -> Result<()> {
    let bytes = encode_bounded_messagepack(value, limit)
        .map_err(|error| BridgeError::invalid(error.to_string()))?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| BridgeError::invalid("bridge frame length exceeds u32"))?;
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(MAGIC);
    header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    header[6] = lane as u8;
    header[8..].copy_from_slice(&length.to_be_bytes());
    stream
        .write_all(&header)
        .await
        .map_err(BridgeError::unknown)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(BridgeError::unknown)?;
    stream.flush().await.map_err(BridgeError::unknown)?;
    Ok(())
}

pub(crate) async fn read_message<R: AsyncRead + Unpin + ?Sized, T: DeserializeOwned>(
    stream: &mut R,
    lane: Lane,
    limit: usize,
) -> Result<T> {
    let mut header = [0u8; 12];
    // EOF, including an empty read between requests, is not source exhaustion.
    stream
        .read_exact(&mut header)
        .await
        .map_err(BridgeError::unknown)?;
    if &header[..4] != MAGIC
        || u16::from_be_bytes([header[4], header[5]]) != PROTOCOL_VERSION
        || header[6] != lane as u8
        || header[7] != 0
    {
        return Err(BridgeError::invalid(
            "bridge framing, version or lane mismatch",
        ));
    }
    let length = u32::from_be_bytes(header[8..].try_into().expect("four header bytes")) as usize;
    if length == 0 || length > limit {
        return Err(BridgeError::invalid(
            "bridge frame length exceeds configured limit",
        ));
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(BridgeError::unknown)?;
    preflight_messagepack(&bytes, limit, 65_536)?;
    rmp_serde::from_slice(&bytes).map_err(|error| BridgeError::invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn partial_reads_and_writes_preserve_a_whole_frame() {
        let (mut writer, mut reader) = duplex(3);
        let value = vec![0xacu8; 257];
        let (sent, received) = tokio::join!(
            write_message(&mut writer, Lane::Control, &value, 1024),
            read_message::<_, Vec<u8>>(&mut reader, Lane::Control, 1024)
        );
        sent.unwrap();
        assert_eq!(received.unwrap(), value);
    }

    #[tokio::test]
    async fn lengths_versions_truncation_and_trailing_data_reject() {
        for (version, length, body) in [
            (PROTOCOL_VERSION, u32::MAX, vec![]),
            (PROTOCOL_VERSION + 1, 1, vec![0xc0]),
            (PROTOCOL_VERSION, 5, vec![0xc0]),
            (PROTOCOL_VERSION, 2, vec![0xc0, 0xc0]),
        ] {
            let (mut writer, mut reader) = duplex(128);
            let mut frame = MAGIC.to_vec();
            frame.extend_from_slice(&version.to_be_bytes());
            frame.extend_from_slice(&[Lane::Control as u8, 0]);
            frame.extend_from_slice(&length.to_be_bytes());
            frame.extend(body);
            writer.write_all(&frame).await.unwrap();
            writer.shutdown().await.unwrap();
            assert!(read_message::<_, ()>(&mut reader, Lane::Control, 64)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn exact_encoder_limit_is_enforced_before_writing() {
        let value = "bounded";
        let length = encode_bounded_messagepack(&value, 100).unwrap().len();
        let mut output = Vec::new();
        write_message(&mut output, Lane::Control, &value, length)
            .await
            .unwrap();
        assert_eq!(output.len(), 12 + length);
        let mut failed = Vec::new();
        assert!(
            write_message(&mut failed, Lane::Control, &value, length - 1)
                .await
                .is_err()
        );
        assert!(failed.is_empty());
    }
}
