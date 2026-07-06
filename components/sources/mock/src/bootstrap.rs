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

//! A minimal bootstrap provider for the mock source.
//!
//! It emits a deterministic batch of node-insert bootstrap events. Each element
//! carries `Arc<str>` labels/properties — the exact `repr(Rust)` payload shape
//! that caused issue #602 — so the dynamic-plugin bootstrap path is exercised
//! end to end across the cdylib FFI boundary (see the bootstrap scenario in
//! `host-sdk/tests/ffi_layout_mismatch_test.rs`).

use std::sync::Arc;

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::events::BootstrapEvent;
use drasi_lib::channels::BootstrapEventSender;

/// Number of synthetic bootstrap events the mock source emits per subscription.
pub const MOCK_BOOTSTRAP_EVENT_COUNT: u64 = 10;

/// Emits a fixed batch of node-insert bootstrap events for the mock source.
pub struct MockBootstrapProvider {
    source_id: String,
    count: u64,
}

impl MockBootstrapProvider {
    /// Create a provider that emits [`MOCK_BOOTSTRAP_EVENT_COUNT`] events.
    pub fn new(source_id: impl Into<String>) -> Self {
        Self {
            source_id: source_id.into(),
            count: MOCK_BOOTSTRAP_EVENT_COUNT,
        }
    }
}

#[async_trait]
impl BootstrapProvider for MockBootstrapProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        _context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        let mut sent = 0usize;
        for seq in 1..=self.count {
            let element_id = format!("bootstrap_{seq}");
            let reference = ElementReference::new(&self.source_id, &element_id);

            let mut properties = ElementPropertyMap::new();
            properties.insert("value", ElementValue::Integer(seq as i64));
            properties.insert("name", ElementValue::String(Arc::from(element_id.as_str())));

            let metadata = ElementMetadata {
                reference,
                labels: Arc::from(vec![Arc::from("Bootstrap")]),
                effective_from: 0,
            };
            let element = Element::Node {
                metadata,
                properties,
            };

            let event = BootstrapEvent {
                source_id: self.source_id.clone(),
                change: SourceChange::Insert { element },
                timestamp: chrono::Utc::now(),
                sequence: seq,
            };

            // Receiver dropped (e.g. subscriber went away) — stop early.
            if event_tx.send(event).await.is_err() {
                break;
            }
            sent += 1;
        }

        Ok(BootstrapResult {
            event_count: sent,
            source_position: None,
        })
    }
}
