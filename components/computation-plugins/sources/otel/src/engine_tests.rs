// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Group admission, undo on refusal, pause gating and TTL sweeping against a
//! real ingress runtime.

use super::*;
use crate::config::OtelSourceConfig;
use crate::projection::tests::{
    client_span, gauge, metrics, point, resource, settings, traces, RECEIVED,
};
use crate::projection::{map_metrics, map_traces};
use crate::state::ElementCategory;
use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentDescriptor, ComponentId, GraphChangeCodec};
use drasi_computation_network_common::ingress::{IngressErrorKind, SourceRuntime};
use drasi_computation_plugin_sdk::lifecycle::{CancellationToken, LifecycleError, StopMode};
use drasi_core_types::models::SourceChange;
use serde_json::json;
use std::sync::Arc;

const ID: &str = "6a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

fn runtime(extra: serde_json::Value) -> Result<SourceRuntime> {
    let mut value = json!({"stream": "otel", "grpcPort": 0});
    value
        .as_object_mut()
        .context("object")?
        .extend(extra.as_object().context("object")?.clone());
    let config = OtelSourceConfig::from_value(value)?;
    SourceRuntime::new(
        ComponentDescriptor::try_new(ComponentId::try_new(ID)?, vec![])?,
        config.ingress()?,
        None,
        None,
    )
}

/// Starts a runtime whose listener only hands its ingress to the test.
async fn started(runtime: &mut SourceRuntime) -> Result<Arc<Ingress>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    runtime
        .start(
            &CancellationToken::new(),
            None,
            move |_listener, ingress| async move {
                let shutdown = ingress.shutdown.clone();
                let _ = sender.send(ingress);
                shutdown.cancelled().await;
                Ok(())
            },
        )
        .await
        .map_err(LifecycleError::into_source)?;
    Ok(receiver.await?)
}

/// Drains every queued envelope and returns its changes with their sequences.
async fn drain(runtime: &mut SourceRuntime) -> Result<Vec<(u64, SourceChange)>> {
    let mut changes = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_millis(50), runtime.next()).await {
            Ok(envelope) => {
                let envelope = envelope?.context("envelope")?.envelope;
                let sequence = envelope.system().sequence();
                for change in GraphChangeCodec::decode_changes(&envelope)? {
                    changes.push((sequence, change));
                }
            }
            Err(_) => return Ok(changes),
        }
    }
}

fn summary(changes: &[(u64, SourceChange)]) -> Vec<(u64, &'static str, String)> {
    changes
        .iter()
        .map(|(sequence, change)| {
            let kind = match change {
                SourceChange::Insert { .. } => "insert",
                SourceChange::Update { .. } => "update",
                SourceChange::Delete { .. } => "delete",
                _ => "other",
            };
            (
                *sequence,
                kind,
                change.get_reference().element_id.to_string(),
            )
        })
        .collect()
}

fn checkout_gauge(
    value: f64,
) -> crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest {
    metrics(
        resource(&[("service.name", "checkout")]),
        vec![gauge("latency", vec![point(value, 0, &[])])],
    )
}

fn engine(extra: serde_json::Value) -> Engine {
    let mut value = json!({"metricAllowlist": ["latency"]});
    value
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    Engine::new(ID, settings(value))
}

