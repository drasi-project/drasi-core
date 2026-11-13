// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A real non-query native transformer: retain an input, then emit it when due.
//! Request state, business state, completion and retained output use one owner.

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use drasi_computation_contracts::{
    emission_id, BinaryEnvelopeCodec, ChangeEnvelope, ComponentDescriptor, ComponentRole,
    GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress, ImplementationIdentity,
    InputEnvelope, OutputEnvelope, OutputProgress, PipeRequirements, PortDescriptor, PortDirection,
    PortId, ScheduledRequest, StreamId, SystemMetadata, WakeupSource,
};
use drasi_computation_plugin_sdk as sdk;
use drasi_core_types::{
    interface::OutboxPageLimits,
    models::{ElementValue, SourceChange},
};

pub const IMPLEMENTATION: &str = "fixture/scheduled-delay";
pub const COMPLETED: &str = "delay/completed";
pub const PRODUCER: &str = "delay/producer";
const EMISSION: &str = "delay/emission";

pub fn plugin() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "fixture/native-scheduling",
        "1",
        vec![Arc::new(Factory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(not(test))]
sdk::export_computation_plugin!(plugin());

pub struct Factory;

impl sdk::Factory for Factory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        sdk::FactoryMetadata {
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                .expect("fixture identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: sdk::ConfigSchema::from_fields([
                ("delay_ms", sdk::ConfigType::Integer, true),
                ("fail_input", sdk::ConfigType::Boolean, false),
                ("fail_due", sdk::ConfigType::Boolean, false),
            ]),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).expect("fixture port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: sdk::Capabilities {
                indexed_processing: true,
                scheduled_processing: true,
                committed_input_progress: Some(PortId::try_new("in").expect("fixture input")),
                durable_output: true,
                continuations: true,
                startup_continuation: true,
                wakeups: true,
                output_progress: Some(OutputProgress::Origin),
                ..Default::default()
            },
        }
    }

    fn create(
        &self,
        _: &sdk::CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        anyhow::bail!("scheduled delay requires the negotiated processing service")
    }

    fn create_with_processing(
        &self,
        request: &sdk::CreateRequest,
        _: sdk::ControlSender,
        processing: sdk::NativeProcessing,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        let scheduling = processing
            .scheduling()
            .context("missing scheduling service")?;
        Ok(sdk::Component::Transformer(Box::new(Delay {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            scope: request.scope.clone().context("missing processing scope")?,
            delay_ms: request.configuration["delay_ms"]
                .as_u64()
                .context("invalid delay")?,
            fail_input: request
                .configuration
                .get("fail_input")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            fail_due: request
                .configuration
                .get("fail_due")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            codec: Arc::new(sdk::wire::codec(&[GraphChangeCodec::schema()])?),
            processing,
            scheduling,
            producer: None,
            pending: None,
            replay: false,
        }))
        .into())
    }
}

struct Delay {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    scope: sdk::Scope,
    delay_ms: u64,
    fail_input: bool,
    fail_due: bool,
    codec: Arc<BinaryEnvelopeCodec>,
    processing: sdk::NativeProcessing,
    scheduling: sdk::NativeScheduling,
    producer: Option<GraphProducerIdentity>,
    pending: Option<ChangeEnvelope>,
    replay: bool,
}

fn page() -> OutboxPageLimits {
    OutboxPageLimits {
        max_records: NonZeroUsize::new(1).expect("one retained output"),
        max_bytes: NonZeroUsize::new(1024 * 1024).expect("bounded output"),
    }
}

async fn number(context: &sdk::NativeProcessingContext, key: &str) -> anyhow::Result<u64> {
    Ok(context
        .read_checkpoint(key)
        .await?
        .map_or(0, |checkpoint| checkpoint.sequence))
}

#[async_trait]
impl sdk::NativeComponent for Delay {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        let scope = self.scope.clone();
        let component = self.descriptor.id().clone();
        let stream = StreamId::try_new("delay/out")?;
        let (producer, pending): (GraphProducerIdentity, Option<Vec<u8>>) = self
            .processing
            .run(move |context| async move {
                context.recover_input_progress().await?;
                context.stage_bootstrap_complete(true).await?;
                let producer = match context.read_checkpoint(PRODUCER).await? {
                    Some(checkpoint) => {
                        anyhow::ensure!(checkpoint.sequence == 1, "invalid delay producer version");
                        let producer: GraphProducerIdentity = serde_json::from_slice(
                            &checkpoint
                                .source_position
                                .context("missing delay producer")?,
                        )?;
                        anyhow::ensure!(
                            producer.graph_id() == scope.graph_id
                                && producer.component_id() == &component
                                && producer.stream() == &stream,
                            "delay producer belongs to another component"
                        );
                        producer
                    }
                    None => {
                        let producer = GraphProducerIdentity::new(
                            scope.instance_id,
                            scope.graph_id,
                            component.clone(),
                            stream,
                            true,
                        )?;
                        context
                            .stage_checkpoint(PRODUCER, 1, Some(&serde_json::to_vec(&producer)?))
                            .await?;
                        producer
                    }
                };
                let head = context.read_result_sequence().await?;
                let delivered = context.read_delivery_progress().await?;
                if head.is_none() {
                    context.stage_result_sequence(0).await?;
                    context.stage_delivery_progress(0).await?;
                }
                let pending = if head.unwrap_or(0) > delivered.unwrap_or(0) {
                    let mut page = context
                        .read_outbox_page(delivered.unwrap_or(0), page())
                        .await?;
                    anyhow::ensure!(
                        page.len() == 1 && Some(page[0].0) == head,
                        "missing retained delay output"
                    );
                    Some(page.remove(0).1)
                } else {
                    None
                };
                Ok((producer, pending))
            })
            .await?;
        self.producer = Some(producer);
        self.pending = pending.map(|bytes| self.codec.decode(&bytes)).transpose()?;
        self.replay = self.pending.is_some();
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.pending = None;
        self.producer = None;
        self.replay = false;
        Ok(())
    }
}

