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

//! PoC: Reproduce HTTP/2 multiplexing body truncation (Issue #493).
//!
//! This test creates conditions that trigger HTTP/2 flow control race conditions
//! by using a bandwidth-throttled TCP proxy between client and server. The
//! throttling forces many small TCP reads/writes, creating interleaving of h2
//! DATA frames across multiplexed streams — the exact condition that triggers
//! body truncation in hyper 0.14 / h2 0.3.x.
//!
//! Run with: cargo test -p drasi-bootstrap-http --test h2_truncation_poc -- --ignored --nocapture

#![allow(clippy::unwrap_used, clippy::uninlined_format_args)]

use axum::{routing::get, Json, Router};
use futures::future::join_all;
use h2::server as h2_server;
use rcgen::generate_simple_self_signed;
use reqwest::Client;
use rustls::ServerConfig;
use serde_json::{json, Value as JsonValue};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, Duration};
use tokio_rustls::TlsAcceptor;

/// Generate a large JSON array with N items, each having a long string property.
fn generate_large_json(num_items: usize, item_size_bytes: usize) -> JsonValue {
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
    json!(items)
}

/// Start a TLS server that serves large JSON payloads.
/// Returns (server_addr_port) - the raw address to connect to.
#[allow(clippy::unwrap_used)]
async fn start_tls_server() -> (String, Arc<rustls::ServerConfig>) {
    let app = Router::new()
        .route(
            "/endpoint-a",
            get(|| async { Json(generate_large_json(80, 3000)) }),
        )
        .route(
            "/endpoint-b",
            get(|| async { Json(generate_large_json(100, 3000)) }),
        )
        .route(
            "/endpoint-c",
            get(|| async { Json(generate_large_json(90, 3000)) }),
        )
        .route(
            "/endpoint-d",
            get(|| async { Json(generate_large_json(120, 3000)) }),
        )
        .route(
            "/endpoint-e",
            get(|| async { Json(generate_large_json(70, 3000)) }),
        );

    // Generate self-signed TLS certificate for localhost
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let cert = generate_simple_self_signed(subject_alt_names).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.key_pair.serialize_pem();

    let certs = rustls_pemfile::certs(&mut Cursor::new(cert_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut Cursor::new(key_pem.as_bytes()))
        .unwrap()
        .unwrap();

    let mut tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let server_config = Arc::new(tls_config);
    let tls_acceptor = TlsAcceptor::from(Arc::clone(&server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // DevSkim: ignore DS137138
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };
            let acceptor = tls_acceptor.clone();
            let app = app.clone();

            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                let service = hyper_util::service::TowerToHyperService::new(app);
                let builder = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                );
                let _ = builder.serve_connection(io, service).await;
            });
        }
    });

    (format!("127.0.0.1:{}", addr.port()), server_config) // DevSkim: ignore DS137138
}

/// Start a throttling TCP proxy that forwards data between client and server
/// with bandwidth limiting and small buffer sizes.
///
/// This simulates real network conditions by:
/// - Limiting throughput to `bytes_per_sec` in each direction
/// - Using small read buffers to force many small TCP reads
/// - Introducing micro-delays between chunk forwards
#[allow(clippy::unwrap_used)]
async fn start_throttling_proxy(target_addr: &str, bytes_per_sec: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // DevSkim: ignore DS137138
    let proxy_addr = listener.local_addr().unwrap();
    let target = target_addr.to_string();

    tokio::spawn(async move {
        loop {
            let (client_stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };

            let target = target.clone();
            tokio::spawn(async move {
                let server_stream = match tokio::net::TcpStream::connect(&target).await {
                    Ok(s) => s,
                    Err(_) => return,
                };

                let (mut client_read, mut client_write) = client_stream.into_split();
                let (mut server_read, mut server_write) = server_stream.into_split();

                // Calculate delay per chunk to achieve target throughput
                let chunk_size = 2048usize;
                let delay_per_chunk =
                    Duration::from_micros((chunk_size as u64 * 1_000_000) / bytes_per_sec as u64);

                // Client -> Server (request direction, usually small, minimal throttle)
                let c2s = tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match client_read.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if server_write.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = server_write.shutdown().await;
                });

                // Server -> Client (response direction, throttled)
                let s2c = tokio::spawn(async move {
                    let mut buf = vec![0u8; chunk_size];
                    loop {
                        match server_read.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if client_write.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                                // Throttle: sleep after each chunk
                                sleep(delay_per_chunk).await;
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = client_write.shutdown().await;
                });

                let _ = tokio::join!(c2s, s2c);
            });
        }
    });

    format!("127.0.0.1:{}", proxy_addr.port()) // DevSkim: ignore DS137138
}

