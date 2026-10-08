// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_network::SSE_SINK;

async fn roundtrip() -> Result<()> {
    let port = free_port()?;
    let config = json!({
        "queryStreams": {"q": "q/out"},
        "host": "127.0.0.1", "port": port,
        "ssePath": "/events", "heartbeatIntervalMs": 25,
    });
    let mut sink = component(SSE_SINK, "ui", config)?;
    assert_eq!(sink.completion(), SinkCompletion::Accepted);
    assert!(sink.handle(result_input(1)?).await.is_err());
    sink.start().await?;
    let original = sink.configuration()?;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/missing"))
            .send()
            .await?
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/events"),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let mut response = client
        .get(format!("http://127.0.0.1:{port}/events"))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    sink.handle(result_input(42)?).await?;
    let mut frames = Vec::new();
    tokio::time::timeout(DEADLINE, async {
        let mut pending = String::new();
        loop {
            let bytes = response.chunk().await?.context("SSE stream ended")?;
            pending.push_str(std::str::from_utf8(&bytes)?);
            while let Some(end) = pending.find("\n\n") {
                let frame: String = pending.drain(..end + 2).collect();
                for data in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
                    frames.push(serde_json::from_str::<Value>(data.trim())?);
                }
            }
            if frames.iter().any(|frame| frame["queryId"] == "q")
                && frames.iter().any(|frame| frame["type"] == "heartbeat")
            {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    let expected = QueryChangeCodec::to_legacy_result(&result_input(42)?.envelope)?;
    assert_eq!(
        frames
            .iter()
            .find(|frame| frame["queryId"] == "q")
            .context("query event")?["results"],
        serde_json::to_value(expected.results)?
    );
    assert_eq!(sink.configuration()?, original);
    sink.stop().await?;
    tokio::time::timeout(DEADLINE, async {
        while response.chunk().await?.is_some() {}
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(sink.handle(result_input(43)?).await.is_err());
    sink.start().await?;
    sink.handle(result_input(44)?).await?;
    sink.stop().await?;
    let _released = TcpListener::bind(("127.0.0.1", port)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sse_current_thread() -> Result<()> {
    roundtrip().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_sse_multi_thread() -> Result<()> {
    roundtrip().await
}
