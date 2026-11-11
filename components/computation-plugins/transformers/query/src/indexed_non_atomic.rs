// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_query_common::{QueryPublicationCheckpoint, PENDING_QUERY_OUTPUT};

pub(super) fn configured(request: &CreateRequest) -> Result<bool> {
    match request.configuration.get("publication") {
        None => Ok(false),
        Some(serde_json::Value::String(value)) if value == "atomic" => Ok(false),
        Some(serde_json::Value::String(value)) if value == "non_atomic" => Ok(true),
        _ => anyhow::bail!("publication must be 'atomic' or 'non_atomic'"),
    }
}

pub(super) async fn stage(
    context: &NativeProcessingContext,
    state: QueryPublicationCheckpoint,
) -> Result<()> {
    let checkpoint = state.checkpoint();
    context
        .stage_checkpoint(
            PENDING_QUERY_OUTPUT,
            checkpoint.sequence,
            checkpoint.source_position.as_deref(),
        )
        .await?;
    Ok(())
}

pub(super) async fn begin(processing: &NativeProcessing) -> Result<()> {
    processing
        .run(|context| async move { stage(&context, QueryPublicationCheckpoint::Evaluating).await })
        .await
}

pub(super) async fn begin_input(
    processing: &NativeProcessing,
    input: InputEnvelope,
) -> Result<bool> {
    processing
        .run(move |context| async move {
            if context.validate_input_progress(&input).await?.duplicate {
                anyhow::ensure!(
                    context.prepare_input_progress(&input).await?.duplicate,
                    "duplicate input changed during publication preparation"
                );
                return Ok(true);
            }
            let _ = drasi_query_common::prepare_query_input::<false>(&input.envelope, None)?
                .into_changes(&input.envelope)?;
            QueryOperation::new(&input.envelope)?;
            stage(&context, QueryPublicationCheckpoint::Evaluating).await?;
            Ok(false)
        })
        .await
}

pub(super) async fn finish(
    processing: &NativeProcessing,
    state: Arc<QueryOutput>,
    projector: ResultNodeProjector,
    retention: u64,
    prepared: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    processing
        .run_output(move |context| async move {
            let pending = context
                .read_checkpoint(PENDING_QUERY_OUTPUT)
                .await?
                .context("non-atomic evaluation has no publication marker")?;
            let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
            let output = prepared.map(|bytes| codec.decode(&bytes)).transpose()?;
            let expected = match &output {
                Some(output) => {
                    QueryPublicationCheckpoint::Pending(QueryChangeCodec::query_sequence(output)?)
                }
                None => QueryPublicationCheckpoint::Evaluating,
            }
            .checkpoint();
            anyhow::ensure!(
                pending.sequence == expected.sequence
                    && pending.source_position == expected.source_position,
                "non-atomic output does not match the committed evaluation"
            );
            context.resume_input_progress().await?;
            let output = match output {
                Some(output) => Some(
                    stage_output(
                        &context, &state, &projector, output, retention, false, false,
                    )
                    .await?,
                ),
                None => None,
            };
            stage(&context, QueryPublicationCheckpoint::Complete).await?;
            Ok(output)
        })
        .await
}

impl DurableQuery {
    pub(super) async fn prepare_non_atomic_reset(
        &self,
        processing: &NativeProcessing,
        force: bool,
    ) -> Result<()> {
        let configuration = self.configuration.clone();
        let candidate = self.producer.clone();
        let policy = self.recovery;
        let bootstrap = self.bootstrap_configured;
        let reset_configuration = self.reset_configuration;
        processing
            .run(move |context| async move {
                let profile = context.state_info().await?;
                anyhow::ensure!(
                    profile.non_atomic_publication
                        && !profile.volatile
                        && profile.persistent_checkpoints,
                    "non-atomic query requires its actual persistent publication profile"
                );
                if recovery::reset_required(
                    &context,
                    &configuration,
                    &candidate,
                    recovery::ResetIntent {
                        policy,
                        bootstrap_configured: bootstrap,
                        force,
                        reset_configuration,
                        update_operation: None,
                    },
                )
                .await?
                .is_some()
                {
                    context.begin_non_atomic_reset(true).await?;
                }
                Ok(())
            })
            .await
    }
}
