// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_computation_contracts::BOOTSTRAP_STATE_CHECKPOINT;
use drasi_core::computation::ComputationIndexes;
use drasi_core::interface::{CheckpointStore, SourceCheckpoint};
use drasi_query_common::QueryResetMarker;
use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;

use super::progress::{Draft, RESET_GENERATION};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NonAtomicReset {
    state: QueryResetMarker,
    preserve_bootstrap: bool,
}

fn non_atomic_key(component: &str) -> String {
    format!("computation-processing-reset-v1:{component}")
}

async fn non_atomic_marker(
    resources: &ComputationIndexes,
    component: &str,
) -> Result<Option<NonAtomicReset>> {
    let rows = resources
        .outbox_writer()
        .context("missing reset intent store")?
        .read_page(
            &non_atomic_key(component),
            0,
            drasi_core::interface::OutboxPageLimits {
                max_records: NonZeroUsize::new(2).expect("positive record limit"),
                max_bytes: NonZeroUsize::new(4096).expect("positive byte limit"),
            },
        )
        .await?;
    match rows.as_slice() {
        [] => Ok(None),
        [(1, bytes)] => {
            anyhow::ensure!(
                bytes.len() <= 4096,
                "non-atomic reset intent exceeds its byte bound"
            );
            let marker: NonAtomicReset = serde_json::from_slice(bytes)?;
            anyhow::ensure!(
                marker.state.generation > 0,
                "invalid non-atomic reset generation"
            );
            Ok(Some(marker))
        }
        _ => anyhow::bail!("invalid retained non-atomic reset intent"),
    }
}

async fn write_non_atomic_marker(
    resources: &ComputationIndexes,
    component: &str,
    marker: &NonAtomicReset,
) -> Result<()> {
    resources
        .outbox_writer()
        .context("missing reset intent store")?
        .append(&non_atomic_key(component), 1, &serde_json::to_vec(marker)?)
        .await?;
    Ok(())
}

pub(super) async fn non_atomic_info(
    resources: &ComputationIndexes,
    component: &str,
) -> Result<Option<drasi_computation_plugin_sdk::processing::ProcessingResetInfo>> {
    Ok(non_atomic_marker(resources, component)
        .await?
        .map(
            |marker| drasi_computation_plugin_sdk::processing::ProcessingResetInfo {
                generation: marker.state.generation,
                in_progress: marker.state.in_progress,
            },
        ))
}

pub(super) async fn begin_non_atomic(
    resources: &ComputationIndexes,
    component: &str,
    progress: &Draft,
    preserve_bootstrap: bool,
) -> Result<u64> {
    anyhow::ensure!(
        progress.is_non_atomic(),
        "non-atomic reset was not negotiated"
    );
    progress.ensure_resettable()?;
    let store = progress.checkpoint_store(resources)?;
    let previous = non_atomic_marker(resources, component).await?;
    let generation = QueryResetMarker::next_generation(
        0,
        generation(store.as_ref()).await?,
        previous.as_ref().map(|marker| &marker.state),
    )?;
    let marker = NonAtomicReset {
        state: QueryResetMarker {
            sequence: store.read_result_sequence(component).await?.unwrap_or(0),
            in_progress: true,
            generation,
        },
        preserve_bootstrap,
    };
    write_non_atomic_marker(resources, component, &marker).await?;
    Ok(generation)
}

pub(super) async fn reset_non_atomic(
    resources: &ComputationIndexes,
    component: &str,
    progress: &Draft,
    preserve_bootstrap: bool,
) -> Result<u64> {
    anyhow::ensure!(
        progress.is_non_atomic(),
        "non-atomic reset was not negotiated"
    );
    progress.ensure_resettable()?;
    let marker = non_atomic_marker(resources, component)
        .await?
        .context("non-atomic reset intent must commit before clearing state")?;
    anyhow::ensure!(
        marker.state.in_progress && marker.preserve_bootstrap == preserve_bootstrap,
        "non-atomic reset does not match its committed intent"
    );
    let store = progress.checkpoint_store(resources)?;
    let (_, bootstrap) = metadata(store.as_ref(), preserve_bootstrap).await?;
    store.stage_clear_checkpoints().await?;
    clear_indexes(resources).await?;
    resources
        .outbox_writer()
        .context("missing reset outbox")?
        .clear(component)
        .await?;
    resources
        .live_results_writer()
        .context("missing reset live results")?
        .clear(component)
        .await?;
    stage_metadata(
        store.as_ref(),
        component,
        progress,
        marker.state.generation,
        bootstrap,
    )
    .await
}