#[async_trait]
impl sdk::NativeTransformer for Delay {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.scheduling.wakeup_source())
    }

    fn has_pending_emissions(&self) -> bool {
        self.replay
    }

    fn resume_delivery(&mut self) -> anyhow::Result<()> {
        self.replay = self.pending.is_some();
        Ok(())
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let codec = self.codec.clone();
        let delay = self.delay_ms;
        let fail = self.fail_input;
        self.processing
            .run(move |context| async move {
                let decision = context.prepare_input_progress(&input).await?;
                if decision.duplicate {
                    context.stage_prepared_input_progress().await?;
                    return Ok(());
                }
                let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
                let [SourceChange::Insert { element }] = changes.as_slice() else {
                    anyhow::bail!("delay fixture accepts one input element");
                };
                let key = element.get_reference().element_id.to_string();
                let action = match element.get_property("action") {
                    ElementValue::String(action) => action.as_ref(),
                    _ => "request",
                };
                let previous = context.scheduled_work(&key).await?;
                if action == "cancel" {
                    context
                        .cancel_scheduled_work(
                            &previous.context("missing cancellation target")?.token,
                        )
                        .await?;
                    context.delete_element(element.get_reference()).await?;
                } else {
                    let request = ScheduledRequest {
                        key,
                        due_time_ms: context
                            .physical_time()
                            .await?
                            .checked_add(delay)
                            .context("deadline overflow")?,
                        payload: codec.encode(&input.envelope)?,
                    };
                    if action == "replace" {
                        context
                            .replace_scheduled_work(
                                &previous.context("missing replacement target")?.token,
                                request,
                            )
                            .await?;
                    } else {
                        anyhow::ensure!(action == "request", "unknown delay action");
                        context.request_scheduled_work(request).await?;
                    }
                    context.set_element(element, &[0]).await?;
                }
                context.stage_prepared_input_progress().await?;
                anyhow::ensure!(!fail, "intentional delay input rollback");
                Ok(())
            })
            .await?;
        Ok(Vec::new())
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.pending.is_none(),
            "confirm retained output before more due work"
        );
        let codec = self.codec.clone();
        let producer = self.producer.clone().context("delay is not started")?;
        let fail = self.fail_due;
        let bytes: Option<Vec<u8>> = self
            .processing
            .run_output(move |context| async move {
                let Some(work) = context.claim_due_scheduled_work().await? else {
                    return Ok(None);
                };
                let input = codec.decode(&work.payload)?;
                let sequence = context
                    .read_result_sequence()
                    .await?
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("delay output sequence exhausted")?;
                let emission = number(&context, EMISSION)
                    .await?
                    .checked_add(1)
                    .context("delay emission sequence exhausted")?;
                let timestamp =
                    chrono::DateTime::from_timestamp_millis(i64::try_from(work.due_time_ms)?)
                        .context("delay deadline is out of timestamp range")?;
                let mut output = input.try_derive(
                    emission_id(producer.stream(), emission)?,
                    input.changes().clone(),
                    SystemMetadata::new(producer.stream().clone(), emission)
                        .with_timestamp(timestamp),
                )?;
                GraphProducerProgress::annotate(&mut output, &producer, sequence)?;
                let bytes = codec.encode(&output)?;
                context
                    .append_outbox_and_trim(sequence, &bytes, sequence)
                    .await?;
                context.stage_result_sequence(sequence).await?;
                context.stage_checkpoint(EMISSION, emission, None).await?;
                let completed = number(&context, COMPLETED)
                    .await?
                    .checked_add(1)
                    .context("delay completion count exhausted")?;
                context.stage_checkpoint(COMPLETED, completed, None).await?;
                context.complete_scheduled_work(&work.token).await?;
                anyhow::ensure!(!fail, "intentional delay completion rollback");
                Ok(Some(bytes))
            })
            .await?;
        self.pending = bytes.map(|bytes| self.codec.decode(&bytes)).transpose()?;
        Ok(self
            .pending
            .clone()
            .map(|envelope| OutputEnvelope {
                port: PortId::try_new("out").expect("fixture output"),
                envelope,
            })
            .into_iter()
            .collect())
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        if !self.replay {
            return Ok(Vec::new());
        }
        let emission: u64 = self
            .processing
            .run(|context| async move {
                let emission = number(&context, EMISSION)
                    .await?
                    .checked_add(1)
                    .context("delay emission sequence exhausted")?;
                context.stage_checkpoint(EMISSION, emission, None).await?;
                Ok(emission)
            })
            .await?;
        let output = self
            .pending
            .as_ref()
            .context("missing delay replay")?
            .reemit(emission)?;
        self.pending = Some(output.clone());
        self.replay = false;
        Ok(vec![OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: output,
        }])
    }

    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        if outputs.is_empty() {
            return Ok(());
        }
        let [output] = outputs else {
            anyhow::bail!("delay confirms one output at a time")
        };
        let pending = self.pending.as_ref().context("no pending delay output")?;
        anyhow::ensure!(
            self.codec.encode(&output.envelope)? == self.codec.encode(pending)?,
            "delay confirmation differs from its retained output"
        );
        let sequence = GraphProducerProgress::from_envelope(&output.envelope)?
            .context("missing delay producer progress")?
            .sequence();
        self.processing
            .run(move |context| async move {
                context.stage_delivery_progress(sequence).await?;
                Ok(())
            })
            .await?;
        self.pending = None;
        Ok(())
    }
}
