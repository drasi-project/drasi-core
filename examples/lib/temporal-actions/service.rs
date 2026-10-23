// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

//! Read-only status endpoint for the containerized example, not a public Drasi API.

use anyhow::{ensure, Result};
use drasi_lib::{DrasiLib, StateStoreProvider};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

pub async fn healthcheck() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect("127.0.0.1:8080").await?;
        stream.write_all(b"GET /status HTTP/1.0\r\n\r\n").await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        ensure!(
            response.starts_with(b"HTTP/1.0 200 "),
            "Drasi is not healthy"
        );
        Ok(())
    })
    .await?
}

pub async fn run(core: &DrasiLib, store: &impl StateStoreProvider) -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    let stop = tokio::signal::ctrl_c();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            biased;
            stopped = &mut stop => return Ok(stopped?),
            connection = listener.accept() => {
                let (mut stream, _) = connection?;
                let response = tokio::time::timeout(Duration::from_secs(3), async {
                    let mut request = String::new();
                    BufReader::new((&mut stream).take(1024)).read_line(&mut request).await?;
                    let (code, body) = if request.starts_with("GET /status HTTP/") {
                        let status = crate::status(core, store).await?;
                        let healthy = ["status", "sourceStatus", "queryStatus"]
                            .iter().all(|key| status[key] == "Running");
                        (if healthy { "200 OK" } else { "503 Service Unavailable" },
                         serde_json::to_vec(&status)?)
                    } else {
                        ("404 Not Found", b"{\"error\":\"use GET /status\"}".to_vec())
                    };
                    let headers = format!("HTTP/1.0 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    stream.write_all(headers.as_bytes()).await?;
                    stream.write_all(&body).await?;
                    stream.shutdown().await?;
                    anyhow::Ok(())
                }).await;
                match response {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => eprintln!("Status request failed: {error:#}"),
                    Err(error) => eprintln!("Status request timed out: {error}"),
                }
            }
        }
    }
}
