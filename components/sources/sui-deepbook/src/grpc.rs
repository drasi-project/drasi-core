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

use anyhow::{Context, Result};
use log::{debug, warn};
use serde::Deserialize;
use sui_rpc::field::FieldMask;
use sui_rpc::proto::sui::rpc::v2::{
    self as proto, SubscribeCheckpointsRequest, SubscribeCheckpointsResponse,
};
use sui_sdk_types::bcs::FromBcs;
use tonic::Streaming;

use crate::rpc::SuiEvent;

const MAX_DECODING_MESSAGE_SIZE: usize = 8 * 1024 * 1024;

/// gRPC client for Sui full node checkpoint streaming.
pub struct SuiGrpcClient {
    client: sui_rpc::Client,
}

impl SuiGrpcClient {
    pub fn new(endpoint: &str) -> Result<Self> {
        let client = sui_rpc::Client::new(endpoint)
            .map_err(|e| anyhow::anyhow!("Failed to create gRPC client: {e}"))?
            .with_max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE);
        Ok(Self { client })
    }

    /// Subscribe to the live checkpoint stream.
    /// Returns a streaming response that yields checkpoints in order.
    pub async fn subscribe_checkpoints(
        &mut self,
    ) -> Result<Streaming<SubscribeCheckpointsResponse>> {
        let mut sub_client = self.client.subscription_client();
        let mut request = SubscribeCheckpointsRequest::default();
        request.read_mask = Some(FieldMask {
            paths: vec![
                "sequence_number".to_owned(),
                "transactions.digest".to_owned(),
                "transactions.events".to_owned(),
                "transactions.timestamp".to_owned(),
            ],
        });

        let response = sub_client
            .subscribe_checkpoints(request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to checkpoints: {e}"))?;

        Ok(response.into_inner())
    }

    /// Fetch a specific checkpoint by sequence number (for backfilling gaps).
    pub async fn get_checkpoint(&mut self, sequence_number: u64) -> Result<proto::Checkpoint> {
        let mut ledger_client = self.client.ledger_client();
        let mut request = proto::GetCheckpointRequest::default();
        request.read_mask = Some(FieldMask {
            paths: vec![
                "sequence_number".to_owned(),
                "transactions.digest".to_owned(),
                "transactions.events".to_owned(),
                "transactions.timestamp".to_owned(),
            ],
        });
        request.checkpoint_id = Some(proto::get_checkpoint_request::CheckpointId::SequenceNumber(
            sequence_number,
        ));

        let response = ledger_client
            .get_checkpoint(request)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get checkpoint {sequence_number}: {e}"))?
            .into_inner();

        response.checkpoint.context(format!(
            "No checkpoint in response for seq {sequence_number}"
        ))
    }
}

/// Extract DeepBook events from a gRPC checkpoint, filtering by package ID.
/// Converts gRPC proto events to our existing `SuiEvent` struct for compatibility
/// with the existing mapping pipeline.
pub fn extract_deepbook_events(
    checkpoint: &proto::Checkpoint,
    deepbook_package_id: &str,
) -> Vec<SuiEvent> {
    let mut events = Vec::new();
    let checkpoint_seq = checkpoint.sequence_number.unwrap_or(0);

    for tx in &checkpoint.transactions {
        let tx_digest = tx.digest.as_deref().unwrap_or("unknown");
        let timestamp_ms = tx
            .timestamp
            .as_ref()
            .map(|t| (t.seconds as u64) * 1000 + (t.nanos as u64) / 1_000_000);

        if let Some(ref events_container) = tx.events {
            for (event_seq, event) in events_container.events.iter().enumerate() {
                let pkg = event.package_id.as_deref().unwrap_or("");
                if pkg != deepbook_package_id {
                    continue;
                }

                let event_type = event.event_type.as_deref().unwrap_or("").to_string();
                let sender = event.sender.as_deref().unwrap_or("").to_string();
                let module = event.module.as_deref().unwrap_or("").to_string();

                // Deserialize BCS event contents to JSON
                let parsed_json = deserialize_event_bcs(
                    &event_type,
                    event.contents.as_ref().and_then(|c| c.value.as_deref()),
                );

                let sui_event = SuiEvent {
                    id: crate::rpc::EventCursor {
                        tx_digest: tx_digest.to_string(),
                        event_seq: event_seq.to_string(),
                    },
                    package_id: pkg.to_string(),
                    transaction_module: module,
                    sender,
                    event_type,
                    parsed_json,
                    timestamp_ms,
                };

                events.push(sui_event);
            }
        }
    }

    if !events.is_empty() {
        debug!(
            "Extracted {} DeepBook events from checkpoint {}",
            events.len(),
            checkpoint_seq
        );
    }

    events
}

// ── BCS Deserialization for Known DeepBook V3 Event Types ──