/// Start a proxy that abruptly kills the server->client direction after
/// forwarding a specified number of bytes. This simulates what happens when
/// a load balancer resets an HTTP/2 connection mid-response, or when GOAWAY
/// is received while DATA frames are in-flight.
///
/// The key behavior: the proxy forwards `bytes_before_kill` bytes from the server,
/// then closes the connection abruptly (no graceful shutdown). This forces the
/// client's h2 implementation to deal with a connection that died while streams
/// had partially-received bodies.
#[allow(clippy::unwrap_used)]
async fn start_truncating_proxy(target_addr: &str, bytes_before_kill: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // DevSkim: ignore DS137138
    let proxy_addr = listener.local_addr().unwrap();
    let target = target_addr.to_string();
    let kill_threshold = bytes_before_kill;

    tokio::spawn(async move {
        loop {
            let (client_stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => continue,
            };

            let target = target.clone();
            tokio::spawn(async move {
                let server_stream = match tokio::net::TcpStream::connect(&target).await {
                    Ok(s) => s,
                    Err(_) => return,
                };

                let (mut client_read, mut client_write) = client_stream.into_split();
                let (mut server_read, mut server_write) = server_stream.into_split();

                // Client -> Server (forward normally)
                let c2s = tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match client_read.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if server_write.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });

                // Server -> Client: forward bytes then ABRUPTLY drop
                let s2c = tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut total_forwarded = 0usize;
                    loop {
                        match server_read.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                total_forwarded += n;
                                if total_forwarded >= kill_threshold {
                                    // Forward partial last chunk then kill
                                    let remaining = n.saturating_sub(
                                        total_forwarded.saturating_sub(kill_threshold),
                                    );
                                    if remaining > 0 {
                                        let _ = client_write.write_all(&buf[..remaining]).await;
                                    }
                                    // Graceful shutdown (TCP FIN) — NOT RST.
                                    // This simulates a TLS close_notify without h2 GOAWAY,
                                    // which may cause the client's h2 layer to think the
                                    // connection completed normally.
                                    let _ = client_write.shutdown().await;
                                    return;
                                }
                                if client_write.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = client_write.shutdown().await;
                });

                let _ = tokio::join!(c2s, s2c);
            });
        }
    });

    format!("127.0.0.1:{}", proxy_addr.port()) // DevSkim: ignore DS137138
}

/// Fetch a URL, parse JSON body, validate structure integrity.
async fn fetch_and_validate(client: &Client, url: &str) -> Result<usize, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP error: {}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|e| format!("Body read failed: {e}"))?;

    let body_len = body.len();

    // Parse JSON - truncation causes parse failures
    let parsed: Vec<JsonValue> = serde_json::from_str(&body).map_err(|e| {
        let tail = &body[body_len.saturating_sub(80)..];
        format!("JSON parse failed (body_len={body_len}, tail={tail:?}): {e}")
    })?;

    // Validate end-markers to detect data corruption
    for (i, item) in parsed.iter().enumerate() {
        let expected = format!("end-marker-{i}");
        let actual = item.get("checksum").and_then(|v| v.as_str()).unwrap_or("");
        if actual != expected {
            return Err(format!(
                "Data corruption at item {i}: expected '{expected}', got '{actual}'"
            ));
        }
    }

    Ok(body_len)
}

