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

//! WebSocket streaming and REST polling for Hyperliquid.

use crate::config::HyperliquidSourceConfig;
use crate::mapping::{
    map_funding_rate_to_changes, map_liquidation_to_changes, map_mid_prices_to_changes,
    map_order_book_to_changes, map_trade_to_changes, InitializedEntities,
};
use crate::rest::HyperliquidRestClient;
use crate::types::{FundingSnapshot, L2Book, Liquidation, Trade, WsMessage};
use anyhow::{anyhow, Result};
use drasi_lib::channels::{ChangeDispatcher, SourceEvent, SourceEventWrapper};
use drasi_lib::profiling;
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;
use futures_util::{Sink, SinkExt, StreamExt};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::tungstenite::Message;

const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Clone, Default)]
pub struct StreamState {
    pub initialized: Arc<RwLock<InitializedEntities>>,
    pub trade_dedupe: Arc<RwLock<HashMap<String, u64>>>,
    pub funding_state: Arc<RwLock<HashMap<String, FundingSnapshot>>>,
}

pub struct WsStreamParams {
    pub source_id: String,
    pub ws_url: String,
    pub config: HyperliquidSourceConfig,
    pub coins: Vec<String>,
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub stream_state: StreamState,
    pub shutdown_rx: watch::Receiver<bool>,
    pub start_timestamp: Option<i64>,
}

pub struct FundingPollParams {
    pub source_id: String,
    pub rest_client: HyperliquidRestClient,
    pub config: HyperliquidSourceConfig,
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub stream_state: StreamState,
    pub shutdown_rx: watch::Receiver<bool>,
    pub start_timestamp: Option<i64>,
}

pub async fn load_trade_dedupe_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    coins: &[String],
) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Some(store) = state_store {
        for coin in coins {
            let key = format!("last_trade_tid:{coin}");
            if let Ok(Some(bytes)) = store.get(source_id, &key).await {
                if bytes.len() == 8 {
                    let tid = u64::from_le_bytes(
                        <[u8; 8]>::try_from(bytes.as_slice()).expect("len checked above"),
                    );
                    map.insert(coin.clone(), tid);
                }
            }
        }
    }
    map
}

pub async fn load_funding_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    coins: &[String],
) -> HashMap<String, FundingSnapshot> {
    let mut map = HashMap::new();
    if let Some(store) = state_store {
        for coin in coins {
            let key = format!("funding_state:{coin}");
            if let Ok(Some(bytes)) = store.get(source_id, &key).await {
                if let Ok(snapshot) = serde_json::from_slice::<FundingSnapshot>(&bytes) {
                    map.insert(coin.clone(), snapshot);
                }
            }
        }
    }
    map
}

pub async fn run_ws_stream(params: WsStreamParams) -> Result<()> {
    let WsStreamParams {
        source_id,
        ws_url,
        config,
        coins,
        dispatchers,
        state_store,
        stream_state,
        mut shutdown_rx,
        start_timestamp,
    } = params;
    let mut backoff = 1u64;
    let coin_filter: Option<HashSet<String>> = match &config.coins {
        crate::config::CoinSelection::Specific { coins } => Some(coins.iter().cloned().collect()),
        crate::config::CoinSelection::All => None,
    };

    loop {
        if *shutdown_rx.borrow() {
            info!("Hyperliquid WS stream received shutdown");
            break;
        }

        info!("Connecting to Hyperliquid WebSocket at {ws_url}");
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((ws_stream, _)) => {
                info!("Hyperliquid WebSocket connected");
                backoff = 1;

                let (mut write, mut read) = ws_stream.split();
                subscribe_to_channels(&mut write, &config, &coins).await?;

                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            info!("Hyperliquid WS shutdown signal received");
                            let _ = write.send(Message::Close(None)).await;
                            return Ok(());
                        }
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Err(e) = handle_ws_message(
                                        &source_id,
                                        &dispatchers,
                                        &state_store,
                                        &stream_state,
                                        &coin_filter,
                                        &text,
                                        start_timestamp,
                                    ).await {
                                        warn!("Failed to handle WS message: {e}");
                                    }
                                }
                                Some(Ok(Message::Ping(payload))) => {
                                    write.send(Message::Pong(payload)).await.ok();
                                }
                                Some(Ok(Message::Pong(_))) => {}
                                Some(Ok(Message::Close(_))) => {
                                    warn!("WebSocket closed by server");
                                    break;
                                }
                                Some(Ok(_)) => {}
                                Some(Err(e)) => {
                                    warn!("WebSocket error: {e}");
                                    break;
                                }
                                None => {
                                    warn!("WebSocket stream ended");
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("WebSocket connect failed: {e}");
            }
        }

        if *shutdown_rx.borrow() {
            break;
        }

        let wait = std::time::Duration::from_secs(backoff);
        warn!("Reconnecting in {backoff}s...");
        tokio::select! {
            _ = tokio::time::sleep(wait) => {},
            _ = shutdown_rx.changed() => {
                break;
            }
        }
        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF_SECS);
    }

    Ok(())
}

