// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Runs the plugin SDK conformance kit through built dynamic libraries, so the
//! scenarios cover the exported C ABI as well as the linked factory.

#![cfg(feature = "test-support")]

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{InputEnvelope, PortId};
use drasi_computation_plugin_sdk::lifecycle::UpdateBoundary;
use drasi_computation_plugin_sdk::testing::{
    Offer, SinkConformance, SinkProbe, SourceConformance, SourceProbe, STEP_TIMEOUT,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_host_sdk::computation::conformance::LoadedTarget;
use drasi_lib::computation::v1::{GraphChangeCodec, StreamId};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SOURCE_ID: &str = "00000000-0000-4000-8000-0000000000c1";
const SINK_ID: &str = "00000000-0000-4000-8000-0000000000c2";
const HTTP_SOURCE: &str = "drasi.network/http-source";
const CAPTURE: &str = "drasi.standard/capture";

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace")
        .to_owned()
}

/// The staged dynamic library for `package`; see `make build-test-plugins`.
fn library(package: &str) -> Result<PathBuf> {
    let target = match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace().join(path),
        None => workspace().join("target"),
    };
    let path = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug/plugins"))
        .join(format!(
            "{}{}{}",
            std::env::consts::DLL_PREFIX,
            package.replace('-', "_"),
            std::env::consts::DLL_SUFFIX
        ));
    anyhow::ensure!(
        path.is_file(),
        "missing native plugin at {}; run make build-test-plugins (or set DRASI_NATIVE_PLUGIN_DIR)",
        path.display()
    );
    Ok(path)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn http_source() -> Result<SourceConformance<'static, LoadedTarget>> {
    SourceConformance::with_target(
        LoadedTarget::load(library("drasi-computation-source-http")?, HTTP_SOURCE)?,
        SOURCE_ID,
    )
}

fn http_configuration() -> Value {
    json!({ "stream": "events", "port": free_port() })
}

struct HttpProbe;

#[async_trait]
impl SourceProbe for HttpProbe {
    async fn offer(&self, configuration: &Value) -> Result<Offer> {
        let url = format!(
            "http://{}:{}/sources/{SOURCE_ID}/events",
            configuration["host"].as_str().unwrap_or("127.0.0.1"),
            configuration["port"]
        );
        let event = json!({"operation":"insert","timestamp":1_700_000_000_000_000_000u64,
            "element":{"type":"node","id":"e1","labels":["Item"],"properties":{"n":1}}});
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?
            .post(url)
            .json(&event)
            .send()
            .await;
        match response {
            Err(error) if error.is_connect() => Ok(Offer::Refused),
            Err(error) => Err(error.into()),
            Ok(response) if response.status().is_success() => Ok(Offer::Accepted),
            Ok(response) if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                Ok(Offer::Refused)
            }
            Ok(response) => anyhow::bail!("unexpected status {}", response.status()),
        }
    }
}

fn capture_sink() -> Result<SinkConformance<'static, LoadedTarget>> {
    SinkConformance::with_target(
        LoadedTarget::load(library("drasi-computation-sink-capture")?, CAPTURE)?,
        SINK_ID,
        Vec::new(),
    )
}

/// Observes the capture sink's JSON-lines file.
struct CaptureProbe {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl CaptureProbe {
    fn new() -> Result<Self> {
        let root = workspace().join("target/native-computation-tests");
        std::fs::create_dir_all(&root)?;
        let directory = tempfile::Builder::new()
            .prefix("conformance-")
            .tempdir_in(root)?;
        let path = directory.path().join("capture.jsonl");
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    /// Appends across restarts, so deliveries accumulate.
    fn configuration(&self) -> Value {
        json!({ "path": self.path, "append": true })
    }

    fn lines(&self) -> usize {
        std::fs::read_to_string(&self.path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
}

#[async_trait]
impl SinkProbe for CaptureProbe {
    fn input(&self, sequence: u64) -> Result<InputEnvelope> {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("conformance", &format!("row-{sequence}")),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::from(json!({ "n": sequence })),
            },
        };
        Ok(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_change(
                change,
                StreamId::try_new("conformance/out")?,
                sequence,
                None,
            )?,
            execution_context: Default::default(),
        })
    }

    async fn expect_delivered(&self, _configuration: &Value, count: usize) -> Result<()> {
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        while self.lines() < count {
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "capture file holds {} of {count} deliveries",
                self.lines()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
}

macro_rules! both_flavors {
    ($name:ident, $body:expr) => {
        mod $name {
            use super::*;

            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() -> Result<()> {
                $body.await
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() -> Result<()> {
                $body.await
            }
        }
    };
}

both_flavors!(loaded_http_source_lifecycle, async {
    http_source()?
        .lifecycle(http_configuration(), &HttpProbe)
        .await
        .context("loaded HTTP source lifecycle")
});

both_flavors!(loaded_http_source_cancelled_start, async {
    http_source()?
        .cancelled_start(http_configuration(), &HttpProbe)
        .await
        .context("loaded HTTP source cancelled start")
});

both_flavors!(loaded_http_source_dispose_and_erase, async {
    let suite = http_source()?;
    suite.dispose_matrix(http_configuration()).await?;
    suite.erase(http_configuration()).await
});

#[test]
fn loaded_http_source_configuration_and_assessment() -> Result<()> {
    let suite = http_source()?;
    let current = http_configuration();
    suite.configuration_round_trip(std::slice::from_ref(&current))?;
    let mut timeout = current.clone();
    timeout["timeoutMs"] = json!(5000);
    suite.assess_matrix(
        &current,
        &[
            (current.clone(), Some(UpdateBoundary::Pause)),
            (timeout, Some(UpdateBoundary::Pause)),
        ],
    )
}

both_flavors!(loaded_capture_sink_lifecycle, async {
    let probe = CaptureProbe::new()?;
    capture_sink()?
        .lifecycle(probe.configuration(), &probe)
        .await
        .context("loaded capture sink lifecycle")
});

both_flavors!(loaded_capture_sink_dispose_matrix, async {
    let probe = CaptureProbe::new()?;
    capture_sink()?.dispose_matrix(probe.configuration()).await
});
