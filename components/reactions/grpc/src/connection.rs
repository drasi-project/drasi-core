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

//! Connection management for gRPC reactions.
//!
//! This module handles the creation and lifecycle of gRPC client connections,
//! including retry logic and error handling.

use anyhow::Result;
use log::{error, info, warn};
use std::time::Duration;
use tonic::transport::Channel;

use super::proto::ReactionServiceClient;

/// Connection state for the gRPC client
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed,
    Reconnecting,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::Connecting => write!(f, "Connecting"),
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Failed => write!(f, "Failed"),
            ConnectionState::Reconnecting => write!(f, "Reconnecting"),
        }
    }
}

/// Create a lazy gRPC client connection
///
/// This creates a gRPC channel that connects lazily - the actual TCP connection
/// is established when the first RPC call is made.
///
/// # Arguments
/// * `endpoint` - The gRPC endpoint URL (e.g., "grpc://localhost:50052")
/// * `timeout_ms` - Request timeout in milliseconds
///
/// # Returns
/// A ReactionServiceClient configured with the specified endpoint and timeout
pub async fn create_client(
    endpoint: &str,
    timeout_ms: u64,
) -> Result<ReactionServiceClient<Channel>> {
    let endpoint = endpoint.replace("grpc://", "http://");
    info!("Creating lazy gRPC channel to {endpoint} with timeout: {timeout_ms}ms");
    let channel = Channel::from_shared(endpoint.clone())?
        .timeout(Duration::from_millis(timeout_ms))
        .connect_lazy();
    info!("Lazy channel created; actual socket connect will occur on first RPC");
    let client = ReactionServiceClient::new(channel);
    info!("ReactionServiceClient created - connection will establish on first RPC call");
    Ok(client)
}

/// Create a gRPC client connection with retry logic
///
/// Attempts to create a connection with exponential backoff between retries.
///
/// # Arguments
/// * `endpoint` - The gRPC endpoint URL
/// * `timeout_ms` - Request timeout in milliseconds
/// * `max_retries` - Maximum number of retry attempts
///
/// # Returns
/// A ReactionServiceClient if connection succeeds, or an error if all retries fail
pub async fn create_client_with_retry(
    endpoint: &str,
    timeout_ms: u64,
    max_retries: u32,
) -> Result<ReactionServiceClient<Channel>> {
    let mut retries = 0;
    let mut backoff = Duration::from_millis(500);

    loop {
        match create_client(endpoint, timeout_ms).await {
            Ok(client) => {
                info!("Successfully created client for endpoint: {endpoint}");
                return Ok(client);
            }
            Err(e) => {
                if retries >= max_retries {
                    error!("Failed to create client after {max_retries} retries: {e}");
                    return Err(e);
                }
                warn!(
                    "Failed to create client (attempt {}/{}): {}",
                    retries + 1,
                    max_retries,
                    e
                );
                retries += 1;
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}
