// Copyright 2025 The Drasi Authors.
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

//! PoC: Reproduce HTTP/2 silent body truncation (Issue #493).
//!
//! These tests use a custom h2 server (via the `h2` crate) that sends
//! END_STREAM after delivering only a fraction of the response body. This
//! reproduces the exact silent truncation scenario that causes issue #493:
//! the client gets `Ok(partial_body)` with no transport error, and the
//! response is unparseable JSON.
//!
//! The tests prove:
//! 1. Without Content-Length, h2 silently truncates (bug reproduced).
//! 2. With Content-Length, h2 detects the mismatch and returns an error.
//! 3. HTTP/1.1 (`.http1_only()`) cannot connect to h2-only servers,
//!    preventing the h2-specific truncation entirely.
//!
//! Run with: cargo test -p drasi-bootstrap-http --test h2_truncation_poc -- --ignored --nocapture

#![allow(clippy::unwrap_used, clippy::uninlined_format_args)]

use futures::future::join_all;
use h2::server as h2_server;
use reqwest::Client;
use serde_json::{json, Value as JsonValue};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Generate a large JSON string representing an array of N items.
fn generate_large_json_string(num_items: usize, item_size_bytes: usize) -> String {
    let padding = "x".repeat(item_size_bytes);
    let items: Vec<JsonValue> = (0..num_items)
        .map(|i| {
            json!({
                "id": i,
                "name": format!("item-{i}"),
                "data": padding,
                "checksum": format!("end-marker-{i}")
            })
        })
        .collect();
    serde_json::to_string(&json!(items)).unwrap()
}

/// Start an h2 server that sends only a fraction of the body then END_STREAM.
///
/// - `truncate_fraction`: how much of the body to send (0.0–1.0).
/// - `include_content_length`: if true, sets Content-Length to full body size
///   (causing a detectable mismatch).
async fn start_h2_truncating_server(
    truncate_fraction: f64,
    include_content_length: bool,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => return,
            };

            let truncate_frac = truncate_fraction;
            let include_cl = include_content_length;

            tokio::spawn(async move {
                let mut conn = match h2_server::handshake(socket).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[h2 server] handshake error: {e}");
                        return;
                    }
                };

                while let Some(request) = conn.accept().await {
                    let (request, mut respond) = match request {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("[h2 server] accept error: {e}");
                            return;
                        }
                    };

                    let truncate_frac = truncate_frac;
                    let include_cl = include_cl;

                    tokio::spawn(async move {
                        let path = request.uri().path().to_string();

                        let num_items = match path.as_str() {
                            "/api/data1" => 100,
                            "/api/data2" => 150,
                            "/api/data3" => 200,
                            "/api/data4" => 120,
                            "/api/data5" => 180,
                            _ => 50,
                        };
                        let full_body = generate_large_json_string(num_items, 1500);
                        let full_len = full_body.len();

                        let send_bytes = (full_len as f64 * truncate_frac) as usize;
                        let truncated_body = &full_body.as_bytes()[..send_bytes];

                        let mut response = http::Response::builder()
                            .status(200)
                            .header("content-type", "application/json");

                        if include_cl {
                            response =
                                response.header("content-length", full_len.to_string());
                        }

                        let response = response.body(()).unwrap();
                        let mut send_stream = respond.send_response(response, false).unwrap();

                        // Send truncated body in h2 frame-sized chunks
                        let chunk_size = 16384;
                        let mut offset = 0;
                        while offset < truncated_body.len() {
                            let end = std::cmp::min(offset + chunk_size, truncated_body.len());
                            let is_last = end >= truncated_body.len();

                            send_stream.reserve_capacity(end - offset);
                            loop {
                                let cap = send_stream.capacity();
                                if cap > 0 {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }

                            let chunk = bytes::Bytes::copy_from_slice(&truncated_body[offset..end]);
                            if let Err(e) = send_stream.send_data(chunk, is_last) {
                                eprintln!("[h2 server] send_data error on {path}: {e}");
                                return;
                            }
                            offset = end;
                        }
                    });
                }
            });
        }
    });

    (port, handle)
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Proves: Without Content-Length, h2 END_STREAM after 50% of body causes
/// the client to get `Ok(partial_body)` — silent truncation, no transport error.
#[tokio::test]
#[ignore]
async fn test_h2_premature_end_stream_no_content_length() {
    let _ = env_logger::try_init();

    let (port, _server) = start_h2_truncating_server(0.5, false).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== h2 Premature END_STREAM (no Content-Length) ===");
    println!("Server on port {port}, sending 50% of body + END_STREAM");
    println!("Expected: client gets Ok(partial_body) → JSON parse fails");
    println!();

    let client = Client::builder().http2_prior_knowledge().build().unwrap();

    let endpoints: Vec<String> = (1..=5)
        .map(|i| format!("http://127.0.0.1:{port}/api/data{i}"))
        .collect();

    let mut silent_truncations = 0u32;
    let mut proper_errors = 0u32;
    let mut successes = 0u32;
    let total_rounds = 10;

    for round in 0..total_rounds {
        let mut futures = Vec::new();
        for url in &endpoints {
            let client = client.clone();
            let url = url.clone();
            futures.push(tokio::spawn(async move {
                let result = client.get(&url).send().await;
                match result {
                    Ok(resp) => {
                        let status = resp.status();
                        match resp.text().await {
                            Ok(body) => match serde_json::from_str::<JsonValue>(&body) {
                                Ok(_) => ("success".to_string(), body.len(), String::new()),
                                Err(e) => (
                                    "truncated".to_string(),
                                    body.len(),
                                    format!("status={status}, JSON err: {e}"),
                                ),
                            },
                            Err(e) => ("body_error".to_string(), 0, format!("{e}")),
                        }
                    }
                    Err(e) => ("request_error".to_string(), 0, format!("{e}")),
                }
            }));
        }

        let results = join_all(futures).await;
        for res in results {
            match res {
                Ok((kind, len, detail)) => match kind.as_str() {
                    "success" => successes += 1,
                    "truncated" => {
                        silent_truncations += 1;
                        eprintln!("  [Round {round}] ✗ SILENT TRUNCATION: body_len={len}, {detail}");
                    }
                    _ => {
                        proper_errors += 1;
                        eprintln!("  [Round {round}] error: {detail}");
                    }
                },
                Err(e) => {
                    proper_errors += 1;
                    eprintln!("  [Round {round}] task error: {e}");
                }
            }
        }
    }

    let total = total_rounds as u32 * endpoints.len() as u32;
    println!("\n=== Results: Premature END_STREAM (no Content-Length) ===");
    println!("Total requests: {total}");
    println!("Silent truncations (BUG reproduced): {silent_truncations}");
    println!("Proper errors (transport detected): {proper_errors}");
    println!("Full successes (should be 0): {successes}");

    if silent_truncations > 0 {
        println!("\n🔴 BUG REPRODUCED: {silent_truncations} silent truncations!");
        println!("   The client received Ok(partial_body) — no transport error.");
        println!("   This proves HTTP/2 END_STREAM can silently truncate responses.");
    } else if proper_errors > 0 {
        println!("\n🟡 Truncation was DETECTED by transport layer.");
    } else {
        println!("\n🟢 All requests succeeded (unexpected for 50% truncation).");
    }
}

