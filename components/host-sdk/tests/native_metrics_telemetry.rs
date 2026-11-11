// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod telemetry_support;

#[test]
fn actual_native_metrics_and_logs_without_otel() -> anyhow::Result<()> {
    for multi_thread in [false, true] {
        let runtime = if multi_thread {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
        };
        runtime.block_on(telemetry_support::metrics_contract())?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn required_native_metrics_fail_before_a_context_only_constructor() -> anyhow::Result<()> {
    use drasi_host_sdk::computation;
    use drasi_lib::{computation::v1::*, observability::*};
    use telemetry_support::*;
    let plugin = computation::load(fixture("execution_context_fixture"))?;
    let metrics = RecordingMetrics::default();
    let logging = ComponentLogRuntime::new(LogPolicy::new("off")?)?;
    let graph = graph("required", &metrics, logging.clone(), None, false)?;
    let bound = binding(&graph, "required", 1, logging.clone())?;
    let result = ExecutionScope::new(Default::default())
        .with_plugin_telemetry(Some(bound))
        .in_scope(|| {
            plugin.factories()[0]
                .create_component(ComponentId::try_new(PROBE_ID)?, serde_json::json!({}))
        });
    let error = result
        .err()
        .expect("required metrics must fail before construction");
    assert!(matches!(
        error.downcast_ref::<TelemetryError>(),
        Some(TelemetryError::Unsupported(_))
    ));
    drop(graph);
    logging.shutdown(std::time::Duration::from_secs(2)).await?;
    Ok(())
}
