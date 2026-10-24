// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};

use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, EnvelopeCodec, GraphChangeCodec, InputEnvelope, SinkCompletion,
};
use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;
use tokio::io::AsyncWriteExt;

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
    ])
    .expect("constant capture configuration")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    path: String,
    #[serde(default)]
    append: bool,
}
pub(super) struct Capture {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    codec: EnvelopeCodec,
    file: Option<tokio::fs::File>,
}
impl Capture {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
    ) -> anyhow::Result<Self> {
        let configuration: Configuration = serde_json::from_value(configuration)?;
        anyhow::ensure!(
            !configuration.path.is_empty(),
            "capture path must not be empty"
        );
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("constant"));
        codec.register_schema(GraphChangeCodec::schema())?;
        Ok(Self {
            descriptor,
            configuration,
            codec,
            file: None,
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
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.file.is_none(), "capture already started");
        self.file = Some(
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(self.configuration.append)
                .truncate(!self.configuration.append)
                .open(&self.configuration.path)
                .await?,
        );
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush().await?;
        }
        self.file = None;
        Ok(())
    }
}
#[async_trait]
impl NativeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        anyhow::ensure!(input.port.as_str() == "in", "unknown capture input port");
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("capture is not running"))?;
        let mut bytes = self.codec.encode(&input.envelope)?.to_vec();
        bytes.push(b'\n');
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(())
    }
}