pub(super) async fn finish_non_atomic(
    resources: &ComputationIndexes,
    component: &str,
    progress: &Draft,
) -> Result<()> {
    anyhow::ensure!(
        progress.is_non_atomic(),
        "non-atomic reset was not negotiated"
    );
    let mut marker = non_atomic_marker(resources, component)
        .await?
        .context("non-atomic reset has no retained intent")?;
    anyhow::ensure!(
        marker.state.in_progress,
        "non-atomic reset is already complete"
    );
    let store = progress.checkpoint_store(resources)?;
    let generation = store
        .read_checkpoint(RESET_GENERATION)
        .await?
        .context("reset state has not committed")?;
    anyhow::ensure!(
        generation.sequence == marker.state.generation && generation.source_position.is_none(),
        "reset completion belongs to another generation"
    );
    if marker.preserve_bootstrap {
        let bootstrap = store
            .read_checkpoint(drasi_computation_contracts::BOOTSTRAP_COMPLETE_CHECKPOINT)
            .await?
            .context("reset bootstrap has not completed")?;
        anyhow::ensure!(
            bootstrap.sequence == 1 && bootstrap.source_position.is_none(),
            "reset bootstrap has not completed"
        );
    }
    marker.state.sequence = store
        .read_result_sequence(component)
        .await?
        .context("reset result head is missing")?;
    anyhow::ensure!(
        marker.preserve_bootstrap || marker.state.sequence == 0,
        "deprovision reset did not clear its result head"
    );
    marker.state.in_progress = false;
    write_non_atomic_marker(resources, component, &marker).await
}

async fn generation(store: &dyn CheckpointStore) -> Result<u64> {
    let generation = store.read_checkpoint(RESET_GENERATION).await?;
    if let Some(generation) = &generation {
        anyhow::ensure!(
            generation.sequence > 0 && generation.source_position.is_none(),
            "invalid reset generation"
        );
    }
    Ok(generation.map_or(0, |value| value.sequence))
}

async fn metadata(
    store: &dyn CheckpointStore,
    preserve_bootstrap: bool,
) -> Result<(u64, Option<SourceCheckpoint>)> {
    let generation = generation(store)
        .await?
        .checked_add(1)
        .context("processing reset generation exhausted")?;
    let bootstrap = if preserve_bootstrap {
        store.read_checkpoint(BOOTSTRAP_STATE_CHECKPOINT).await?
    } else {
        None
    };
    Ok((generation, bootstrap))
}

async fn clear_indexes(resources: &ComputationIndexes) -> Result<()> {
    resources.indexes().element_index.clear().await?;
    resources.indexes().archive_index.clear().await?;
    resources.indexes().result_index.clear().await?;
    resources.indexes().future_queue.clear().await?;
    Ok(())
}

async fn stage_metadata(
    store: &dyn CheckpointStore,
    component: &str,
    progress: &Draft,
    generation: u64,
    bootstrap: Option<SourceCheckpoint>,
) -> Result<u64> {
    if let Some(bootstrap) = bootstrap {
        store
            .stage_checkpoint(
                BOOTSTRAP_STATE_CHECKPOINT,
                bootstrap.sequence,
                bootstrap.source_position.as_ref(),
            )
            .await?;
    }
    store
        .stage_checkpoint(RESET_GENERATION, generation, None)
        .await?;
    store.stage_result_sequence(component, 0).await?;
    progress.reset(generation)?;
    Ok(generation)
}

pub(super) async fn reset_volatile(
    resources: &ComputationIndexes,
    component: &str,
    progress: &Draft,
    preserve_bootstrap: bool,
) -> Result<u64> {
    anyhow::ensure!(
        progress.is_volatile(),
        "volatile reset requires a volatile owner"
    );
    progress.ensure_resettable()?;
    let store = progress.checkpoint_store(resources)?;
    anyhow::ensure!(
        !store.is_persistent(),
        "volatile reset cannot erase persistent checkpoints"
    );
    let (generation, bootstrap) = metadata(store.as_ref(), preserve_bootstrap).await?;
    store.clear_checkpoints().await?;
    clear_indexes(resources).await?;
    stage_metadata(store.as_ref(), component, progress, generation, bootstrap).await
}

pub(super) async fn reset(
    resources: &ComputationIndexes,
    component: &str,
    progress: &Draft,
    preserve_bootstrap: bool,
) -> Result<u64> {
    progress.ensure_resettable()?;
    resources.atomic_result_transaction()?;
    let store = resources
        .checkpoint_store()
        .context("missing reset checkpoint store")?;
    let (generation, bootstrap) = metadata(store.as_ref(), preserve_bootstrap).await?;
    // This optional staged operation must succeed before any other clearing.
    store.stage_clear_checkpoints().await?;
    clear_indexes(resources).await?;
    resources
        .outbox_writer()
        .context("missing reset outbox")?
        .clear(component)
        .await?;
    resources
        .outbox_writer()
        .context("missing reset outbox")?
        .clear(&non_atomic_key(component))
        .await?;
    resources
        .live_results_writer()
        .context("missing reset live results")?
        .clear(component)
        .await?;
    stage_metadata(store.as_ref(), component, progress, generation, bootstrap).await
}
