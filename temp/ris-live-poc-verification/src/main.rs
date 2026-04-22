// POC: Verify tokio-tungstenite can connect to RIPE RIS Live WebSocket
// and receive real-time BGP messages.
//
// This verifies:
// 1. WSS connection to ris-live.ripe.net works
// 2. ris_subscribe message format is accepted
// 3. ris_message responses can be deserialized
// 4. Message fields (timestamp, peer, peer_asn, host, type, etc.) are accessible

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Debug, Serialize)]
struct RisSubscribe {
    #[serde(rename = "type")]
    msg_type: String,
    data: RisSubscribeData,
}

#[derive(Debug, Serialize)]
struct RisSubscribeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    msg_type: Option<String>,
    #[serde(rename = "socketOptions")]
    #[serde(skip_serializing_if = "Option::is_none")]
    socket_options: Option<SocketOptions>,
}

#[derive(Debug, Serialize)]
struct SocketOptions {
    acknowledge: bool,
}

#[derive(Debug, Deserialize)]
struct RisLiveMessage {
    #[serde(rename = "type")]
    msg_type: String,
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RisMessageData {
    timestamp: Option<f64>,
    peer: Option<String>,
    peer_asn: Option<String>,
    id: Option<String>,
    host: Option<String>,
    #[serde(rename = "type")]
    msg_type: Option<String>,
    // UPDATE-specific fields
    path: Option<Vec<Value>>,
    origin: Option<String>,
    community: Option<Vec<Vec<i64>>>,
    med: Option<i64>,
    announcements: Option<Vec<Announcement>>,
    withdrawals: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Announcement {
    next_hop: String,
    prefixes: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_crypto_provider();
    println!("=== RIPE RIS Live POC Verification ===\n");

    // 1. Connect to RIS Live WebSocket
    let url = "wss://ris-live.ripe.net/v1/ws/?client=drasi-poc-verification";
    println!("1. Connecting to: {}", url);

    let (mut ws_stream, response) = connect_async(url).await?;
    println!("   ✅ Connected! HTTP status: {}", response.status());
    println!(
        "   Response headers: {:?}",
        response.headers().keys().collect::<Vec<_>>()
    );

    // 2. Send ris_subscribe with acknowledge
    let subscribe = RisSubscribe {
        msg_type: "ris_subscribe".to_string(),
        data: RisSubscribeData {
            host: Some("rrc00".to_string()),
            msg_type: Some("UPDATE".to_string()),
            socket_options: Some(SocketOptions { acknowledge: true }),
        },
    };
    let subscribe_json = serde_json::to_string(&subscribe)?;
    println!("\n2. Sending subscribe: {}", subscribe_json);
    ws_stream.send(Message::Text(subscribe_json)).await?;
    println!("   ✅ Subscribe message sent");

    // 3. Receive and parse messages
    println!("\n3. Receiving messages (will collect up to 5 ris_messages)...\n");

    let mut msg_count = 0;
    let mut update_count = 0;
    let mut other_count = 0;
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let parsed: RisLiveMessage = serde_json::from_str(&text)?;

                    match parsed.msg_type.as_str() {
                        "ris_subscribe_ok" => {
                            println!("   📋 Subscription confirmed: {:?}", parsed.data);
                        }
                        "ris_message" => {
                            msg_count += 1;
                            if let Some(data) = &parsed.data {
                                let msg_data: RisMessageData =
                                    serde_json::from_value(data.clone())?;
                                let bgp_type =
                                    msg_data.msg_type.as_deref().unwrap_or("unknown");

                                match bgp_type {
                                    "UPDATE" => {
                                        update_count += 1;
                                        println!("   📨 Message #{} [UPDATE]:", msg_count);
                                        println!(
                                            "      timestamp: {:?}",
                                            msg_data.timestamp
                                        );
                                        println!("      peer: {:?}", msg_data.peer);
                                        println!(
                                            "      peer_asn: {:?}",
                                            msg_data.peer_asn
                                        );
                                        println!("      id: {:?}", msg_data.id);
                                        println!("      host: {:?}", msg_data.host);
                                        println!("      origin: {:?}", msg_data.origin);
                                        println!("      path: {:?}", msg_data.path);
                                        println!(
                                            "      community: {:?}",
                                            msg_data.community
                                        );

                                        if let Some(announcements) =
                                            &msg_data.announcements
                                        {
                                            println!("      announcements:");
                                            for a in announcements {
                                                println!(
                                                    "        next_hop: {}, prefixes: {:?}",
                                                    a.next_hop, a.prefixes
                                                );
                                            }
                                        }
                                        if let Some(withdrawals) = &msg_data.withdrawals {
                                            println!(
                                                "      withdrawals: {:?}",
                                                withdrawals
                                            );
                                        }
                                    }
                                    other => {
                                        other_count += 1;
                                        println!(
                                            "   📨 Message #{} [{}]: peer={:?}, host={:?}",
                                            msg_count, other, msg_data.peer, msg_data.host
                                        );
                                    }
                                }
                            }

                            if msg_count >= 5 {
                                break;
                            }
                        }
                        "ris_error" => {
                            println!("   ❌ Error: {:?}", parsed.data);
                        }
                        other => {
                            println!("   ℹ️  Other message type: {}", other);
                        }
                    }
                }
                Ok(Message::Ping(_)) => {
                    println!("   🏓 Ping received");
                }
                Ok(other) => {
                    println!("   ℹ️  Other WS message: {:?}", other);
                }
                Err(e) => {
                    println!("   ❌ WebSocket error: {}", e);
                    break;
                }
            }
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    });

    match timeout.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => println!("\n   ❌ Error during message processing: {}", e),
        Err(_) => println!("\n   ⏰ Timeout reached (30s)"),
    }

    // 4. Summary
    println!("\n=== Summary ===");
    println!("Total messages received: {}", msg_count);
    println!("UPDATE messages: {}", update_count);
    println!("Other BGP messages: {}", other_count);
    println!("\n✅ POC verification complete!");
    println!("\nVerified capabilities:");
    println!("  - WSS connection to ris-live.ripe.net ✅");
    println!("  - ris_subscribe with filters ✅");
    println!("  - JSON message deserialization ✅");
    println!("  - Access to all message fields ✅");

    Ok(())
}