#[tokio::test(flavor = "current_thread")]
async fn an_export_is_admitted_as_ordered_inserts_then_updates() -> Result<()> {
    let mut runtime = runtime(json!({}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({}));
    let first = engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&checkout_gauge(920.0), settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(first, ExportOutcome { rejected: 0 });
    engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&checkout_gauge(700.0), settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(
        summary(&drain(&mut runtime).await?),
        vec![
            (1, "insert", "svc:checkout".into()),
            (2, "insert", "metric:svc:checkout:latency".into()),
            (3, "insert", "reports:metric:svc:checkout:latency".into()),
            (4, "update", "svc:checkout".into()),
            (5, "update", "metric:svc:checkout:latency".into()),
            (6, "update", "reports:metric:svc:checkout:latency".into()),
        ]
    );
    assert_eq!(
        engine.counters(),
        CounterSnapshot {
            accepted: 2,
            rejected: 0,
            dropped: 0,
            expired: 0
        }
    );
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_and_capped_records_are_reported_as_partial_success() -> Result<()> {
    let mut runtime = runtime(json!({}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({"maxServices": 1}));
    let request = crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest {
        resource_metrics: [
            checkout_gauge(1.0),
            metrics(
                resource(&[("service.name", "payments")]),
                vec![gauge("latency", vec![point(2.0, 0, &[])])],
            ),
            metrics(
                resource(&[("service.name", "checkout")]),
                vec![gauge("unlisted", vec![point(3.0, 0, &[])])],
            ),
        ]
        .into_iter()
        .flat_map(|request| request.resource_metrics)
        .collect(),
    };
    let outcome = engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&request, settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    // One unlisted metric rejected, one new service over its cap dropped.
    assert_eq!(outcome, ExportOutcome { rejected: 2 });
    assert_eq!(
        engine.counters(),
        CounterSnapshot {
            accepted: 1,
            rejected: 1,
            dropped: 1,
            expired: 0
        }
    );
    assert_eq!(drain(&mut runtime).await?.len(), 3);
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_group_is_undone_so_a_retry_inserts_again() -> Result<()> {
    // Room for exactly one three-change group: the second group of the same
    // request cannot be reserved before the timeout.
    let mut runtime = runtime(json!({"ingressCapacity": 8, "timeoutMs": 100}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({}));
    let request = crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest {
        resource_metrics: [
            checkout_gauge(1.0),
            metrics(
                resource(&[("service.name", "a")]),
                vec![gauge("latency", vec![point(2.0, 0, &[])])],
            ),
            metrics(
                resource(&[("service.name", "b")]),
                vec![gauge("latency", vec![point(3.0, 0, &[])])],
            ),
        ]
        .into_iter()
        .flat_map(|request| request.resource_metrics)
        .collect(),
    };
    let error = engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&request, settings, now)
        })
        .await
        .expect_err("the third group cannot fit");
    assert_eq!(error.kind, IngressErrorKind::Timeout);
    {
        let state = engine.state().await;
        assert!(state.contains("svc:checkout") && state.contains("svc:a"));
        assert!(!state.contains("svc:b"), "the refused group left no state");
        assert_eq!(state.count(ElementCategory::Service), 2);
    }
    assert_eq!(drain(&mut runtime).await?.len(), 6);
    // The exporter retries the whole request: admitted groups update, the
    // refused one is inserted. The consumer drains concurrently so all nine
    // changes can pass through the eight-slot queue.
    let retry = engine.ingest_at(&ingress, RECEIVED, |settings, now| {
        map_metrics(&request, settings, now)
    });
    let (retried, received) = tokio::join!(retry, async {
        let mut received = Vec::new();
        while received.len() < 9 {
            let envelope = runtime.next().await?.context("envelope")?.envelope;
            let sequence = envelope.system().sequence();
            for change in GraphChangeCodec::decode_changes(&envelope)? {
                received.push((sequence, change));
            }
        }
        Ok::<_, anyhow::Error>(received)
    });
    retried.map_err(anyhow::Error::new)?;
    let kinds: Vec<_> = summary(&received?)
        .into_iter()
        .map(|(_, kind, id)| (kind, id))
        .collect();
    assert_eq!(kinds[0], ("update", "svc:checkout".into()));
    assert_eq!(kinds[6], ("insert", "svc:b".into()));
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_paused_or_stopped_source_refuses_exports_before_projecting() -> Result<()> {
    let mut runtime = runtime(json!({}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({}));
    runtime.pause(&CancellationToken::new(), None).await?;
    let error = engine
        .ingest_at(&ingress, RECEIVED, |_, _| {
            panic!("a paused source must not project")
        })
        .await
        .expect_err("paused");
    assert_eq!(error.kind, IngressErrorKind::Paused);
    runtime.resume()?;
    engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&checkout_gauge(1.0), settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    runtime.stop(StopMode::Drain, None).await?;
    let error = engine
        .ingest_at(&ingress, RECEIVED, |_, _| {
            panic!("a stopped source must not project")
        })
        .await
        .expect_err("stopped");
    assert_eq!(error.kind, IngressErrorKind::Stopped);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sweeping_deletes_expired_dependencies_and_defers_while_paused() -> Result<()> {
    let mut runtime = runtime(json!({}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({"dependencyTtlSecs": 10}));
    let request = traces(
        resource(&[("service.name", "checkout")]),
        vec![client_span(&[("peer.service", "payments")], 0)],
    );
    engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_traces(&request, settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(drain(&mut runtime).await?.len(), 3);
    assert_eq!(
        engine
            .sweep_at(&ingress, RECEIVED + 9_999)
            .await
            .map_err(anyhow::Error::new)?,
        0
    );
    runtime.pause(&CancellationToken::new(), None).await?;
    assert_eq!(
        engine
            .sweep_at(&ingress, RECEIVED + 10_000)
            .await
            .map_err(anyhow::Error::new)?,
        0
    );
    assert!(engine
        .state()
        .await
        .contains("dep:svc:checkout:svc:payments"));
    runtime.resume()?;
    assert_eq!(
        engine
            .sweep_at(&ingress, RECEIVED + 10_000)
            .await
            .map_err(anyhow::Error::new)?,
        1
    );
    let deletes = drain(&mut runtime).await?;
    assert_eq!(
        summary(&deletes),
        vec![(4, "delete", "dep:svc:checkout:svc:payments".into())]
    );
    match &deletes[0].1 {
        SourceChange::Delete { metadata } => assert_eq!(metadata.effective_from, RECEIVED + 10_000),
        other => panic!("expected a delete, got {other:?}"),
    }
    // Services have no TTL and remain.
    assert!(engine.state().await.contains("svc:checkout"));
    assert_eq!(engine.counters().expired, 1);
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn large_expiries_are_admitted_in_queue_sized_chunks() -> Result<()> {
    let mut runtime = runtime(json!({"ingressCapacity": 8}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({"dependencyTtlSecs": 1}));
    // Ten dependencies, admitted in requests that each fit the queue.
    for callee in 0..10 {
        let request = traces(
            resource(&[("service.name", "checkout")]),
            vec![client_span(
                &[("peer.service", &format!("callee-{callee}"))],
                0,
            )],
        );
        engine
            .ingest_at(&ingress, RECEIVED, |settings, now| {
                map_traces(&request, settings, now)
            })
            .await
            .map_err(anyhow::Error::new)?;
        drain(&mut runtime).await?;
    }
    let sweep = {
        let engine = &engine;
        let ingress = ingress.clone();
        async move { engine.sweep_at(&ingress, RECEIVED + 1_000).await }
    };
    // The consumer drains concurrently so each chunk can be reserved.
    let (expired, deletes) = tokio::join!(sweep, async {
        let mut deletes = Vec::new();
        while deletes.len() < 10 {
            let envelope = runtime.next().await?.context("envelope")?.envelope;
            deletes.extend(GraphChangeCodec::decode_changes(&envelope)?);
        }
        Ok::<_, anyhow::Error>(deletes)
    });
    assert_eq!(expired.map_err(anyhow::Error::new)?, 10);
    let deletes = deletes?;
    assert!(deletes
        .iter()
        .all(|change| matches!(change, SourceChange::Delete { .. })));
    assert_eq!(engine.state().await.count(ElementCategory::DependsOn), 0);
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn updated_settings_apply_to_the_next_export() -> Result<()> {
    let mut runtime = runtime(json!({}))?;
    let ingress = started(&mut runtime).await?;
    let engine = engine(json!({}));
    let request = metrics(
        resource(&[("service.name", "checkout")]),
        vec![gauge("errors", vec![point(1.0, 0, &[])])],
    );
    let refused = engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&request, settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(refused.rejected, 1);
    engine.update_settings(settings(json!({"metricAllowlist": ["errors"]})));
    let admitted = engine
        .ingest_at(&ingress, RECEIVED, |settings, now| {
            map_metrics(&request, settings, now)
        })
        .await
        .map_err(anyhow::Error::new)?;
    assert_eq!(admitted.rejected, 0);
    assert_eq!(drain(&mut runtime).await?.len(), 3);
    runtime.stop(StopMode::Drain, None).await?;
    Ok(())
}
