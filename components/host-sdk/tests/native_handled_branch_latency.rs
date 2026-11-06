// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "../../../lib/tests/handled_branch_latency.rs"]
mod pipeline;
mod telemetry_support;

#[tokio::test(start_paused = true)]
async fn real_native_retained_input_and_reply_preserve_source_to_sink_duration(
) -> anyhow::Result<()> {
    let path = telemetry_support::fixture("handled_branch_fixture");
    anyhow::ensure!(
        path.is_file(),
        "Build SDK handled_branch_fixture before running this test"
    );
    let plugin = drasi_host_sdk::computation::load(path)?;
    let transformer = plugin.factories()[0].create_component(
        drasi_lib::computation::v1::ComponentId::try_new("native")?,
        serde_json::json!({}),
    )?;
    pipeline::native_transformer_pipeline(Box::new(transformer), true).await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn v2_only_native_transformer_delivers_data_without_fabricated_metrics_only_continuity(
) -> anyhow::Result<()> {
    let path = telemetry_support::fixture("handled_branch_v2_fixture");
    anyhow::ensure!(
        path.is_file(),
        "Build SDK handled_branch_v2_fixture before running this test"
    );
    let plugin = drasi_host_sdk::computation::load(path)?;
    let transformer = plugin.factories()[0].create_component(
        drasi_lib::computation::v1::ComponentId::try_new("native")?,
        serde_json::json!({}),
    )?;
    pipeline::native_transformer_pipeline(Box::new(transformer), false).await;
    Ok(())
}
