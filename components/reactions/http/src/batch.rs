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
//! The wire payload is a single Pattern C [`BatchEnvelope`] whose `batch`
//! field is a JSON array of rendered items — either a per-query body template
//! rendered for each change, or the default change-notification envelope when
//! no body template applies.

use anyhow::Result;
use log::{debug, warn};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Method,
};

use crate::output::BatchEnvelope;
use crate::process::{send_with_retry, DeliveryOutcome};

/// POST a coalesced batch to `{base_url}{batch_endpoint}` as a single
/// [`BatchEnvelope`] (`{ "batch": [ … ] }`).
///
/// Returns the [`DeliveryOutcome`]: `Delivered` when the server accepted the
/// batch, or `Dropped` when it was permanently rejected (a poison batch). The
/// caller advances the checkpoint in both cases, but the distinction is surfaced
/// so a permanent rejection is logged as such rather than as a success.
pub(crate) async fn send_coalesced_batch(
    client: &Client,
    base_url: &str,
    batch_endpoint: &str,
    token: &Option<String>,
    items: Vec<serde_json::Value>,
    reaction_name: &str,
) -> Result<DeliveryOutcome> {
    let total = items.len();
    let envelope = BatchEnvelope { batch: items };

    let batch_url = format!("{base_url}{batch_endpoint}");
    let body = serde_json::to_string(&envelope)?;

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    if let Some(token) = token {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }

    debug!("[{reaction_name}] Sending coalesced batch of {total} results to {batch_url}");

    let outcome = send_with_retry(
        client,
        Method::POST,
        batch_url,
        headers,
        body,
        reaction_name,
        "batch HTTP request",
    )
    .await?;

    match outcome {
        DeliveryOutcome::Delivered => {
            debug!("[{reaction_name}] Batch of {total} results sent successfully")
        }
        DeliveryOutcome::Dropped => warn!(
            "[{reaction_name}] Coalesced batch of {total} results permanently rejected by the \
             server (poison drop); advancing past it"
        ),
    }
    Ok(outcome)
}