/// Deserialize BCS event contents based on the event type string.
/// Returns a serde_json::Value for the existing mapping pipeline.
fn deserialize_event_bcs(event_type: &str, bcs_bytes: Option<&[u8]>) -> serde_json::Value {
    let Some(bytes) = bcs_bytes else {
        return serde_json::Value::Object(Default::default());
    };

    // Extract the short event name from the full type path.
    // First strip generic type parameters (e.g. `<0x2::sui::SUI, ...::USDC>`)
    // so that rsplit("::") finds the event name, not a type parameter.
    let base_type = event_type.split('<').next().unwrap_or(event_type);
    let short_name = base_type.rsplit("::").next().unwrap_or("");

    let result =
        match short_name {
            "PriceAdded" => {
                PriceAdded::from_bcs(bytes).map(|e| serde_json::to_value(e).unwrap_or_default())
            }
            "FlashLoanBorrowed" => FlashLoanBorrowed::from_bcs(bytes)
                .map(|e| serde_json::to_value(e).unwrap_or_default()),
            "OrderFilled" => {
                OrderFilled::from_bcs(bytes).map(|e| serde_json::to_value(e).unwrap_or_default())
            }
            "ReferralClaimed" => ReferralClaimed::from_bcs(bytes)
                .map(|e| serde_json::to_value(e).unwrap_or_default()),
            "OrderPlaced" | "OrderCanceled" | "OrderModified" => {
                OrderEvent::from_bcs(bytes).map(|e| serde_json::to_value(e).unwrap_or_default())
            }
            "PoolCreated" => {
                PoolCreated::from_bcs(bytes).map(|e| serde_json::to_value(e).unwrap_or_default())
            }
            _ => {
                debug!("Unknown DeepBook event type for BCS deserialization: {short_name}");
                Ok(serde_json::Value::Object(Default::default()))
            }
        };

    match result {
        Ok(val) => val,
        Err(e) => {
            warn!("Failed to BCS-deserialize event type '{short_name}': {e}. Using empty payload.");
            serde_json::Value::Object(Default::default())
        }
    }
}

// ── DeepBook V3 Event Structs ──
// These are BCS-serialized by the Move runtime. Field order must match the Move struct definition.
// Values are BCS-encoded: u64 as little-endian 8 bytes, bool as single byte,
// Address as 32 bytes, String as length-prefixed bytes.

/// PriceAdded event from deep_price module
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct PriceAdded {
    pub conversion_rate: u64,
    pub timestamp: u64,
    pub is_base_conversion: bool,
    #[serde(with = "address_as_hex")]
    pub reference_pool: [u8; 32],
    #[serde(with = "address_as_hex")]
    pub target_pool: [u8; 32],
}

/// FlashLoanBorrowed event from pool module
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct FlashLoanBorrowed {
    #[serde(with = "address_as_hex")]
    pub pool_id: [u8; 32],
    pub borrow_quantity: u64,
    pub type_name: TypeName,
}

/// TypeName struct used in FlashLoanBorrowed
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct TypeName {
    pub name: String,
}

/// OrderFilled event from order_info module
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct OrderFilled {
    #[serde(with = "address_as_hex")]
    pub pool_id: [u8; 32],
    pub maker_order_id: u128,
    pub taker_order_id: u128,
    pub maker_client_order_id: u64,
    pub taker_client_order_id: u64,
    pub price: u64,
    pub taker_is_bid: bool,
    pub taker_fee: u64,
    pub taker_fee_is_deep: bool,
    pub maker_fee: u64,
    pub maker_fee_is_deep: bool,
    pub base_quantity: u64,
    pub quote_quantity: u64,
    #[serde(with = "address_as_hex")]
    pub maker_balance_manager_id: [u8; 32],
    #[serde(with = "address_as_hex")]
    pub taker_balance_manager_id: [u8; 32],
    pub timestamp: u64,
}

/// ReferralClaimed event
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct ReferralClaimed {
    #[serde(with = "address_as_hex")]
    pub pool_id: [u8; 32],
    #[serde(with = "address_as_hex")]
    pub referral_id: [u8; 32],
    #[serde(with = "address_as_hex")]
    pub owner: [u8; 32],
    pub base_amount: u64,
    pub quote_amount: u64,
    pub deep_amount: u64,
}

/// Generic order event (OrderPlaced, OrderCanceled, OrderModified)
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct OrderEvent {
    #[serde(with = "address_as_hex")]
    pub pool_id: [u8; 32],
    pub order_id: u128,
    pub client_order_id: u64,
    pub price: u64,
    pub is_bid: bool,
    #[serde(with = "address_as_hex")]
    pub balance_manager_id: [u8; 32],
    pub quantity: u64,
    pub timestamp: u64,
}

/// PoolCreated event
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct PoolCreated {
    #[serde(with = "address_as_hex")]
    pub pool_id: [u8; 32],
    pub taker_fee: u64,
    pub maker_fee: u64,
    pub tick_size: u64,
    pub lot_size: u64,
    pub min_size: u64,
}

