// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{ReadResult, SnapshotBatchLimits};
use crate::computation::v1::{
    BinaryEnvelopeCodec, BootstrapSessionId, ChangeEnvelope, ComponentGeneration, OperationEpoch,
    OutputCursor, OutputEndpoint, OutputReadError, Schema, SchemaDescriptor,
};
use futures::{stream, stream::BoxStream, StreamExt};
use sha2::{Digest, Sha256};
use std::{num::NonZeroU64, sync::Arc};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufWriter},
};

#[cfg(test)]
mod tests;

pub(super) type DigestBytes = [u8; 32];
const DIGEST_LEN: u64 = 32;
const FRAME_HEADER_LEN: u64 = 8 + DIGEST_LEN;

fn failed(error: impl Into<anyhow::Error>) -> OutputReadError {
    OutputReadError::Failed(error.into())
}

/// The out-of-band expected scope, including for a zero-frame file. Hash the
/// definition directly rather than making a second copy of its opaque bytes.
pub(in super::super) fn file_scope(
    session: BootstrapSessionId,
    endpoint: &OutputEndpoint,
    generation: ComponentGeneration,
    operation: OperationEpoch,
    dataset: &str,
    cursor: Option<&OutputCursor>,
    schema: &SchemaDescriptor,
) -> ReadResult<DigestBytes> {
    let mut hash = Sha256::new();
    serde_json::to_writer(
        &mut hash,
        &(
            "drasi-current-state/v1",
            session,
            endpoint,
            generation.0,
            operation.0,
            dataset,
            cursor,
            schema.id(),
            schema.version().value(),
            schema.encoding(),
        ),
    )
    .map_err(failed)?;
    hash.update(schema.definition());
    Ok(hash.finalize().into())
}

/// A sealed, in-process file reference. There is no pathname to reopen,
/// substitute, pass to a plugin, or scavenge after a process crash.
pub(in super::super) struct FileTransfer {
    file: BufReader<File>,
    bytes: u64,
    digest: DigestBytes,
    codec: BinaryEnvelopeCodec,
    limits: SnapshotBatchLimits,
}

/// The cut may only become known when a provider finishes. Reserve its scope
/// trailer up front, append each envelope once, and bind the trailer at seal.
pub(super) struct FileWriter {
    file: BufWriter<File>,
    bytes: u64,
    digest: Sha256,
    codec: BinaryEnvelopeCodec,
    limits: SnapshotBatchLimits,
    quota: NonZeroU64,
}

impl FileWriter {
    pub(super) async fn open(
        codec: BinaryEnvelopeCodec,
        limits: SnapshotBatchLimits,
        quota: NonZeroU64,
    ) -> ReadResult<Self> {
        if DIGEST_LEN > quota.get() {
            return Err(over_quota(quota));
        }

        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(failed)?
            .map_err(failed)?;
        Ok(Self {
            file: BufWriter::new(File::from_std(file)),
            bytes: DIGEST_LEN,
            digest: Sha256::new(),
            codec,
            limits,
            quota,
        })
    }

    /// On error or cancellation the caller must discard this writer.
    pub(super) async fn append(&mut self, envelope: &ChangeEnvelope) -> ReadResult<()> {
        let frame = self.codec.encode(envelope).map_err(failed)?;
        let len = frame.len() as u64;
        self.bytes = self
            .bytes
            .checked_add(FRAME_HEADER_LEN)
            .and_then(|bytes| bytes.checked_add(len))
            .filter(|bytes| *bytes <= self.quota.get())
            .ok_or_else(|| over_quota(self.quota))?;
        let checksum = Sha256::digest(&frame);
        for part in [&len.to_be_bytes()[..], &checksum, &frame] {
            self.file.write_all(part).await.map_err(failed)?;
            self.digest.update(part);
        }
        Ok(())
    }

    pub(super) async fn finish(mut self, scope: DigestBytes) -> ReadResult<FileTransfer> {
        self.file.write_all(&scope).await.map_err(failed)?;
        self.digest.update(scope);
        self.file.flush().await.map_err(failed)?;
        Ok(FileTransfer {
            file: BufReader::new(self.file.into_inner()),
            bytes: self.bytes,
            digest: self.digest.finalize().into(),
            codec: self.codec,
            limits: self.limits,
        })
    }
}

fn over_quota(quota: NonZeroU64) -> OutputReadError {
    OutputReadError::invalid(format!(
        "snapshot exceeds the {quota}-byte temporary file quota"
    ))
}

