// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use drasi_lib::{
    computation::v1::ComponentId,
    management::{CommittedConfiguration, DesiredInstance},
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::{HostConfig, HostObservation, CONTROL_VERSION};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub version: u32,
    pub instance_id: String,
    pub generation: Uuid,
    pub endpoint: PathBuf,
    pub manifest_fingerprint: String,
    pub token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn new(config: &HostConfig) -> Result<Self> {
        let endpoint = config.state_dir.join("c.sock");
        ensure!(
            endpoint.as_os_str().as_encoded_bytes().len() <= 100,
            "state directory is too long for a portable Unix control socket"
        );
        Ok(Self {
            version: CONTROL_VERSION,
            instance_id: config.instance_id.clone(),
            generation: Uuid::new_v4(),
            endpoint,
            manifest_fingerprint: config.fingerprint()?,
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        })
    }

    pub fn load(state_dir: &Path) -> Result<Self> {
        crate::state::read_json(&state_dir.join("control.json"), 8192)
    }

    pub(crate) fn validate(&self, config: &HostConfig) -> Result<()> {
        ensure!(
            self.version == CONTROL_VERSION
                && self.instance_id == config.instance_id
                && self.endpoint == config.state_dir.join("c.sock")
                && self.manifest_fingerprint == config.fingerprint()?
                && self.token.len() == 64
                && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "launch identity or manifest fingerprint mismatch"
        );
        Ok(())
    }

    pub(crate) fn authorize(&self, request: &ControlRequest) -> Result<()> {
        ensure!(
            request.version == CONTROL_VERSION,
            "unsupported control version"
        );
        ensure!(
            request.instance_id == self.instance_id && request.generation == self.generation,
            "stale or foreign instance generation"
        );
        let actual = request.token.as_bytes();
        let expected = self.token.as_bytes();
        ensure!(
            actual.len() == expected.len(),
            "control authentication failed"
        );
        let difference = actual
            .iter()
            .zip(expected)
            .fold(0_u8, |value, (a, b)| value | (a ^ b));
        ensure!(difference == 0, "control authentication failed");
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlCommand {
    Hello,
    Status,
    Configuration,
    Operation {
        operation_id: Uuid,
    },
    Apply {
        expected_revision: u64,
        management_request_id: String,
        desired: Box<DesiredInstance>,
    },
    Reconcile,
    Start,
    StartComponents {
        components: Vec<ComponentId>,
    },
    Quiesce {
        components: Vec<ComponentId>,
    },
    StopTree,
    RepairChild {
        component: String,
        generation: Uuid,
    },
    StopChild {
        component: String,
        generation: Uuid,
    },
    ForceChild {
        component: String,
        generation: Uuid,
    },
    /// CLI process boundary only; never a DrasiLib component cancellation mode.
    ForceExit,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlRequest {
    pub version: u32,
    pub instance_id: String,
    pub generation: Uuid,
    pub token: String,
    pub request_id: Uuid,
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
    Accepted,
    Running,
    Incomplete,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationStatus {
    pub id: Uuid,
    pub phase: OperationPhase,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlReply {
    Hello {
        runtime: String,
        package_version: String,
        capabilities: Vec<String>,
    },
    Status {
        observation: Box<HostObservation>,
    },
    Accepted {
        operation_id: Uuid,
    },
    Operation {
        operation: OperationStatus,
    },
    Configuration {
        configuration: CommittedConfiguration,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlResponse {
    pub version: u32,
    pub instance_id: String,
    pub generation: Uuid,
    pub request_id: Uuid,
    pub reply: Option<ControlReply>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct ControlClient {
    pub credentials: Credentials,
    pub max_frame_bytes: usize,
    pub timeout: Duration,
}

impl ControlClient {
    pub fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            max_frame_bytes: 2 * crate::config::MAX_MANIFEST_BYTES,
            timeout: Duration::from_secs(2),
        }
    }

    /// Blocking local control, intended for the supervisor's independent thread
    /// or a synchronous embedding host, not a graph data runner.
    pub fn request(&self, request_id: Uuid, command: ControlCommand) -> Result<ControlReply> {
        ensure!(
            tokio::runtime::Handle::try_current().is_err(),
            "use request_async from a Tokio runtime"
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(self.request_async(request_id, command))
    }

    pub async fn request_async(
        &self,
        request_id: Uuid,
        command: ControlCommand,
    ) -> Result<ControlReply> {
        tokio::time::timeout(self.timeout, async {
            let mut stream = tokio::net::UnixStream::connect(&self.credentials.endpoint)
                .await
                .context("connect instance control")?;
            write_async(
                &mut stream,
                &ControlRequest {
                    version: CONTROL_VERSION,
                    instance_id: self.credentials.instance_id.clone(),
                    generation: self.credentials.generation,
                    token: self.credentials.token.clone(),
                    request_id,
                    command,
                },
                self.max_frame_bytes,
            )
            .await?;
            let response: ControlResponse = read_async(&mut stream, self.max_frame_bytes).await?;
            ensure!(
                response.version == CONTROL_VERSION
                    && response.instance_id == self.credentials.instance_id
                    && response.generation == self.credentials.generation
                    && response.request_id == request_id,
                "stale or mismatched control response"
            );
            if let Some(error) = response.error {
                anyhow::bail!("{error}");
            }
            response.reply.context("control response has no result")
        })
        .await
        .context("bounded control request timed out")?
    }

    pub fn status(&self) -> Result<HostObservation> {
        match self.request(Uuid::new_v4(), ControlCommand::Status)? {
            ControlReply::Status { observation } => Ok(*observation),
            _ => anyhow::bail!("unexpected control status reply"),
        }
    }
}

pub(crate) fn read_frame<T: DeserializeOwned>(reader: &mut impl Read, limit: usize) -> Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= limit,
        "control frame exceeds byte budget"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid bounded control frame")
}

pub(crate) fn write_frame(
    writer: &mut impl Write,
    value: &impl Serialize,
    limit: usize,
) -> Result<()> {
    let bytes = encode(value, limit)?;
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub(crate) async fn read_async<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<T> {
    let length = reader.read_u32().await? as usize;
    ensure!(
        length > 0 && length <= limit,
        "control frame exceeds byte budget"
    );
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).context("invalid bounded control frame")
}

pub(crate) async fn write_async(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
    limit: usize,
) -> Result<()> {
    let bytes = encode(value, limit)?;
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= limit && bytes.len() <= u32::MAX as usize,
        "control frame exceeds byte budget"
    );
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn rejects_oversized_and_recursive_frames_before_unbounded_allocation() {
        let mut input = std::io::Cursor::new(u32::MAX.to_be_bytes());
        assert!(read_frame::<serde_json::Value>(&mut input, 256).is_err());
        let nested = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        let mut input = (nested.len() as u32).to_be_bytes().to_vec();
        input.extend(nested.as_bytes());
        assert!(read_frame::<serde_json::Value>(&mut input.as_slice(), 1024).is_err());
    }

    #[test]
    fn authenticates_generation_before_mutation() {
        let config = HostConfig::empty("host", "/host");
        let credentials = Credentials::new(&config).unwrap();
        let mut request = ControlRequest {
            version: CONTROL_VERSION,
            instance_id: "host".into(),
            generation: Uuid::new_v4(),
            token: credentials.token.clone(),
            request_id: Uuid::new_v4(),
            command: ControlCommand::StopTree,
        };
        assert!(credentials.authorize(&request).is_err());
        request.generation = credentials.generation;
        assert!(credentials.authorize(&request).is_ok());
        request.token.replace_range(0..1, "x");
        assert!(credentials.authorize(&request).is_err());
    }
}
