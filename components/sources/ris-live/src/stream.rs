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

//! WebSocket stream processing for RIS Live.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::sync::{watch, RwLock};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite};
use url::Url;

use drasi_lib::channels::{ChangeDispatcher, SourceEvent, SourceEventWrapper};
use drasi_lib::profiling::{timestamp_ns, ProfilingMetadata};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::state_store::StateStoreProvider;

use crate::config::RisLiveSourceConfig;
use crate::mapping::{GraphMapper, PersistedStreamState, StreamState};
use crate::messages::{
    message_timestamp_millis, RisErrorData, RisIncomingMessage, RisMessageData, RisSubscribeMessage,
};

const STATE_KEY: &str = "ris-live.stream-state.v1";

/// Minimum interval between state persistence writes.
const PERSIST_INTERVAL: Duration = Duration::from_secs(5);

/// Runs the resilient streaming loop with reconnect behavior.
pub async fn run_stream_loop(
    source_id: String,
    config: RisLiveSourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let initial_state = load_initial_state(&source_id, &config, &state_store).await?;
    let mut mapper = GraphMapper::new(source_id.clone(), initial_state);
    let mut last_persisted = Instant::now();

    loop {
        if *shutdown_rx.borrow() {
            persist_state(&source_id, &state_store, mapper.state()).await?;
            info!("[{source_id}] RIS stream shutdown requested");
            return Ok(());
        }

        match run_single_connection(
            &source_id,
            &config,
            &dispatchers,
            &state_store,
            &mut mapper,
            &mut last_persisted,
            &mut shutdown_rx,
        )
        .await
        {
            Ok(()) => {
                if *shutdown_rx.borrow() {
                    persist_state(&source_id, &state_store, mapper.state()).await?;
                    return Ok(());
                }
                warn!("[{source_id}] RIS connection ended, reconnecting");
            }
            Err(error) => {
                error!("[{source_id}] RIS streaming error: {error}");
            }
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(config.reconnect_delay_secs())) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    persist_state(&source_id, &state_store, mapper.state()).await?;
                    return Ok(());
                }
            }
        }
    }
}