async fn subscribe_to_channels<W>(
    write: &mut W,
    config: &HyperliquidSourceConfig,
    coins: &[String],
) -> Result<()>
where
    W: Sink<Message> + Unpin,
{
    if config.enable_trades {
        for coin in coins {
            let msg = serde_json::json!({
                "method": "subscribe",
                "subscription": { "type": "trades", "coin": coin }
            });
            write
                .send(Message::Text(msg.to_string()))
                .await
                .map_err(|_| anyhow!("WebSocket subscribe failed"))?;
        }
    }

    if config.enable_order_book {
        for coin in coins {
            let msg = serde_json::json!({
                "method": "subscribe",
                "subscription": { "type": "l2Book", "coin": coin }
            });
            write
                .send(Message::Text(msg.to_string()))
                .await
                .map_err(|_| anyhow!("WebSocket subscribe failed"))?;
        }
    }

    if config.enable_mid_prices {
        let msg = serde_json::json!({
            "method": "subscribe",
            "subscription": { "type": "allMids" }
        });
        write
            .send(Message::Text(msg.to_string()))
            .await
            .map_err(|_| anyhow!("WebSocket subscribe failed"))?;
    }

    if config.enable_liquidations {
        let msg = serde_json::json!({
            "method": "subscribe",
            "subscription": { "type": "liquidations" }
        });
        write
            .send(Message::Text(msg.to_string()))
            .await
            .map_err(|_| anyhow!("WebSocket subscribe failed"))?;
    }

    Ok(())
}

async fn handle_ws_message(
    source_id: &str,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    stream_state: &StreamState,
    coin_filter: &Option<HashSet<String>>,
    text: &str,
    start_timestamp: Option<i64>,
) -> Result<()> {
    let msg: WsMessage = serde_json::from_str(text)?;

    match msg.channel.as_str() {
        "subscriptionResponse" => {
            debug!("Subscription confirmed: {text}");
        }
        "trades" => {
            let trades: Vec<Trade> = serde_json::from_value(msg.data)?;
            for trade in trades {
                if !passes_start_timestamp(start_timestamp, trade.time) {
                    continue;
                }

                if let Some(filter) = coin_filter {
                    if !filter.contains(&trade.coin) {
                        continue;
                    }
                }

                if !should_emit_trade(&trade, stream_state, state_store, source_id).await {
                    continue;
                }

                let changes = map_trade_to_changes(source_id, &trade)?;
                dispatch_changes(source_id, dispatchers, changes).await;
            }
        }
        "l2Book" => {
            let book: L2Book = serde_json::from_value(msg.data)?;
            if !passes_start_timestamp(start_timestamp, book.time) {
                return Ok(());
            }
            if let Some(filter) = coin_filter {
                if !filter.contains(&book.coin) {
                    return Ok(());
                }
            }

            let mut initialized = stream_state.initialized.write().await;
            let changes = map_order_book_to_changes(source_id, &book, &mut initialized)?;
            dispatch_changes(source_id, dispatchers, changes).await;
        }
        "allMids" => {
            let mids_value = msg
                .data
                .get("mids")
                .cloned()
                .unwrap_or_else(|| msg.data.clone());
            let mids: HashMap<String, String> = serde_json::from_value(mids_value)?;
            let timestamp = chrono::Utc::now().timestamp_millis();
            if !passes_start_timestamp(start_timestamp, timestamp) {
                return Ok(());
            }
            let filtered = filter_mids(mids, coin_filter);
            let mut initialized = stream_state.initialized.write().await;
            let changes =
                map_mid_prices_to_changes(source_id, &filtered, &mut initialized, timestamp)?;
            dispatch_changes(source_id, dispatchers, changes).await;
        }
        "liquidations" => {
            let liquidations: Vec<Liquidation> = serde_json::from_value(msg.data)?;
            for liquidation in liquidations {
                if !passes_start_timestamp(start_timestamp, liquidation.time) {
                    continue;
                }
                if let Some(filter) = coin_filter {
                    if !filter.contains(&liquidation.coin) {
                        continue;
                    }
                }
                let changes = map_liquidation_to_changes(source_id, &liquidation)?;
                dispatch_changes(source_id, dispatchers, changes).await;
            }
        }
        _ => {
            debug!("Ignoring channel {}", msg.channel);
        }
    }

    Ok(())
}

