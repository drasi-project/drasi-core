// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_lib::{config::SourceSubscriptionSettings, observability::ExecutionContext, Source};
use drasi_source_http::HttpSourceBuilder;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "current_thread")]
async fn incoming_http_trace_context_survives_adaptive_dispatch() -> Result<()> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let source = HttpSourceBuilder::new("trace-http")
        .with_host("127.0.0.1")
        .with_port(port)
        .build()?;
    source.start().await?;
    let mut subscription = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "trace-http".into(),
            query_id: "trace-query".into(),
            nodes: Default::default(),
            relations: Default::default(),
            enable_bootstrap: false,
            resume_from: None,
            resume_sequence: None,
            request_position_handle: false,
        })
        .await?;
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
        {
            Ok(response) => {
                response.error_for_status()?;
                break;
            }
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error).context("HTTP listener readiness");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    for (index, parent) in [
        Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00"),
        Some("malformed"),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let mut request = client
            .post(format!("http://127.0.0.1:{port}/sources/trace-http/events"))
            .json(&json!({"operation":"insert","element":{
                "type":"node","id":format!("row-{index}"),"labels":["Room"],"properties":{"value":index}
            }}));
        if let Some(parent) = parent {
            request = request
                .header("traceparent", parent)
                .header("tracestate", "vendor=value");
        }
        let result = request
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        anyhow::ensure!(result["success"] == true, "event rejected: {result}");
        let message =
            tokio::time::timeout(Duration::from_secs(5), subscription.receiver.recv_message())
                .await
                .context("HTTP source dispatch timeout")??;
        assert_eq!(
            message.execution_context,
            ExecutionContext::from_w3c_headers(parent.map(str::as_bytes), Some(b"vendor=value")),
        );
    }
    source.stop().await?;
    Ok(())
}