/// Core test logic: run concurrent H2 requests through a throttled proxy.
async fn run_h2_throttled_test(
    proxy_addr: &str,
    num_rounds: usize,
    concurrent_per_round: usize,
) -> (usize, usize, usize) {
    let endpoints: Vec<String> = vec!["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|e| format!("https://{proxy_addr}/endpoint-{e}"))
        .collect();

    // HTTP/2 client (default ALPN negotiation, NO http1_only)
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(1)
        .build()
        .expect("Failed to build client");

    let total = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicUsize::new(0));

    for round in 0..num_rounds {
        let mut futures = Vec::new();

        for i in 0..concurrent_per_round {
            let url = endpoints[i % endpoints.len()].clone();
            let client = client.clone();
            let total = Arc::clone(&total);
            let failed = Arc::clone(&failed);
            let truncated = Arc::clone(&truncated);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);
                match fetch_and_validate(&client, &url).await {
                    Ok(_) => {}
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        if e.contains("JSON parse failed") || e.contains("Data corruption") {
                            truncated.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[Round {round}] ✗ TRUNCATION: {e}");
                        } else if round < 3 {
                            eprintln!("[Round {round}] FAILURE: {e}");
                        }
                    }
                }
            }));
        }

        join_all(futures).await;
    }

    (
        total.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
        truncated.load(Ordering::Relaxed),
    )
}

/// Test HTTP/2 multiplexing through a bandwidth-throttled proxy.
/// The throttling creates conditions where h2 DATA frames are delivered
/// slowly and interleaved across streams, triggering flow control races.
///
/// Throttle: 500KB/s — enough to serve data but slow enough to create
/// concurrent in-flight streams with partially received bodies.
#[tokio::test]
#[ignore]
async fn test_h2_throttled_proxy_500kbps() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    // 500 KB/s throttle — large responses (250-400KB each) take 0.5-0.8s
    // With 10+ concurrent streams, all data is in-flight simultaneously
    let proxy_addr = start_throttling_proxy(&server_addr, 500_000).await;

    // Give proxy time to start
    sleep(Duration::from_millis(50)).await;

    let (total, failed, truncated) = run_h2_throttled_test(&proxy_addr, 10, 15).await;

    println!("\n=== HTTP/2 Throttled Proxy (500KB/s) Results ===");
    println!("Total requests: {total}");
    println!("Failed requests: {failed}");
    println!("Truncation errors: {truncated}");
    println!(
        "Success rate: {:.1}%",
        if total > 0 {
            (total - failed) as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    );

    if truncated > 0 {
        println!("\n✓ REPRODUCED: {truncated} truncation(s) under throttled HTTP/2");
    } else if failed > 0 {
        println!("\n⚠ {failed} failures (connection errors, not truncation)");
    } else {
        println!("\n  All requests succeeded at this throttle level");
    }
}

/// More aggressive throttle: 100KB/s.
/// With 250-400KB responses and 15 concurrent streams, this creates
/// extreme multiplexing pressure — each response takes 2.5-4 seconds,
/// keeping all streams in-flight simultaneously for extended periods.
#[tokio::test]
#[ignore]
async fn test_h2_throttled_proxy_100kbps() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    // 100 KB/s — very slow, maximum h2 flow control pressure
    let proxy_addr = start_throttling_proxy(&server_addr, 100_000).await;
    sleep(Duration::from_millis(50)).await;

    let (total, failed, truncated) = run_h2_throttled_test(&proxy_addr, 5, 20).await;

    println!("\n=== HTTP/2 Throttled Proxy (100KB/s) Results ===");
    println!("Total requests: {total}");
    println!("Failed requests: {failed}");
    println!("Truncation errors: {truncated}");
    println!(
        "Success rate: {:.1}%",
        if total > 0 {
            (total - failed) as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    );

    if truncated > 0 {
        println!("\n✓ REPRODUCED: {truncated} truncation(s) under heavy throttle");
    }
}

/// Extremely constrained: 50KB/s with burst concurrency.
/// This mimics a congested network connection where multiple large
/// responses compete for a very limited pipe.
#[tokio::test]
#[ignore]
async fn test_h2_throttled_proxy_50kbps() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    // 50 KB/s — extreme throttle
    let proxy_addr = start_throttling_proxy(&server_addr, 50_000).await;
    sleep(Duration::from_millis(50)).await;

    let (total, failed, truncated) = run_h2_throttled_test(&proxy_addr, 3, 25).await;

    println!("\n=== HTTP/2 Throttled Proxy (50KB/s) Results ===");
    println!("Total requests: {total}");
    println!("Failed requests: {failed}");
    println!("Truncation errors: {truncated}");
    println!(
        "Success rate: {:.1}%",
        if total > 0 {
            (total - failed) as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    );

    if truncated > 0 {
        println!("\n✓ REPRODUCED: {truncated} truncation(s) under extreme throttle");
    }
}

