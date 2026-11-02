// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use drasi_bootstrap_noop::NoOpBootstrapProvider;
use drasi_core::interface::IndexBackendPlugin;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::BootstrapEventSender;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::wal::WalProvider;
use drasi_lib::{
    CapacityPolicy, DrasiLib, DurabilityConfig, Query, RecoveryPolicy, Source, StorageBackendRef,
};
use drasi_source_grpc::GrpcSourceBuilder;
use drasi_source_http::HttpSourceBuilder;
use drasi_source_otel::OtelSource;
use drasi_wal_redb::RedbWalProvider;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const SOURCE: &str = "ingress";
const QUERY: &str = "query";
const PHASE_ENV: &str = "DRASI_INGRESS_RECOVERY_PHASE";
const SOURCE_ENV: &str = "DRASI_INGRESS_RECOVERY_SOURCE";
const POLICY_ENV: &str = "DRASI_INGRESS_RECOVERY_POLICY";
const ROOT_ENV: &str = "DRASI_INGRESS_RECOVERY_ROOT";

struct RecordingNoOp {
    marker: PathBuf,
}

#[async_trait]
impl BootstrapProvider for RecordingNoOp {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let mut marker = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.marker)?;
        marker.write_all(b"bootstrap\n")?;
        marker.sync_all()?;
        NoOpBootstrapProvider::new()
            .bootstrap(request, context, event_tx, settings)
            .await
    }
}

fn durability() -> DurabilityConfig {
    DurabilityConfig {
        enabled: true,
        max_events: 128,
        capacity_policy: CapacityPolicy::RejectIncoming,
    }
}

async fn run_query_phase(
    kind: &str,
    policy: RecoveryPolicy,
    root: &Path,
    restart: bool,
) -> Result<()> {
    let provider = RecordingNoOp {
        marker: root.join("bootstrap-calls"),
    };
    let builder = DrasiLib::builder()
        .with_id("durable-ingress-recovery")
        .with_query(
            Query::cypher(QUERY)
                .query("MATCH (n:Node) RETURN n.id AS id")
                .from_source(SOURCE)
                .auto_start(false)
                .enable_bootstrap(true)
                .with_storage_backend(StorageBackendRef::Named("persistent".into()))
                .with_recovery_policy(policy)
                .build(),
        )
        .with_index_provider(
            "persistent",
            Arc::new(RocksDbIndexProvider::new(root.join("rocks"), false, false)),
        )
        .with_wal_provider(Arc::new(RedbWalProvider::new(root.join("wal"))));
    let builder = match kind {
        "http" => builder.with_source(
            HttpSourceBuilder::new(SOURCE)
                .with_host("127.0.0.1")
                .with_port(0)
                .with_durability(durability())
                .with_bootstrap_provider(provider)
                .build()?,
        ),
        "grpc" => builder.with_source(
            GrpcSourceBuilder::new(SOURCE)
                .with_host("127.0.0.1")
                .with_port(0)
                .with_durability(durability())
                .with_bootstrap_provider(provider)
                .build()?,
        ),
        "otel" => {
            let source = OtelSource::builder(SOURCE)
                .with_grpc_bind("127.0.0.1:0")
                .with_durability(durability())
                .build()?;
            source.set_bootstrap_provider(Box::new(provider)).await;
            builder.with_source(source)
        }
        _ => bail!("unknown source {kind}"),
    };
    let core = builder.build().await?;
    core.start().await?;
    core.start_query(QUERY).await?;
    let query = core
        .query_manager()
        .get_query_instance(QUERY)
        .await
        .map_err(anyhow::Error::msg)?;
    if restart {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = query.fetch_snapshot().await?;
                if snapshot.as_of_sequence == 1 {
                    assert_eq!(
                        snapshot.to_vec(),
                        vec![serde_json::json!({"id": "replayed"})]
                    );
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("WAL event was not replayed")??;
    } else {
        let snapshot = query.fetch_snapshot().await?;
        assert_eq!(snapshot.as_of_sequence, 0);
        assert!(snapshot.to_vec().is_empty());
    }
    drop(query);
    core.shutdown().await?;
    Ok(())
}

async fn append_wal_event(root: &Path) -> Result<()> {
    let provider = RocksDbIndexProvider::new(root.join("rocks"), false, false);
    let indexes = provider.create_indexes(QUERY).await?;
    let checkpoints = indexes
        .checkpoint_store
        .as_ref()
        .context("checkpoint store")?;
    assert_eq!(checkpoints.read_result_sequence(QUERY).await?, Some(0));
    assert!(checkpoints.read_checkpoint(SOURCE).await?.is_none());
    drop(indexes);
    drop(provider);

    let wal = RedbWalProvider::new(root.join("wal"));
    wal.register(SOURCE, durability().to_wal_config()).await?;
    let mut properties = ElementPropertyMap::new();
    properties.insert("id", ElementValue::String("replayed".into()));
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, "replayed"),
                labels: Arc::from(vec![Arc::from("Node")]),
                effective_from: 0,
            },
            properties,
        },
    };
    assert_eq!(wal.append(SOURCE, &change).await?, 1);
    Ok(())
}

#[tokio::test]
async fn durable_ingress_recovery_phase() -> Result<()> {
    let Ok(phase) = std::env::var(PHASE_ENV) else {
        return Ok(());
    };
    let kind = std::env::var(SOURCE_ENV)?;
    let policy = match std::env::var(POLICY_ENV)?.as_str() {
        "strict" => RecoveryPolicy::Strict,
        "auto_reset" => RecoveryPolicy::AutoReset,
        other => bail!("unknown recovery policy {other}"),
    };
    let root = PathBuf::from(std::env::var(ROOT_ENV)?);
    match phase.as_str() {
        "seed" => run_query_phase(&kind, policy, &root, false).await,
        "append" => append_wal_event(&root).await,
        "restart" => run_query_phase(&kind, policy, &root, true).await,
        other => bail!("unknown phase {other}"),
    }
}

fn check_recovery(kind: &str, policy: &str) -> Result<()> {
    let root = tempfile::tempdir()?;
    for phase in ["seed", "append", "restart"] {
        let output = Command::new(std::env::current_exe()?)
            .args(["--exact", "durable_ingress_recovery_phase", "--nocapture"])
            .env(PHASE_ENV, phase)
            .env(SOURCE_ENV, kind)
            .env(POLICY_ENV, policy)
            .env(ROOT_ENV, root.path())
            .output()?;
        if !output.status.success() {
            bail!(
                "{kind} {policy} {phase} failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let calls = std::fs::read_to_string(root.path().join("bootstrap-calls"))?;
    assert_eq!(calls.lines().count(), 1, "{kind} must not bootstrap twice");
    Ok(())
}

#[test]
fn http_strict() -> Result<()> {
    check_recovery("http", "strict")
}

#[test]
fn http_auto_reset() -> Result<()> {
    check_recovery("http", "auto_reset")
}

#[test]
fn grpc_strict() -> Result<()> {
    check_recovery("grpc", "strict")
}

#[test]
fn grpc_auto_reset() -> Result<()> {
    check_recovery("grpc", "auto_reset")
}

#[test]
fn otel_strict() -> Result<()> {
    check_recovery("otel", "strict")
}

#[test]
fn otel_auto_reset() -> Result<()> {
    check_recovery("otel", "auto_reset")
}