async fn run_single_connection(
    source_id: &str,
    config: &RisLiveSourceConfig,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    mapper: &mut GraphMapper,
    last_persisted: &mut Instant,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    ensure_crypto_provider();
    let url = build_url(config)?;
    info!("[{source_id}] Connecting to RIS Live: {url}");

    let (mut socket, response) = connect_async(url.as_str())
        .await
        .with_context(|| format!("failed to connect to RIS Live at {url}"))?;
    debug!(
        "[{source_id}] Connected to RIS Live (status: {})",
        response.status()
    );

    let subscribe = RisSubscribeMessage::from_config(config);
    let payload =
        serde_json::to_string(&subscribe).context("failed to serialize subscribe payload")?;
    socket
        .send(Message::Text(payload))
        .await
        .context("failed to send ris_subscribe")?;
    info!("[{source_id}] Subscription message sent");

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
            }
            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        process_text_frame(source_id, config, dispatchers, state_store, mapper, last_persisted, &text).await?;
                    }
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Ok(Message::Ping(payload))) => {
                        socket.send(Message::Pong(payload)).await.context("failed to send pong")?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) => {
                        info!("[{source_id}] RIS server closed the connection");
                        return Ok(());
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(tungstenite::Error::ConnectionClosed)) => {
                        info!("[{source_id}] RIS connection closed");
                        return Ok(());
                    }
                    Some(Err(error)) => {
                        return Err(anyhow!("websocket read error: {error}"));
                    }
                    None => {
                        info!("[{source_id}] RIS stream ended");
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn process_text_frame(
    source_id: &str,
    config: &RisLiveSourceConfig,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    mapper: &mut GraphMapper,
    last_persisted: &mut Instant,
    text: &str,
) -> Result<()> {
    let incoming: RisIncomingMessage = serde_json::from_str(text)
        .with_context(|| format!("failed to parse RIS message wrapper: {text}"))?;

    match incoming.msg_type.as_str() {
        "ris_subscribe_ok" => {
            info!("[{source_id}] Subscription acknowledged");
            Ok(())
        }
        "ris_error" => {
            if let Some(payload) = incoming.data {
                let err: RisErrorData =
                    serde_json::from_value(payload).context("failed to parse ris_error payload")?;
                Err(anyhow!("RIS server error: {}", err.message))
            } else {
                Err(anyhow!("RIS server sent ris_error without payload"))
            }
        }
        "ris_message" => {
            let payload = incoming
                .data
                .ok_or_else(|| anyhow!("ris_message missing payload"))?;
            let message: RisMessageData =
                serde_json::from_value(payload).context("failed to parse ris_message payload")?;

            if !config.should_process_timestamp(message_timestamp_millis(&message)) {
                debug!("[{source_id}] Skipping message due to start_from timestamp");
                return Ok(());
            }

            let mut changes = Vec::new();
            match message.msg_type.as_deref() {
                Some("UPDATE") => {
                    changes.extend(mapper.process_announcements(&message));
                    changes.extend(mapper.process_withdrawals(&message));
                }
                Some("RIS_PEER_STATE") if config.include_peer_state => {
                    changes.extend(mapper.process_peer_state(&message));
                }
                Some("RIS_PEER_STATE") => {}
                Some("OPEN") | Some("KEEPALIVE") | Some("NOTIFICATION") => {}
                _ => {}
            }

            if !changes.is_empty() {
                for change in changes {
                    dispatch_change(source_id, dispatchers, change).await?;
                }
                if last_persisted.elapsed() >= PERSIST_INTERVAL {
                    persist_state(source_id, state_store, mapper.state()).await?;
                    *last_persisted = Instant::now();
                }
            }
            Ok(())
        }
        "pong" => Ok(()),
        "ris_rrc_list" => Ok(()),
        other => {
            debug!("[{source_id}] Ignoring unsupported message type: {other}");
            Ok(())
        }
    }
}

async fn dispatch_change(
    source_id: &str,
    dispatchers: &Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    change: drasi_core::models::SourceChange,
) -> Result<()> {
    let mut profiling = ProfilingMetadata::new();
    profiling.source_send_ns = Some(timestamp_ns());

    let wrapper = SourceEventWrapper::with_profiling(
        source_id.to_string(),
        SourceEvent::Change(change),
        chrono::Utc::now(),
        profiling,
    );

    SourceBase::dispatch_from_task(dispatchers.clone(), wrapper, source_id).await
}

fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn build_url(config: &RisLiveSourceConfig) -> Result<Url> {
    let mut url = Url::parse(&config.websocket_url)
        .with_context(|| format!("invalid websocket_url '{}'", config.websocket_url))?;

    if let Some(client_name) = &config.client_name {
        // Remove any existing `client` parameter before appending ours
        let existing: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "client")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut().clear().extend_pairs(existing);
        url.query_pairs_mut().append_pair("client", client_name);
    }

    Ok(url)
}

async fn load_initial_state(
    source_id: &str,
    config: &RisLiveSourceConfig,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<StreamState> {
    if config.clear_state_on_start {
        if let Some(store) = state_store {
            store.delete(source_id, STATE_KEY).await.with_context(|| {
                format!("failed to clear persisted state for source '{source_id}'")
            })?;
        }
        return Ok(StreamState::default());
    }

    load_state(source_id, state_store).await
}

async fn load_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<StreamState> {
    let Some(store) = state_store else {
        return Ok(StreamState::default());
    };

    let bytes = store
        .get(source_id, STATE_KEY)
        .await
        .with_context(|| format!("failed to read state for source '{source_id}'"))?;

    let Some(bytes) = bytes else {
        return Ok(StreamState::default());
    };

    let persisted: PersistedStreamState = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid persisted state payload for source '{source_id}'"))?;

    Ok(persisted.into())
}

async fn persist_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    state: &StreamState,
) -> Result<()> {
    let Some(store) = state_store else {
        return Ok(());
    };

    let payload =
        serde_json::to_vec(&PersistedStreamState::from(state)).context("failed to encode state")?;
    store
        .set(source_id, STATE_KEY, payload)
        .await
        .with_context(|| format!("failed to persist state for source '{source_id}'"))?;
    Ok(())
}
