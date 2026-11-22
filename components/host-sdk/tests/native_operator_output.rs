// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Operator-visible output from loaded plugin libraries reaches the host's
//! logger through the native logging extension: the log sink's default
//! `output: log` and the profiler's reports (LGA-1, PFA-1).
//!
//! This target installs its own process-wide logger, so it is a separate
//! test binary.

#[path = "common/hooks.rs"]
mod hooks;

use anyhow::{Context, Result};
use drasi_computation_contracts::{InputBinding, OutputEndpoint};
use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Once},
    time::Duration,
};

const SINK_ID: &str = "00000000-0000-0000-0000-0000000007a1";
const SOURCE_ID: &str = "00000000-0000-0000-0000-0000000007a2";

/// Every record any library forwarded to this host.
static RECORDS: Mutex<Vec<(log::Level, String, String)>> = Mutex::new(Vec::new());

struct HostLogger;

impl log::Log for HostLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        RECORDS.lock().expect("records").push((
            record.level(),
            record.target().to_owned(),
            record.args().to_string(),
        ));
    }
    fn flush(&self) {}
}

/// The host's logger must be in place before a library is loaded: the
/// library receives the host's level when it is loaded.
fn install_host_logger() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&HostLogger).expect("test host logger");
        log::set_max_level(log::LevelFilter::Info);
    });
}

fn records(target: &str) -> Vec<(log::Level, String)> {
    RECORDS
        .lock()
        .expect("records")
        .iter()
        .filter(|(_, record_target, _)| record_target == target)
        .map(|(level, _, message)| (*level, message.clone()))
        .collect()
}

fn library(package: &str) -> Result<Arc<NativePlugin>> {
    install_host_logger();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("workspace")?
        .to_owned();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug/plugins"));
    let path = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        path.is_file(),
        "missing {}; first run make build-test-plugins",
        path.display()
    );
    computation::load(path)
}

fn create(plugin: &NativePlugin, configuration: Value) -> Result<NativeComponentProxy> {
    let factory = &plugin.factories()[0];
    let inputs = if factory.metadata().capabilities.binds_inputs {
        vec![InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(ComponentId::try_new(SOURCE_ID)?, PortId::try_new("out")?),
            stream: StreamId::try_new("source/out")?,
        }]
    } else {
        Vec::new()
    };
    factory.create_component_with_inputs(ComponentId::try_new(SINK_ID)?, configuration, inputs)
}

fn input(sequence: u64) -> Result<InputEnvelope> {
    let mut properties = ElementPropertyMap::new();
    properties.insert(
        "name",
        drasi_core::models::ElementValue::String("Alice".into()),
    );
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(SOURCE_ID, "visible-change"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1,
        },
        properties,
    };
    Ok(InputEnvelope {
        execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            &[drasi_core::models::SourceChange::Insert { element }],
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_log_sink_default_output_reaches_the_host_logger() -> Result<()> {
    let plugin = library("drasi-computation-sink-log")?;
    let mut sink = create(
        &plugin,
        json!({"outputs": {"items": {"component": SOURCE_ID, "port": "out"}}}),
    )?;
    sink.initialize(&hooks::initialize_ctx()).await?;
    sink.start(&hooks::start_ctx()).await?;
    sink.handle(input(1)?).await?;
    let lines = records("drasi.log");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].0, log::Level::Info);
    assert!(
        lines[0].1.contains("visible-change") && lines[0].1.contains("Alice"),
        "{lines:?}"
    );
    sink.stop(&hooks::stop_ctx()).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_profiler_reports_reach_the_host_logger() -> Result<()> {
    let plugin = library("drasi-computation-sink-profiler")?;
    let mut sink = create(&plugin, json!({"reportIntervalSecs": 1}))?;
    sink.initialize(&hooks::initialize_ctx()).await?;
    sink.start(&hooks::start_ctx()).await?;
    let report = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(report) = records("drasi.profiler").into_iter().next() {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("no profiler report reached the host logger")?;
    assert_eq!(report.0, log::Level::Info);
    assert!(report.1.contains(SINK_ID), "{report:?}");
    sink.stop(&hooks::stop_ctx()).await?;
    Ok(())
}