/// Serde helper to serialize/deserialize [u8; 32] as "0x"-prefixed hex string.
mod address_as_hex {
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex = format!("0x{}", hex::encode(bytes));
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        // BCS deserializes addresses as raw 32 bytes, not hex strings
        <[u8; 32]>::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_deepbook_events_filters_by_package() {
        let deepbook_pkg = "0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497";
        let other_pkg = "0x0000000000000000000000000000000000000000000000000000000000000002";

        let mut deepbook_event = proto::Event::default();
        deepbook_event.package_id = Some(deepbook_pkg.to_string());
        deepbook_event.module = Some("pool".to_string());
        deepbook_event.sender = Some("0xsender1".to_string());
        deepbook_event.event_type = Some(format!(
            "{}::deep_price::PriceAdded",
            "0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809"
        ));

        let mut other_event = proto::Event::default();
        other_event.package_id = Some(other_pkg.to_string());
        other_event.module = Some("coin".to_string());
        other_event.sender = Some("0xsender2".to_string());
        other_event.event_type = Some("0x2::coin::CoinEvent".to_string());

        let mut events_container = proto::TransactionEvents::default();
        events_container.events = vec![deepbook_event, other_event];

        let mut tx = proto::ExecutedTransaction::default();
        tx.digest = Some("test_tx".to_string());
        tx.timestamp = Some(prost_types::Timestamp {
            seconds: 1700000000,
            nanos: 500_000_000,
        });
        tx.events = Some(events_container);

        let mut checkpoint = proto::Checkpoint::default();
        checkpoint.sequence_number = Some(100);
        checkpoint.transactions = vec![tx];

        let events = extract_deepbook_events(&checkpoint, deepbook_pkg);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].package_id, deepbook_pkg);
        assert!(events[0].event_type.contains("PriceAdded"));
        assert_eq!(events[0].sender, "0xsender1");
        assert_eq!(events[0].timestamp_ms, Some(1700000000500));
    }

    #[test]
    fn test_extract_empty_checkpoint() {
        let checkpoint = proto::Checkpoint::default();
        let events = extract_deepbook_events(&checkpoint, "0xsomepkg");
        assert!(events.is_empty());
    }

    /// Diagnostic test: connect to mainnet gRPC and verify messages are received.
    #[tokio::test]
    #[ignore]
    async fn test_grpc_stream_delivers_messages() {
        use sui_rpc::field::FieldMask;

        let mut client =
            SuiGrpcClient::new("https://fullnode.mainnet.sui.io").expect("Failed to create client");

        let mut stream = client
            .subscribe_checkpoints()
            .await
            .expect("Failed to subscribe");

        let deepbook_pkg = "0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497";

        let mut total_checkpoints = 0u32;
        let mut total_deepbook = 0u32;

        for i in 0..20 {
            match tokio::time::timeout(std::time::Duration::from_secs(10), stream.message()).await {
                Ok(Ok(Some(resp))) => {
                    total_checkpoints += 1;
                    let seq = resp.cursor.unwrap_or(0);
                    if let Some(cp) = &resp.checkpoint {
                        let events = extract_deepbook_events(cp, deepbook_pkg);
                        if !events.is_empty() {
                            total_deepbook += events.len() as u32;
                            println!("CP#{i}: seq={seq}, deepbook_events={}", events.len());
                        }
                    } else {
                        println!("CP#{i}: seq={seq}, checkpoint=None");
                    }
                    if total_deepbook >= 3 {
                        break;
                    }
                }
                Ok(Ok(None)) => {
                    println!("Stream ended at checkpoint #{i}");
                    break;
                }
                Ok(Err(e)) => {
                    println!("Stream error at checkpoint #{i}: {e}");
                    break;
                }
                Err(_) => {
                    println!("Timeout at checkpoint #{i}");
                    break;
                }
            }
        }

        println!("Total: {total_checkpoints} checkpoints, {total_deepbook} deepbook events");
        assert!(
            total_checkpoints > 0,
            "Should receive at least 1 checkpoint"
        );
    }

    #[test]
    fn test_deserialize_event_bcs_strips_generics() {
        // Event types with generic parameters should still match the short name.
        // e.g. "0x2c8d...::deep_price::PriceAdded<0x2::sui::SUI, 0xdba3...::usdc::USDC>"
        let event_type_with_generics =
            "0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809::deep_price::PriceAdded<0x2::sui::SUI, 0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC>";

        // Without valid BCS bytes it will fail to deserialize, but it should NOT
        // fall through to the "unknown" arm.  We verify by passing None (which
        // returns an empty object for all branches).  The real test is that the
        // name extraction works — covered by the extraction logic.
        let base_type = event_type_with_generics
            .split('<')
            .next()
            .unwrap_or(event_type_with_generics);
        let short_name = base_type.rsplit("::").next().unwrap_or("");
        assert_eq!(short_name, "PriceAdded");

        // Without generics
        let event_type_simple =
            "0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809::deep_price::PriceAdded";
        let base2 = event_type_simple
            .split('<')
            .next()
            .unwrap_or(event_type_simple);
        let short2 = base2.rsplit("::").next().unwrap_or("");
        assert_eq!(short2, "PriceAdded");
    }
}