fn filter_mids(
    mids: HashMap<String, String>,
    coin_filter: &Option<HashSet<String>>,
) -> HashMap<String, String> {
    if let Some(filter) = coin_filter {
        mids.into_iter()
            .filter(|(coin, _)| filter.contains(coin))
            .collect()
    } else {
        mids
    }
}

fn passes_start_timestamp(start_timestamp: Option<i64>, event_time: i64) -> bool {
    match start_timestamp {
        Some(start) => event_time >= start,
        None => true,
    }
}

async fn should_emit_trade(
    trade: &Trade,
    stream_state: &StreamState,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    source_id: &str,
) -> bool {
    let mut trade_state = stream_state.trade_dedupe.write().await;
    let last_tid = trade_state.get(&trade.coin).copied().unwrap_or_else(|| {
        debug!(
            "No prior trade dedup state for coin '{}' — accepting all trades",
            trade.coin
        );
        0
    });
    if trade.tid <= last_tid {
        return false;
    }

    trade_state.insert(trade.coin.clone(), trade.tid);

    if let Some(store) = state_store {
        let key = format!("last_trade_tid:{}", trade.coin);
        if let Err(e) = store
            .set(source_id, &key, trade.tid.to_le_bytes().to_vec())
            .await
        {
            warn!("Failed to persist trade tid: {e}");
        }
    }

    true
}

async fn dispatch_changes(
    source_id: &str,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    changes: Vec<drasi_core::models::SourceChange>,
) {
    for change in changes {
        let mut profiling = profiling::ProfilingMetadata::new();
        profiling.source_send_ns = Some(profiling::timestamp_ns());

        let wrapper = SourceEventWrapper::with_profiling(
            source_id.to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
            profiling,
        );

        if let Err(e) =
            SourceBase::dispatch_from_task(dispatchers.clone(), wrapper, source_id).await
        {
            debug!("[{source_id}] Dispatch failed (no subscribers): {e}");
        }
    }
}

pub async fn run_funding_poll(params: FundingPollParams) -> Result<()> {
    let FundingPollParams {
        source_id,
        rest_client,
        config,
        dispatchers,
        state_store,
        stream_state,
        mut shutdown_rx,
        start_timestamp,
    } = params;
    let interval_secs = config.funding_poll_interval_secs;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
    let coin_filter: Option<HashSet<String>> = match &config.coins {
        crate::config::CoinSelection::Specific { coins } => Some(coins.iter().cloned().collect()),
        crate::config::CoinSelection::All => None,
    };

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                info!("Funding poll shutdown signal received");
                break;
            }
            _ = interval.tick() => {
                let timestamp = chrono::Utc::now().timestamp_millis();
                if !passes_start_timestamp(start_timestamp, timestamp) {
                    continue;
                }
                match rest_client.fetch_meta_and_asset_ctxs().await {
                    Ok((meta, ctxs)) => {
                        let mut initialized = stream_state.initialized.write().await;
                        let mut funding_state = stream_state.funding_state.write().await;

                        for (asset, ctx) in meta.universe.iter().zip(ctxs.iter()) {
                            if let Some(filter) = &coin_filter {
                                if !filter.contains(&asset.name) {
                                    continue;
                                }
                            }

                            let (changes, snapshot) = map_funding_rate_to_changes(
                                &source_id,
                                &asset.name,
                                ctx,
                                &mut initialized,
                                timestamp,
                            )?;

                            if let Some(previous) = funding_state.get(&asset.name) {
                                if previous == &snapshot {
                                    continue;
                                }
                            }

                            funding_state.insert(asset.name.clone(), snapshot.clone());
                            persist_funding_snapshot(&state_store, &source_id, &asset.name, &snapshot).await;
                            dispatch_changes(&source_id, &dispatchers, changes).await;
                        }
                    }
                    Err(e) => {
                        warn!("Funding poll failed: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn persist_funding_snapshot(
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    source_id: &str,
    coin: &str,
    snapshot: &FundingSnapshot,
) {
    if let Some(store) = state_store {
        let key = format!("funding_state:{coin}");
        match serde_json::to_vec(snapshot) {
            Ok(bytes) => {
                if let Err(e) = store.set(source_id, &key, bytes).await {
                    warn!("Failed to persist funding snapshot: {e}");
                }
            }
            Err(e) => warn!("Failed to serialize funding snapshot: {e}"),
        }
    }
}