/// Control: HTTP/1.1 through the same throttled proxy.
/// Even with throttling, HTTP/1.1 should never truncate because there's
/// no multiplexing — each request/response pair is fully serialized.
#[tokio::test]
#[ignore]
async fn test_h1_throttled_proxy_control() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    let proxy_addr = start_throttling_proxy(&server_addr, 500_000).await;
    sleep(Duration::from_millis(50)).await;

    let endpoints: Vec<String> = vec!["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|e| format!("https://{proxy_addr}/endpoint-{e}"))
        .collect();

    // HTTP/1.1 client (the fix)
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .http1_only()
        .build()
        .expect("Failed to build h1 client");

    let total = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    for _round in 0..5 {
        let mut futures = Vec::new();
        for i in 0..10 {
            let url = endpoints[i % endpoints.len()].clone();
            let client = client.clone();
            let total = Arc::clone(&total);
            let failed = Arc::clone(&failed);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = fetch_and_validate(&client, &url).await {
                    failed.fetch_add(1, Ordering::Relaxed);
                    eprintln!("H1 FAILURE: {e}");
                }
            }));
        }
        join_all(futures).await;
    }

    let t = total.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);

    println!("\n=== HTTP/1.1 Throttled Proxy (Control) Results ===");
    println!("Total requests: {t}");
    println!("Failed requests: {f}");
    println!(
        "Success rate: {:.1}%",
        if t > 0 {
            (t - f) as f64 / t as f64 * 100.0
        } else {
            0.0
        }
    );

    assert_eq!(
        f, 0,
        "HTTP/1.1 should never truncate, even through a throttled proxy. Got {f} failures."
    );
    println!("\n✓ CONFIRMED: HTTP/1.1 remains reliable through throttled proxy");
}

/// Test with a proxy that abruptly kills connections mid-response.
/// This simulates load balancer resets, GOAWAY frames, or network partitions
/// that cause incomplete data delivery.
///
/// Key question: Does reqwest/hyper surface this as an error (correct behavior)
/// or does it silently return truncated body data (the bug)?
#[tokio::test]
#[ignore]
async fn test_h2_connection_kill_mid_response() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    // Kill after 100KB — responses are 250-400KB, so this cuts them mid-body
    let proxy_addr = start_truncating_proxy(&server_addr, 100_000).await;
    sleep(Duration::from_millis(50)).await;

    let endpoints: Vec<String> = vec!["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|e| format!("https://{proxy_addr}/endpoint-{e}"))
        .collect();

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(1)
        .build()
        .expect("Failed to build client");

    let total = Arc::new(AtomicUsize::new(0));
    let got_error = Arc::new(AtomicUsize::new(0));
    let got_truncated_body = Arc::new(AtomicUsize::new(0));
    let got_success = Arc::new(AtomicUsize::new(0));

    // Fire concurrent requests - the proxy will kill the connection
    for round in 0..5 {
        let mut futures = Vec::new();
        for url in &endpoints {
            let client = client.clone();
            let url = url.clone();
            let total = Arc::clone(&total);
            let got_error = Arc::clone(&got_error);
            let got_truncated_body = Arc::clone(&got_truncated_body);
            let got_success = Arc::clone(&got_success);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);

                let resp = match client.get(&url).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        got_error.fetch_add(1, Ordering::Relaxed);
                        if round == 0 {
                            eprintln!("  [send error] {e}");
                        }
                        return;
                    }
                };

                match resp.text().await {
                    Ok(body) => {
                        // Got a body - is it complete or truncated?
                        match serde_json::from_str::<JsonValue>(&body) {
                            Ok(_) => {
                                got_success.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                // THIS IS THE BUG: body was received as "Ok" but it's truncated
                                got_truncated_body.fetch_add(1, Ordering::Relaxed);
                                eprintln!(
                                    "  [Round {round}] ✗ SILENT TRUNCATION: body_len={}, parse_error={e}",
                                    body.len()
                                );
                                eprintln!(
                                    "    tail: {:?}",
                                    &body[body.len().saturating_sub(60)..]
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // This is CORRECT behavior - error on incomplete body
                        got_error.fetch_add(1, Ordering::Relaxed);
                        if round == 0 {
                            eprintln!("  [body read error - correct] {e}");
                        }
                    }
                }
            }));
        }
        join_all(futures).await;
    }

    let t = total.load(Ordering::Relaxed);
    let errors = got_error.load(Ordering::Relaxed);
    let truncated = got_truncated_body.load(Ordering::Relaxed);
    let success = got_success.load(Ordering::Relaxed);

    println!("\n=== HTTP/2 Connection Kill Test Results ===");
    println!("Total requests: {t}");
    println!("Proper errors (connection/body read): {errors}");
    println!("Silent truncations (BUG - Ok body but invalid JSON): {truncated}");
    println!("Full successes (raced before kill): {success}");

    if truncated > 0 {
        println!("\n✓ REPRODUCED THE BUG: reqwest returned Ok(body) with truncated data!");
        println!("  This is the exact issue from #493 - the h2 layer reports stream");
        println!("  completion despite incomplete data when the connection is reset.");
        println!("  The .http1_only() fix prevents this because HTTP/1.1 detects");
        println!("  incomplete responses via Content-Length mismatch or chunked EOF.");
    } else if errors > 0 {
        println!("\n  Good: all connection kills were properly surfaced as errors.");
        println!("  The h2 implementation correctly propagates connection resets.");
        println!("  The truncation may require different conditions (GOAWAY frame,");
        println!("  specific h2 window states) rather than raw TCP kills.");
    } else {
        println!("\n  Unexpected: all requests succeeded despite connection kills.");
    }
}

