// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, DisposeContext, InitializeContext, LifecycleResult, PauseContext, ResumeContext,
    StartContext, StopContext,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, EnvelopeCodec, GraphChangeCodec, InputEnvelope, PermanentFailurePolicy,
    SinkCompletion,
};
use serde::{Deserialize, Serialize};
use std::{io::SeekFrom, num::NonZeroUsize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(super) fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    ConfigDefinition::try_new([
        Field::new(
            "path",
            String,
            true,
            "Nonempty output file path; its parent directory must already exist.",
        ),
        Field::new(
            "append",
            Boolean,
            false,
            "Append to the file instead of truncating it when the component starts.",
        )
        .default(false),
        Field::on_permanent_failure(),
    ])
    .expect("constant capture configuration")
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Configuration {
    path: String,
    #[serde(default)]
    append: bool,
    #[serde(default)]
    pub(super) on_permanent_failure: PermanentFailurePolicy,
}

impl Configuration {
    pub(super) fn parse(configuration: serde_json::Value) -> anyhow::Result<Self> {
        let configuration: Self = serde_json::from_value(configuration)?;
        anyhow::ensure!(
            !configuration.path.is_empty(),
            "capture path must not be empty"
        );
        Ok(configuration)
    }
}

/// The open capture file and the length of its complete frames.
struct Output {
    file: tokio::fs::File,
    /// File length after the last frame that was completely written.
    committed: u64,
    /// A write after `committed` failed or was cancelled and may have left a
    /// partial frame.
    dirty: bool,
}

impl Output {
    /// Opens the file. When appending, a trailing partial frame left by an
    /// earlier process is removed, so every frame starts on its own line.
    async fn open(path: &str, append: bool) -> anyhow::Result<Self> {
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .await?;
        let committed = if append {
            complete_length(&mut file).await?
        } else {
            0
        };
        let mut output = Self {
            file,
            committed,
            dirty: true,
        };
        output.repair().await?;
        Ok(output)
    }

    /// Cuts the file back to its complete frames. A cancelled write's
    /// in-flight I/O completes before `set_len` runs.
    async fn repair(&mut self) -> anyhow::Result<()> {
        if self.dirty {
            self.file.set_len(self.committed).await?;
            self.file.seek(SeekFrom::Start(self.committed)).await?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Writes one complete frame, or leaves the file to be cut back to the
    /// previous frame before anything else is written.
    async fn write_frame(&mut self, frame: &[u8]) -> anyhow::Result<()> {
        self.repair().await?;
        self.dirty = true;
        self.file.write_all(frame).await?;
        self.file.flush().await?;
        self.committed += frame.len() as u64;
        self.dirty = false;
        Ok(())
    }
}

/// The length of the file's complete, newline-terminated frames.
async fn complete_length(file: &mut tokio::fs::File) -> anyhow::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut end = file.metadata().await?.len();
    let mut buffer = vec![0; CHUNK as usize];
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let chunk = &mut buffer[..(end - start) as usize];
        file.seek(SeekFrom::Start(start)).await?;
        file.read_exact(chunk).await?;
        if let Some(newline) = chunk.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + newline as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

pub(super) struct Capture {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    codec: EnvelopeCodec,
    output: Option<Output>,
}
impl Capture {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
    ) -> anyhow::Result<Self> {
        let configuration = Configuration::parse(configuration)?;
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("constant"));
        codec.register_schema(GraphChangeCodec::schema())?;
        Ok(Self {
            descriptor,
            configuration,
            codec,
            output: None,
        })
    }
}
#[async_trait]
impl NativeComponent for Capture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.configuration)?)
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn start(&mut self, _ctx: &mut StartContext) -> LifecycleResult {
        Ok(self.start_activation().await?)
    }
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        Ok(self.stop_activation().await?)
    }
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        nothing_to_do()
    }
}

impl Capture {
    async fn start_activation(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.output.is_none(), "capture already started");
        self.output =
            Some(Output::open(&self.configuration.path, self.configuration.append).await?);
        Ok(())
    }
    async fn stop_activation(&mut self) -> anyhow::Result<()> {
        if let Some(output) = &mut self.output {
            output.repair().await?;
            output.file.flush().await?;
        }
        self.output = None;
        Ok(())
    }
}
#[async_trait]
impl NativeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    /// Appends one newline-terminated frame. A write that fails (for example
    /// on a full disk) or is cancelled is cut back before the next write, so
    /// the retried input never follows a partial frame.
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(input.port.as_str() == "in", "unknown capture input port");
        let output = self
            .output
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("capture is not running"))?;
        let mut bytes = self.codec.encode(&input.envelope)?.to_vec();
        bytes.push(b'\n');
        output.write_frame(&bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, GraphChangeCodec, PortId, StreamId};
    use drasi_computation_plugin_sdk::testing::contexts;
    use drasi_computation_plugin_sdk::Factory;
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use std::sync::Arc;

    const SOURCE: &str = "5c3e49cc-32f8-5b75-888d-30b75b572221";

    fn input(sequence: u64, payload_bytes: usize) -> anyhow::Result<InputEnvelope> {
        let mut properties = ElementPropertyMap::new();
        properties.insert(
            "payload",
            ElementValue::String(Arc::from("x".repeat(payload_bytes))),
        );
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, "one"),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1,
            },
            properties,
        };
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_changes(
                &[SourceChange::Insert { element }],
                StreamId::try_new(format!("{SOURCE}/out"))?,
                sequence,
                None,
            )?,
        })
    }

    fn frames(path: &std::path::Path, codec: &EnvelopeCodec) -> anyhow::Result<Vec<u64>> {
        let bytes = std::fs::read(path)?;
        anyhow::ensure!(
            bytes.last().is_none_or(|byte| *byte == b'\n'),
            "partial tail"
        );
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| Ok(codec.decode(line)?.system().sequence()))
            .collect()
    }

    /// A write cancelled midway through a frame (as by Abort or a deadline)
    /// leaves no partial frame in front of the redelivered input, and a
    /// restart in append mode drops a partial tail left by a crash (CPA-1).
    #[tokio::test(flavor = "multi_thread")]
    async fn interrupted_writes_never_leave_a_partial_frame() -> anyhow::Result<()> {
        let directory = std::env::temp_dir().join(format!("capture-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("frames.jsonl");
        let configuration = serde_json::json!({"path": path, "append": true});
        let mut sink = Capture::new(
            crate::CaptureFactory
                .metadata()
                .descriptor(ComponentId::try_new(
                    "b11e8f74-9ac7-5734-b9b8-d9fd4c356f13",
                )?)?,
            configuration,
        )?;
        sink.start(&mut contexts::start()).await?;
        sink.handle(input(1, 16)?).await?;
        // Larger than one buffered file write, so the frame is still partial
        // when the cancelled future is dropped.
        let large = input(2, 8 * 1024 * 1024)?;
        assert!(
            tokio::time::timeout(std::time::Duration::ZERO, sink.handle(large.clone()))
                .await
                .is_err()
        );
        sink.handle(large).await?;
        assert_eq!(frames(&path, &sink.codec)?, [1, 2]);

        // A crash mid-frame leaves a partial tail; restarting in append mode
        // continues from the last complete frame.
        sink.stop(&contexts::stop(
            drasi_computation_plugin_sdk::lifecycle::StopMode::Drain,
        ))
        .await?;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"{\"partial"))?;
        sink.start(&mut contexts::start()).await?;
        sink.handle(input(3, 16)?).await?;
        assert_eq!(frames(&path, &sink.codec)?, [1, 2, 3]);
        std::fs::remove_dir_all(&directory)?;
        Ok(())
    }
}
