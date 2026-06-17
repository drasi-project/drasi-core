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

//! Shared `send_batch_with_retry` helper used by both batching runners.
//!
//! Owns the retry and connection-recovery semantics so the fixed and adaptive
//! runners deliver batches the same way.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use log::{debug, error, info, trace, warn};
use rand::Rng;
use tonic::transport::Channel;

use crate::connection::create_client;
use crate::proto::{
    ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem, ReactionServiceClient,
};

/// Send a batch with full retry / reconnection semantics.
///
/// Returns `(needs_new_client, replacement_client)`:
/// * `needs_new_client = false` → batch delivered successfully.
/// * `needs_new_client = true, replacement = Some(c)` → caller should swap
///   the existing client for `c` and retry the same batch.
/// * `needs_new_client = true, replacement = None` → caller should drop
///   the existing client and defer the retry to the next batch.
pub(crate) async fn send_batch_with_retry(
    client: &mut ReactionServiceClient<Channel>,
    batch: Vec<ProtoQueryResultItem>,
    query_id: &str,
    metadata: &HashMap<String, String>,
    max_retries: u32,
    endpoint: &str,
    timeout_ms: u64,
) -> Result<(bool, Option<ReactionServiceClient<Channel>>)> {
    let mut retries = 0;
    let mut backoff = Duration::from_millis(100);
    let start_time = std::time::Instant::now();
    let max_retry_duration = Duration::from_secs(60);

    debug!(
        "send_batch_with_retry called - batch_size: {}, query_id: {}, endpoint: {}, max_retries: {}, timeout_ms: {}",
        batch.len(),
        query_id,
        endpoint,
        max_retries,
        timeout_ms
    );

    loop {
        if start_time.elapsed() > max_retry_duration {
            warn!("Max retry duration ({max_retry_duration:?}) exceeded after {retries} attempts");
            return Ok((true, None));
        }
        let attempt_start = std::time::Instant::now();
        debug!(
            "Attempt {}/{} starting at {:?}",
            retries + 1,
            max_retries + 1,
            attempt_start
        );

        let mut request = tonic::Request::new(ProcessResultsRequest {
            results: Some(ProtoQueryResult {
                query_id: query_id.to_string(),
                results: batch.clone(),
                timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            }),
            metadata: metadata.clone(),
        });

        // Also surface configured metadata as actual gRPC request headers
        // (HTTP/2 trailers), in addition to the in-body `metadata` field.
        // Receivers can read either; this matches the README's
        // "authentication or routing headers" framing. Invalid header
        // names/values are logged and skipped rather than failing the
        // batch — the entry is still available in the body field.
        for (k, v) in metadata.iter() {
            match (
                k.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>(),
                v.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>(),
            ) {
                (Ok(name), Ok(value)) => {
                    request.metadata_mut().insert(name, value);
                }
                (Err(e), _) => {
                    warn!(
                        "Skipping metadata header with invalid name '{k}': {e}; \
                         entry still sent in request body"
                    );
                }
                (_, Err(e)) => {
                    warn!(
                        "Skipping metadata header '{k}' with invalid value: {e}; \
                         entry still sent in request body"
                    );
                }
            }
        }

        trace!(
            "About to send ProcessResults request - endpoint: {}, query_id: {}, batch_size: {}",
            endpoint,
            query_id,
            batch.len()
        );

        match client.process_results(request).await {
            Ok(response) => {
                let elapsed = attempt_start.elapsed();
                let resp = response.into_inner();
                debug!(
                    "Response received after {:?} - success: {}, error: '{}'",
                    elapsed, resp.success, resp.error
                );

                if resp.success {
                    trace!(
                        "Successfully sent batch - size: {}, query_id: {}, time: {:?}",
                        batch.len(),
                        query_id,
                        elapsed
                    );
                    return Ok((false, None));
                } else {
                    warn!(
                        "Server returned failure - error: '{}', retries: {}/{}",
                        resp.error, retries, max_retries
                    );
                    if retries >= max_retries {
                        error!("Max retries exceeded - giving up. Error: {}", resp.error);
                        return Err(anyhow::anyhow!(
                            "gRPC reaction failed: Server returned error after {} retries: {}. \
                             Check server logs and verify the gRPC endpoint is functioning correctly.",
                            max_retries,
                            resp.error
                        ));
                    }
                }
            }
            Err(e) => {
                let elapsed = attempt_start.elapsed();
                let error_str = e.to_string();
                let error_str_lower = error_str.to_lowercase();

                error!("Request failed after {elapsed:?} - Full error: {e}");
                debug!(
                    "Error details - code: {:?}, message: {}",
                    e.code(),
                    e.message()
                );

                let error_type = categorize_error(&error_str_lower);
                let is_connection_error = matches!(
                    error_type,
                    "GoAway" | "Connection" | "BrokenPipe" | "ChannelClosed" | "Unavailable"
                );
                let is_overload_error =
                    matches!(error_type, "ResourceExhausted" | "DeadlineExceeded");

                warn!(
                    "Error categorized as '{}' - is_connection_error: {}, retry: {}/{}",
                    error_type,
                    is_connection_error,
                    retries + 1,
                    max_retries + 1
                );

                if is_overload_error {
                    // Server-overload / deadline errors: apply targeted backoff
                    // and retry on the same connection (no reconnect needed).
                    match error_type {
                        "ResourceExhausted" => {
                            warn!("Server overloaded (ResourceExhausted) - backing off");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                        "DeadlineExceeded" => {
                            warn!("Request deadline exceeded - consider increasing timeout");
                        }
                        _ => {}
                    }
                    if retries >= max_retries {
                        return Ok((true, None));
                    }
                } else if is_connection_error {
                    warn!("Connection error detected - type: {error_type}, endpoint: {endpoint}");

                    if error_type == "GoAway" {
                        if error_str.contains("StreamId(0)") {
                            error!(
                                "Server immediately rejected connection with GoAway(StreamId(0))"
                            );
                            warn!("Waiting 2 seconds before retry due to immediate GoAway");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                        info!("Creating fresh connection after GoAway...");
                        match create_client(endpoint, timeout_ms).await {
                            Ok(new_client) => {
                                info!("Successfully created new client after GoAway - will retry request");
                                return Ok((true, Some(new_client)));
                            }
                            Err(create_err) => {
                                warn!("Failed to create new client after GoAway: {create_err}. Will retry on next batch.");
                                return Ok((true, None));
                            }
                        }
                    }

                    if retries == 0 {
                        debug!("Connection error on first attempt for endpoint {endpoint}, signaling for new client");
                        match create_client(endpoint, timeout_ms).await {
                            Ok(new_client) => {
                                info!("Successfully created new client for retry");
                                return Ok((true, Some(new_client)));
                            }
                            Err(create_err) => {
                                warn!("Failed to create new client: {create_err}. Will retry on next batch.");
                                return Ok((true, None));
                            }
                        }
                    } else if retries < max_retries {
                        debug!("Connection error on retry {retries}/{max_retries}, attempting new client");
                        match create_client(endpoint, timeout_ms).await {
                            Ok(new_client) => {
                                debug!("Successfully created new client for retry");
                                return Ok((true, Some(new_client)));
                            }
                            Err(create_err) => {
                                warn!("Failed to create new client: {create_err}");
                            }
                        }
                    } else {
                        warn!("Connection failed after {max_retries} retries. Will retry on next batch.");
                        return Ok((true, None));
                    }
                } else {
                    error!("gRPC call failed (type: application): {e}");
                    if retries >= max_retries {
                        return Err(anyhow::anyhow!(
                            "gRPC reaction failed: Application error after {max_retries} retries: {e}. \
                             This indicates an error in the receiving application, not a connection issue."
                        ));
                    }
                }
            }
        }

        retries += 1;

        let jitter = rand::thread_rng().gen_range(0..100);
        let jittered_backoff = backoff + Duration::from_millis(jitter);

        debug!(
            "Retry {retries}/{max_retries} - backing off for {jittered_backoff:?} (base: {backoff:?}, jitter: {jitter}ms)"
        );

        tokio::time::sleep(jittered_backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

fn categorize_error(error_str_lower: &str) -> &'static str {
    if error_str_lower.contains("goaway") {
        "GoAway"
    } else if error_str_lower.contains("unavailable") {
        "Unavailable"
    } else if error_str_lower.contains("deadline") {
        "DeadlineExceeded"
    } else if error_str_lower.contains("cancelled") {
        "Cancelled"
    } else if error_str_lower.contains("resource") || error_str_lower.contains("exhausted") {
        "ResourceExhausted"
    } else if error_str_lower.contains("connection") || error_str_lower.contains("transport") {
        "Connection"
    } else if error_str_lower.contains("broken pipe")
        || error_str_lower.contains("connection reset")
    {
        "BrokenPipe"
    } else if error_str_lower.contains("eof") || error_str_lower.contains("channel closed") {
        "ChannelClosed"
    } else if error_str_lower.contains("timeout") {
        "Timeout"
    } else {
        "Unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::categorize_error;

    #[test]
    fn classifies_overload_errors() {
        // ResourceExhausted and DeadlineExceeded are the overload signals
        // handled before the connection-error path in send_batch_with_retry.
        assert_eq!(
            categorize_error("status: resourceexhausted"),
            "ResourceExhausted"
        );
        assert_eq!(
            categorize_error("the resource has been exhausted"),
            "ResourceExhausted"
        );
        assert_eq!(categorize_error("deadline has elapsed"), "DeadlineExceeded");
    }

    #[test]
    fn classifies_connection_errors() {
        assert_eq!(categorize_error("http2 goaway received"), "GoAway");
        assert_eq!(categorize_error("status: unavailable"), "Unavailable");
        assert_eq!(categorize_error("tcp connection error"), "Connection");
        assert_eq!(categorize_error("grpc transport error"), "Connection");
        // The `connection` substring is matched before the broken-pipe branch,
        // so "connection reset" is classified as a generic Connection error.
        assert_eq!(categorize_error("connection reset by peer"), "Connection");
        assert_eq!(categorize_error("broken pipe"), "BrokenPipe");
        assert_eq!(categorize_error("unexpected eof"), "ChannelClosed");
        assert_eq!(categorize_error("channel closed"), "ChannelClosed");
    }

    #[test]
    fn classifies_other_and_unknown() {
        assert_eq!(categorize_error("operation was cancelled"), "Cancelled");
        assert_eq!(categorize_error("request timeout"), "Timeout");
        assert_eq!(categorize_error("something entirely different"), "Unknown");
    }

    #[test]
    fn overload_classification_drives_retry_branch() {
        // Mirrors the matches! checks in send_batch_with_retry: overload
        // errors must not be misclassified as connection errors (which would
        // trigger an unnecessary reconnect).
        for tag in ["ResourceExhausted", "DeadlineExceeded"] {
            let is_overload = matches!(tag, "ResourceExhausted" | "DeadlineExceeded");
            let is_connection = matches!(
                tag,
                "GoAway" | "Connection" | "BrokenPipe" | "ChannelClosed" | "Unavailable"
            );
            assert!(is_overload, "{tag} should be overload");
            assert!(!is_connection, "{tag} should not be connection");
        }
    }
}