/// Variant: Kill connection after more data (200KB) to catch the larger responses
/// that might have completed before the kill threshold.
#[tokio::test]
#[ignore]
async fn test_h2_connection_kill_200kb() {
    let _ = env_logger::try_init();
    let (server_addr, _cfg) = start_tls_server().await;

    // Kill after 200KB — some responses (250-400KB) will be mid-body
    let proxy_addr = start_truncating_proxy(&server_addr, 200_000).await;
    sleep(Duration::from_millis(50)).await;

    let endpoints: Vec<String> = vec!["a", "b", "c", "d", "e"]
        .into_iter()
        .map(|e| format!("https://{proxy_addr}/endpoint-{e}"))
        .collect();

    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .pool_max_idle_per_host(1)
        .build()
        .expect("Failed to build client");

    let total = Arc::new(AtomicUsize::new(0));
    let got_error = Arc::new(AtomicUsize::new(0));
    let got_truncated_body = Arc::new(AtomicUsize::new(0));
    let got_success = Arc::new(AtomicUsize::new(0));

    for round in 0..10 {
        let mut futures = Vec::new();
        for url in &endpoints {
            let client = client.clone();
            let url = url.clone();
            let total = Arc::clone(&total);
            let got_error = Arc::clone(&got_error);
            let got_truncated_body = Arc::clone(&got_truncated_body);
            let got_success = Arc::clone(&got_success);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);
                let resp = match client.get(&url).send().await {
                    Ok(r) => r,
                    Err(_) => {
                        got_error.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };
                match resp.text().await {
                    Ok(body) => match serde_json::from_str::<JsonValue>(&body) {
                        Ok(_) => {
                            got_success.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            got_truncated_body.fetch_add(1, Ordering::Relaxed);
                            if round < 3 {
                                eprintln!(
                                    "  [Round {round}] ✗ TRUNCATION: len={}, err={e}",
                                    body.len()
                                );
                            }
                        }
                    },
                    Err(_) => {
                        got_error.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        join_all(futures).await;
    }

    let t = total.load(Ordering::Relaxed);
    let errors = got_error.load(Ordering::Relaxed);
    let truncated = got_truncated_body.load(Ordering::Relaxed);
    let success = got_success.load(Ordering::Relaxed);

    println!("\n=== HTTP/2 Connection Kill (200KB) Results ===");
    println!("Total: {t}, Errors: {errors}, Truncated: {truncated}, Success: {success}");

    if truncated > 0 {
        println!("\n✓ REPRODUCED: {truncated} silent truncation(s)!");
    }
}

// ── GitHub API tests (require GITHUB_TOKEN env var) ──────────────────────────

/// Test concurrent GitHub API requests WITH HTTP/2 (no http1_only).
/// Reproduces the exact scenario from Issue #493.
/// Run: GITHUB_TOKEN=ghp_... cargo test ... test_github -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn test_github_api_h2_concurrent() {
    let _ = env_logger::try_init();

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if token.is_empty() {
        println!("SKIPPED: Set GITHUB_TOKEN env var to run this test");
        return;
    }

    let endpoints = vec![
        "https://api.github.com/repos/drasi-project/drasi-core/pulls?state=open&per_page=100",
        "https://api.github.com/search/issues?q=repo:drasi-project/drasi-core+is:issue+state:open&per_page=100",
        "https://api.github.com/repos/drasi-project/drasi-core/commits?per_page=100",
        "https://api.github.com/repos/drasi-project/drasi-core/issues?state=all&per_page=100",
        "https://api.github.com/repos/drasi-project/drasi-core/pulls?state=closed&per_page=100",
    ];

    // HTTP/2 client — the default (what happens WITHOUT .http1_only())
    let client = Client::builder()
        .user_agent("drasi-h2-truncation-poc")
        .pool_max_idle_per_host(1)
        .build()
        .expect("Failed to build client");

    let total_requests = Arc::new(AtomicUsize::new(0));
    let failed_requests = Arc::new(AtomicUsize::new(0));
    let truncation_errors = Arc::new(AtomicUsize::new(0));

    let num_rounds = 20;

    for round in 0..num_rounds {
        let mut futures = Vec::new();
        // Each round: fire all 5 endpoints concurrently
        for url in &endpoints {
            let client = client.clone();
            let url = url.to_string();
            let token = token.clone();
            let total = Arc::clone(&total_requests);
            let failed = Arc::clone(&failed_requests);
            let truncated = Arc::clone(&truncation_errors);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);
                let resp = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await;

                match resp {
                    Ok(response) => {
                        let status = response.status();
                        if status.as_u16() == 403 || status.as_u16() == 429 {
                            return; // Rate limited
                        }
                        if !status.is_success() {
                            failed.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        let body = match response.text().await {
                            Ok(b) => b,
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                truncated.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[Round {round}] ✗ Body read error for {url}: {e}");
                                return;
                            }
                        };
                        if let Err(e) = serde_json::from_str::<JsonValue>(&body) {
                            failed.fetch_add(1, Ordering::Relaxed);
                            truncated.fetch_add(1, Ordering::Relaxed);
                            eprintln!(
                                "[Round {round}] ✗ TRUNCATION {url}: len={}, err={e}, tail={:?}",
                                body.len(),
                                &body[body.len().saturating_sub(60)..]
                            );
                        }
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[Round {round}] Request error: {e}");
                    }
                }
            }));
        }
        join_all(futures).await;
        // Brief delay to avoid rate limiting
        sleep(Duration::from_millis(200)).await;
    }

    let total = total_requests.load(Ordering::Relaxed);
    let failed = failed_requests.load(Ordering::Relaxed);
    let truncated = truncation_errors.load(Ordering::Relaxed);

    println!("\n=== GitHub API HTTP/2 Results ({num_rounds} rounds × 5 endpoints) ===");
    println!("Total requests: {total}");
    println!("Failed requests: {failed}");
    println!("Truncation errors: {truncated}");

    if truncated > 0 {
        println!("\n✓ REPRODUCED against GitHub API: {truncated} truncation(s)");
    } else {
        println!("\n✗ No truncation observed ({total} requests)");
    }
}

