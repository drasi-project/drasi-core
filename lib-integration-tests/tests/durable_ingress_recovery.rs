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

//! A bootstrap-enabled persistent query over a durable (WAL-backed) ingress
//! source whose bootstrap snapshot is empty and cursorless. After that
//! bootstrap completes, input written while the query is down must be replayed
//! from the start of the WAL on restart, and the query must not bootstrap again,
//! under both recovery policies.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use drasi_bootstrap_noop::NoOpBootstrapProvider;
use drasi_core::interface::StorageScope;
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
    CapacityPolicy, ComponentId, ComponentName, DurabilityConfig, Query, RecoveryPolicy,
    StorageBackendRef,
};
use drasi_source_grpc::GrpcSourceBuilder;
use drasi_source_http::HttpSourceBuilder;
use drasi_source_otel::OtelSource;
use drasi_wal_redb::RedbWalProvider;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SOURCE: &str = "ingress";
const QUERY: &str = "query";

fn source_id() -> ComponentId {
    ComponentId::try_new("00000000-0000-0000-0000-0000000d0a01").expect("fixture UUID")
}

fn query_id() -> ComponentId {
    ComponentId::try_new("00000000-0000-0000-0000-0000000d0a02").expect("fixture UUID")
}

struct RecordingNoOp {
    calls: Arc<AtomicUsize>,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
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
    calls: &Arc<AtomicUsize>,
    restart: bool,
) -> Result<()> {
    let provider = RecordingNoOp {
        calls: calls.clone(),
    };
    let mut query = Query::cypher(ComponentName::try_new(QUERY)?)
        .with_id(query_id())
        .query("MATCH (n:Node) RETURN n.id AS id")
        .from_input_by_name(ComponentName::try_new(SOURCE)?)
        .auto_start(true)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named("persistent".into()))
        .with_recovery_policy(policy)
        .build()?;
    query.id = Some(query_id());
    let builder = lib_integration_tests::query_builder()
        .with_id("durable-ingress-recovery")
        .with_query(query)
        .with_index_provider(
            "persistent",
            Arc::new(RocksDbIndexProvider::new(root.join("rocks"), false, false)),
        )
        .with_wal_provider(Arc::new(RedbWalProvider::new(root.join("wal"))));
    let name = ComponentName::try_new(SOURCE)?;
    let builder = match kind {
        "http" => builder.with_source(
            name,
            HttpSourceBuilder::new(source_id())
                .with_host("127.0.0.1")
                .with_port(0)
                .with_durability(durability())
                .with_bootstrap_provider(provider)
                .build()?,
        ),
        "grpc" => builder.with_source(
            name,
            GrpcSourceBuilder::new(source_id())
                .with_host("127.0.0.1")
                .with_port(0)
                .with_durability(durability())
                .with_bootstrap_provider(provider)
                .build()?,
        ),
        "otel" => {
            let source = OtelSource::builder(source_id())
                .with_grpc_bind("127.0.0.1:0")
                .with_durability(durability())
                .build()?;
            drasi_lib::Source::set_bootstrap_provider(&source, Box::new(provider)).await;
            builder.with_source(name, source)
        }
        _ => bail!("unknown source {kind}"),
    };
    let core = builder.build().await?;
    core.start().await?;
    tokio::time::timeout(
        Duration::from_secs(15),
        core.component_by_id(&query_id())?.wait_started(),
    )
    .await
    .context("query readiness timed out")??;
    let query = core
        .query_manager()
        .get_query_instance_by_id(&query_id())
        .await?;
    if restart {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = query.fetch_snapshot().await?;
                if snapshot.as_of_sequence >= 1 {
                    assert_eq!(snapshot.as_of_sequence, 1);
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
        .context("WAL event written after bootstrap was not replayed")??;
    } else {
        let snapshot = query.fetch_snapshot().await?;
        assert_eq!(snapshot.as_of_sequence, 0);
        assert!(snapshot.to_vec().is_empty());
    }
    drop(query);
    core.shutdown().await?;
    Ok(())
}

/// Simulates input accepted by the durable ingress while the query is down:
/// the first WAL entry after the empty bootstrap.
async fn append_wal_event(root: &Path) -> Result<()> {
    let wal = RedbWalProvider::new(root.join("wal"));
    let scope = StorageScope::default();
    wal.register(&scope, &source_id(), durability().to_wal_config())
        .await?;
    let mut properties = ElementPropertyMap::new();
    properties.insert("id", ElementValue::String("replayed".into()));
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id().as_str(), "replayed"),
                labels: Arc::from(vec![Arc::from("Node")]),
                effective_from: 0,
            },
            properties,
        },
    };
    assert_eq!(wal.append(&scope, &source_id(), &change).await?, 1);
    Ok(())
}

async fn check_recovery(kind: &str, policy: RecoveryPolicy) -> Result<()> {
    let root: PathBuf = tempfile::tempdir()?.keep();
    let calls = Arc::new(AtomicUsize::new(0));
    let result = async {
        run_query_phase(kind, policy, &root, &calls, false)
            .await
            .context("seed")?;
        append_wal_event(&root).await.context("append")?;
        run_query_phase(kind, policy, &root, &calls, true)
            .await
            .context("restart")?;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{kind} must not bootstrap twice"
        );
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[tokio::test(flavor = "current_thread")]
async fn http_strict() -> Result<()> {
    check_recovery("http", RecoveryPolicy::Strict).await
}

#[tokio::test(flavor = "current_thread")]
async fn http_auto_reset() -> Result<()> {
    check_recovery("http", RecoveryPolicy::AutoReset).await
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_strict() -> Result<()> {
    check_recovery("grpc", RecoveryPolicy::Strict).await
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_auto_reset() -> Result<()> {
    check_recovery("grpc", RecoveryPolicy::AutoReset).await
}

#[tokio::test(flavor = "current_thread")]
async fn otel_strict() -> Result<()> {
    check_recovery("otel", RecoveryPolicy::Strict).await
}

#[tokio::test(flavor = "current_thread")]
async fn otel_auto_reset() -> Result<()> {
    check_recovery("otel", RecoveryPolicy::AutoReset).await
}