impl FileTransfer {
    #[cfg(test)]
    pub(super) async fn truncate_for_test(&self) -> std::io::Result<()> {
        self.file.get_ref().set_len(self.bytes - 1).await
    }

    #[cfg(test)]
    pub(super) async fn seal(
        batches: impl Iterator<Item = ReadResult<ChangeEnvelope>> + Send,
        scope: DigestBytes,
        schema: Arc<Schema>,
        limits: SnapshotBatchLimits,
        quota: NonZeroU64,
    ) -> ReadResult<Self> {
        Self::seal_stream(stream::iter(batches), scope, schema, limits, quota).await
    }

    pub(in super::super) async fn seal_stream(
        batches: impl futures::Stream<Item = ReadResult<ChangeEnvelope>> + Send,
        scope: DigestBytes,
        schema: Arc<Schema>,
        limits: SnapshotBatchLimits,
        quota: NonZeroU64,
    ) -> ReadResult<Self> {
        let mut codec = BinaryEnvelopeCodec::new(limits.max_bytes);
        codec.register_schema(schema).map_err(failed)?;
        let mut writer = FileWriter::open(codec, limits, quota).await?;
        tokio::pin!(batches);
        while let Some(batch) = batches.next().await {
            writer.append(&batch?).await?;
            tokio::task::yield_now().await;
        }
        writer.finish(scope).await
    }

    /// Verify the complete seal before exposing even the first batch. Read at
    /// most a block at a time and never follow a corrupt length to allocate.
    async fn verify(&mut self, scope: DigestBytes) -> ReadResult<()> {
        if self.file.get_ref().metadata().await.map_err(failed)?.len() != self.bytes {
            return Err(OutputReadError::invalid("snapshot file length mismatch"));
        }
        self.file.rewind().await.map_err(failed)?;
        let mut hash = Sha256::new();
        let mut block = vec![0; 64 * 1024];
        let mut remaining = self.bytes - DIGEST_LEN;
        while remaining > 0 {
            let len = remaining.min(block.len() as u64) as usize;
            self.file
                .read_exact(&mut block[..len])
                .await
                .map_err(failed)?;
            hash.update(&block[..len]);
            remaining -= len as u64;
        }
        let mut trailer = [0; DIGEST_LEN as usize];
        self.file.read_exact(&mut trailer).await.map_err(failed)?;
        if trailer != scope {
            return Err(OutputReadError::invalid(
                "snapshot file belongs to another read",
            ));
        }
        hash.update(trailer);
        if DigestBytes::from(hash.finalize()) != self.digest {
            return Err(OutputReadError::invalid("snapshot file seal mismatch"));
        }
        self.file.rewind().await.map_err(failed)?;
        Ok(())
    }

    pub(in super::super) fn import(
        self,
        scope: DigestBytes,
    ) -> BoxStream<'static, ReadResult<ChangeEnvelope>> {
        // The stream owns the in-flight future. Cancelling a waiting caller's
        // `next_batch` and retrying cannot lose a partially read frame.
        let remaining = self.bytes - DIGEST_LEN;
        stream::try_unfold(
            (self, remaining, false),
            move |(mut sealed, remaining, verified)| async move {
                if !verified {
                    sealed.verify(scope).await?;
                }
                if remaining == 0 {
                    if sealed
                        .file
                        .get_ref()
                        .metadata()
                        .await
                        .map_err(failed)?
                        .len()
                        != sealed.bytes
                    {
                        return Err(OutputReadError::invalid("snapshot file length changed"));
                    }
                    return Ok(None);
                }
                let len = sealed.file.read_u64().await.map_err(failed)?;
                let rest = remaining
                    .checked_sub(FRAME_HEADER_LEN)
                    .and_then(|bytes| bytes.checked_sub(len))
                    .filter(|_| len != 0 && len <= sealed.limits.max_bytes.get() as u64)
                    .ok_or_else(|| OutputReadError::invalid("invalid snapshot frame length"))?;
                let mut checksum = [0; DIGEST_LEN as usize];
                sealed
                    .file
                    .read_exact(&mut checksum)
                    .await
                    .map_err(failed)?;
                let mut frame = vec![0; len as usize];
                sealed.file.read_exact(&mut frame).await.map_err(failed)?;
                if DigestBytes::from(Sha256::digest(&frame)) != checksum {
                    return Err(OutputReadError::invalid("snapshot frame checksum mismatch"));
                }
                let envelope = sealed.codec.decode(&frame).map_err(failed)?;
                Ok(Some((envelope, (sealed, rest, true))))
            },
        )
        .boxed()
    }
}