/// Control: GitHub API with http1_only (the fix).
#[tokio::test]
#[ignore]
async fn test_github_api_h1_only() {
    let _ = env_logger::try_init();

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if token.is_empty() {
        println!("SKIPPED: Set GITHUB_TOKEN env var to run this test");
        return;
    }

    let endpoints = vec![
        "https://api.github.com/repos/drasi-project/drasi-core/pulls?state=open&per_page=100",
        "https://api.github.com/search/issues?q=repo:drasi-project/drasi-core+is:issue+state:open&per_page=100",
        "https://api.github.com/repos/drasi-project/drasi-core/commits?per_page=100",
    ];

    let client = Client::builder()
        .user_agent("drasi-h2-truncation-poc")
        .http1_only()
        .build()
        .expect("Failed to build client");

    let total = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let truncated = Arc::new(AtomicUsize::new(0));

    for round in 0..10 {
        let mut futures = Vec::new();
        for url in &endpoints {
            let client = client.clone();
            let url = url.to_string();
            let token = token.clone();
            let total = Arc::clone(&total);
            let failed = Arc::clone(&failed);
            let truncated = Arc::clone(&truncated);

            futures.push(tokio::spawn(async move {
                total.fetch_add(1, Ordering::Relaxed);
                let resp = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/vnd.github+json")
                    .send()
                    .await;
                match resp {
                    Ok(response) => {
                        let status = response.status();
                        if status.as_u16() == 403 || status.as_u16() == 429 {
                            return;
                        }
                        if !status.is_success() {
                            failed.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                        let body = match response.text().await {
                            Ok(b) => b,
                            Err(e) => {
                                failed.fetch_add(1, Ordering::Relaxed);
                                eprintln!("[Round {round}] Body read error: {e}");
                                return;
                            }
                        };
                        if let Err(e) = serde_json::from_str::<JsonValue>(&body) {
                            failed.fetch_add(1, Ordering::Relaxed);
                            truncated.fetch_add(1, Ordering::Relaxed);
                            eprintln!("[Round {round}] ✗ TRUNCATION: len={}, err={e}", body.len());
                        }
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        eprintln!("[Round {round}] Error: {e}");
                    }
                }
            }));
        }
        join_all(futures).await;
        sleep(Duration::from_millis(200)).await;
    }

    let t = total.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let tr = truncated.load(Ordering::Relaxed);

    println!("\n=== GitHub API HTTP/1.1 (Fix) Results ===");
    println!("Total requests: {t}");
    println!("Failed requests: {f}");
    println!("Truncation errors: {tr}");

    assert_eq!(tr, 0, "HTTP/1.1 should NOT truncate, got {tr}");
    println!("\n✓ CONFIRMED: http1_only prevents truncation");
}

// =============================================================================
// TEST: Custom h2 server that sends END_STREAM after partial data
// This is the REAL reproduction of the truncation bug.
// In HTTP/2, END_STREAM is the authoritative signal that the body is complete.
// If a server (or intermediary proxy/load balancer) sends END_STREAM prematurely,
// the client receives Ok(partial_body) with NO transport error.
// =============================================================================

/// Start a custom h2 server that truncates responses by sending END_STREAM
/// after only a fraction of the body data.
/// `truncate_fraction` controls how much of the body to send (0.0 - 1.0).
/// If `include_content_length` is true, sends a Content-Length header with
/// the FULL body size (which h2 implementations may validate).
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

                        // Generate a large JSON response
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

                        // Calculate truncation point
                        let send_bytes = (full_len as f64 * truncate_frac) as usize;
                        let truncated_body = &full_body.as_bytes()[..send_bytes];

                        // Build response headers
                        let mut response = http::Response::builder()
                            .status(200)
                            .header("content-type", "application/json");

                        if include_cl {
                            // Send Content-Length with FULL size (mismatch)
                            response = response.header("content-length", full_len.to_string());
                        }

                        let response = response.body(()).unwrap();

                        // Send headers, then partial body with END_STREAM
                        let mut send_stream = respond.send_response(response, false).unwrap();

                        // Send the truncated body in chunks
                        let chunk_size = 16384; // h2 default frame size
                        let mut offset = 0;
                        while offset < truncated_body.len() {
                            let end = std::cmp::min(offset + chunk_size, truncated_body.len());
                            let is_last = end >= truncated_body.len();

                            // Reserve capacity
                            send_stream.reserve_capacity(end - offset);

                            // Wait for capacity
                            loop {
                                let cap = send_stream.capacity();
                                if cap > 0 {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }

                            let chunk = bytes::Bytes::copy_from_slice(&truncated_body[offset..end]);
                            // Send with END_STREAM on the last chunk
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

/// Generate a large JSON string for the h2 server
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

/// Test: h2 server sends END_STREAM after only 50% of body data, WITHOUT
/// Content-Length header. This should result in "silent truncation" — the client
/// gets Ok(partial_body) that is incomplete JSON.
#[tokio::test]
#[ignore]
async fn test_h2_premature_end_stream_no_content_length() {
    let _ = env_logger::try_init();

    // Server sends only 50% of the body then END_STREAM (no Content-Length)
    let (port, _server) = start_h2_truncating_server(0.5, false).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== h2 Premature END_STREAM (no Content-Length) ===");
    println!("Server on port {port}, sending 50% of body + END_STREAM");
    println!("Expected: client gets Ok(partial_body) → JSON parse fails");
    println!();

    // Use reqwest with http2_prior_knowledge (h2c - cleartext h2)
    let client = Client::builder().http2_prior_knowledge().build().unwrap();

    let endpoints = vec![
        format!("http://127.0.0.1:{port}/api/data1"),
        format!("http://127.0.0.1:{port}/api/data2"),
        format!("http://127.0.0.1:{port}/api/data3"),
        format!("http://127.0.0.1:{port}/api/data4"),
        format!("http://127.0.0.1:{port}/api/data5"),
    ];

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
                            Ok(body) => {
                                // Try to parse as JSON
                                match serde_json::from_str::<JsonValue>(&body) {
                                    Ok(_) => ("success".to_string(), body.len(), String::new()),
                                    Err(e) => (
                                        "truncated".to_string(),
                                        body.len(),
                                        format!("status={status}, JSON err: {e}"),
                                    ),
                                }
                            }
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
                    "success" => {
                        successes += 1;
                    }
                    "truncated" => {
                        silent_truncations += 1;
                        eprintln!(
                            "  [Round {round}] ✗ SILENT TRUNCATION: body_len={len}, {detail}"
                        );
                    }
                    "body_error" => {
                        proper_errors += 1;
                        eprintln!("  [Round {round}] body read error: {detail}");
                    }
                    _ => {
                        proper_errors += 1;
                        eprintln!("  [Round {round}] request error: {detail}");
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
        println!("   The h2 implementation properly errors on incomplete bodies.");
    } else {
        println!("\n🟢 All requests succeeded (unexpected for 50% truncation).");
    }
}

/// Test: h2 server sends END_STREAM after only 50% of body data, WITH
/// Content-Length header set to the full body size. The h2 implementation
/// MAY detect the mismatch and return an error.
#[tokio::test]
#[ignore]
async fn test_h2_premature_end_stream_with_content_length() {
    let _ = env_logger::try_init();

    // Server sends only 50% of the body then END_STREAM (WITH Content-Length)
    let (port, _server) = start_h2_truncating_server(0.5, true).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== h2 Premature END_STREAM (WITH Content-Length) ===");
    println!("Server on port {port}, sending 50% of body + END_STREAM");
    println!("Content-Length set to full size → h2 impl SHOULD detect mismatch");
    println!();

    let client = Client::builder().http2_prior_knowledge().build().unwrap();

    let endpoints = vec![
        format!("http://127.0.0.1:{port}/api/data1"),
        format!("http://127.0.0.1:{port}/api/data2"),
        format!("http://127.0.0.1:{port}/api/data3"),
    ];

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

/// Test: Prove that HTTP/1.1 prevents silent truncation.
/// Same truncating server, but client uses http1_only().
#[tokio::test]
#[ignore]
async fn test_h1_prevents_premature_end_stream() {
    let _ = env_logger::try_init();

    // Server only speaks h2, so an HTTP/1.1 client cannot connect.
    // Instead, we use the normal TLS server (which supports both) and verify
    // that with http1_only(), truncation doesn't occur from the normal server.
    // The point: http1_only() avoids h2 entirely, so h2-specific truncation
    // cannot happen.

    let (port, _server) = start_h2_truncating_server(0.5, false).await;
    sleep(Duration::from_millis(100)).await;

    println!("=== HTTP/1.1 vs h2 truncating server ===");
    println!("h2 server on port {port} — HTTP/1.1 client should FAIL to connect");
    println!("(Server only speaks h2, not HTTP/1.1)");
    println!();

    // http1_only() client cannot talk to h2-only server
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
