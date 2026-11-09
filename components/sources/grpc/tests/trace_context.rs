// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_lib::{config::SourceSubscriptionSettings, observability::ExecutionContext, Source};
use drasi_source_grpc::{proto, GrpcSource, GrpcSourceConfig};
use std::time::Duration;

fn event() -> proto::SourceChange {
    proto::SourceChange {
        r#type: proto::ChangeType::Insert as i32,
        source_id: "trace-grpc".into(),
        timestamp: None,
        change: Some(proto::source_change::Change::Element(proto::Element {
            element: Some(proto::element::Element::Node(proto::Node {
                metadata: Some(proto::ElementMetadata {
                    reference: Some(proto::ElementReference {
                        source_id: "trace-grpc".into(),
                        element_id: "row".into(),
                    }),
                    labels: vec!["Room".into()],
                    effective_from: 123,
                }),
                properties: None,
            })),
        })),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn incoming_grpc_trace_context_survives_unary_and_stream_dispatch() -> Result<()> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let source = GrpcSource::new(
        "trace-grpc",
        GrpcSourceConfig {
            host: "127.0.0.1".into(),
            port,
            ..Default::default()
        },
    )?;
    source.start().await?;
    let mut subscription = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "trace-grpc".into(),
            query_id: "trace-query".into(),
            nodes: Default::default(),
            relations: Default::default(),
            enable_bootstrap: false,
            resume_from: None,
            resume_sequence: None,
            request_position_handle: false,
        })
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut client = loop {
        match proto::source_service_client::SourceServiceClient::connect(format!(
            "http://127.0.0.1:{port}"
        ))
        .await
        {
            Ok(client) => break client,
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error).context("gRPC listener readiness");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    for parent in [
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00"),
        Some("malformed"),
        None,
    ] {
        let mut request = tonic::Request::new(proto::SubmitEventRequest {
            event: Some(event()),
        });
        if let Some(parent) = parent {
            request
                .metadata_mut()
                .insert("traceparent", parent.parse()?);
            request
                .metadata_mut()
                .insert("tracestate", "vendor=value".parse()?);
        }
        anyhow::ensure!(
            client.submit_event(request).await?.into_inner().success,
            "event rejected"
        );
        let message =
            tokio::time::timeout(Duration::from_secs(5), subscription.receiver.recv_message())
                .await
                .context("gRPC source dispatch timeout")??;
        assert_eq!(
            message.execution_context,
            ExecutionContext::from_w3c_headers(parent.map(str::as_bytes), Some(b"vendor=value")),
        );
    }
    let parent = "00-11111111111111111111111111111111-2222222222222222-00";
    let mut request = tonic::Request::new(tokio_stream::iter([event(), event()]));
    request
        .metadata_mut()
        .insert("traceparent", parent.parse()?);
    let mut responses = client.stream_events(request).await?.into_inner();
    let response = responses.message().await?.context("stream completion")?;
    assert!(response.success);
    assert_eq!(response.events_processed, 2);
    for _ in 0..2 {
        let message =
            tokio::time::timeout(Duration::from_secs(5), subscription.receiver.recv_message())
                .await
                .context("gRPC stream dispatch timeout")??;
        assert_eq!(
            message.execution_context,
            ExecutionContext::from_w3c_headers(Some(parent.as_bytes()), None),
        );
    }
    source.stop().await?;
    Ok(())
}