/// Proves: With Content-Length set to full body size but only 50% of data sent,
/// the h2 implementation detects the mismatch and returns a proper error.
#[tokio::test]
#[ignore]
async fn test_h2_premature_end_stream_with_content_length() {
    let _ = env_logger::try_init();

    let (port, _server) = start_h2_truncating_server(0.5, true).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== h2 Premature END_STREAM (WITH Content-Length) ===");
    println!("Server on port {port}, sending 50% of body + END_STREAM");
    println!("Content-Length set to full size → h2 impl SHOULD detect mismatch");
    println!();

    let client = Client::builder().http2_prior_knowledge().build().unwrap();

    let endpoints: Vec<String> = (1..=3)
        .map(|i| format!("http://127.0.0.1:{port}/api/data{i}"))
        .collect();

    let mut silent_truncations = 0u32;
    let mut proper_errors = 0u32;
    let mut successes = 0u32;

    for _round in 0..5 {
        let mut futures = Vec::new();
        for url in &endpoints {
            let client = client.clone();
            let url = url.clone();
            futures.push(tokio::spawn(async move {
                let result = client.get(&url).send().await;
                match result {
                    Ok(resp) => match resp.text().await {
                        Ok(body) => match serde_json::from_str::<JsonValue>(&body) {
                            Ok(_) => "success".to_string(),
                            Err(_) => format!("truncated:{}", body.len()),
                        },
                        Err(e) => format!("body_error:{e}"),
                    },
                    Err(e) => format!("request_error:{e}"),
                }
            }));
        }

        let results = join_all(futures).await;
        for res in results {
            match res {
                Ok(result) => {
                    if result == "success" {
                        successes += 1;
                    } else if result.starts_with("truncated:") {
                        silent_truncations += 1;
                        eprintln!("  ✗ SILENT TRUNCATION: {result}");
                    } else {
                        proper_errors += 1;
                        eprintln!("  Error (correct): {result}");
                    }
                }
                Err(e) => {
                    proper_errors += 1;
                    eprintln!("  Task error: {e}");
                }
            }
        }
    }

    let total = 5 * endpoints.len() as u32;
    println!("\n=== Results: Premature END_STREAM (with Content-Length) ===");
    println!("Total: {total}, Truncated: {silent_truncations}, Errors: {proper_errors}, Success: {successes}");

    if silent_truncations > 0 {
        println!("\n🔴 h2 does NOT validate Content-Length vs actual body size!");
    } else if proper_errors > 0 {
        println!("\n🟢 h2 correctly detects Content-Length mismatch → errors.");
        println!("   Content-Length validation at the application layer is redundant.");
    }
}

/// Proves: HTTP/1.1 client (`.http1_only()`) cannot connect to an h2-only server,
/// preventing h2-specific truncation entirely.
#[tokio::test]
#[ignore]
async fn test_h1_prevents_premature_end_stream() {
    let _ = env_logger::try_init();

    let (port, _server) = start_h2_truncating_server(0.5, false).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== HTTP/1.1 vs h2 truncating server ===");
    println!("h2 server on port {port} — HTTP/1.1 client should FAIL to connect");
    println!();

    let client = Client::builder().http1_only().build().unwrap();
    let url = format!("http://127.0.0.1:{port}/api/data1");
    let result = client.get(&url).send().await;

    match result {
        Err(e) => {
            println!("✓ CONFIRMED: HTTP/1.1 client cannot connect to h2-only server");
            println!("  Error: {e}");
            println!("  This proves .http1_only() avoids the h2 truncation path entirely.");
        }
        Ok(resp) => {
            println!("✗ UNEXPECTED: HTTP/1.1 client connected to h2-only server");
            println!("  Status: {}", resp.status());
            let body = resp.text().await.unwrap_or_default();
            println!("  Body len: {}", body.len());
        }
    }
}
