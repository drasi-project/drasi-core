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

//! Coalesced batch delivery, used only when adaptive mode is enabled
//! AND `batch_endpoint` is configured on the reaction.
//!
//! The wire payload is a JSON array of [`BatchResult`] values — one per
//! query represented in the coalesced batch — each carrying its
//! [`DefaultChangeNotification`] entries in order.

use anyhow::Result;
use log::{debug, warn};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use std::collections::HashMap;

// Re-export the on-the-wire batch envelope from its canonical home in
// `crate::output` so the public path `drasi_reaction_http::BatchResult`
// continues to resolve.
pub use crate::output::{BatchResult, DefaultChangeNotification};

/// POST a coalesced batch to `{base_url}{batch_endpoint}` as a JSON array
/// of [`BatchResult`].
pub(crate) async fn send_coalesced_batch(
    client: &Client,
    base_url: &str,
    batch_endpoint: &str,
    token: &Option<String>,
    batches_by_query: HashMap<String, Vec<DefaultChangeNotification>>,
    reaction_name: &str,
) -> Result<()> {
    let batch_results: Vec<BatchResult> = batches_by_query
        .into_iter()
        .map(|(query_id, results)| BatchResult {
            count: results.len(),
            query_id,
            results,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
        .collect();

    let batch_url = format!("{base_url}{batch_endpoint}");
    let body = serde_json::to_string(&batch_results)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }

    let total: usize = batch_results.iter().map(|b| b.count).sum();
    debug!("[{reaction_name}] Sending coalesced batch of {total} results to {batch_url}");

    let response = client
        .post(&batch_url)
        .headers(headers)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unable to read response body".to_string());
        warn!(
            "[{reaction_name}] Batch HTTP request failed with status {}: {error_body}",
            status.as_u16()
        );
    } else {
        debug!("[{reaction_name}] Batch sent successfully");
    }

    Ok(())
}
