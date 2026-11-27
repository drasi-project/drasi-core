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

//! WebSocket implementation of [`ahp::Transport`] on the workspace's
//! `tokio-tungstenite` version (the upstream `ahp-ws` crate pins a newer one).

use ahp::{Transport, TransportError, TransportMessage};
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// AHP transport over a WebSocket connection.
pub struct WsTransport {
    inner: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsTransport {
    /// Connect to `url`, optionally sending `Authorization: Bearer <token>`.
    pub async fn connect(url: &str, bearer_token: Option<&str>) -> anyhow::Result<Self> {
        let mut request = url
            .into_client_request()
            .with_context(|| format!("invalid AHP endpoint '{url}'"))?;
        if let Some(token) = bearer_token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("token is not a valid header value")?;
            request.headers_mut().insert("Authorization", value);
        }
        let (inner, _response) = connect_async(request)
            .await
            .with_context(|| format!("failed connecting to AHP host at '{url}'"))?;
        Ok(Self { inner })
    }
}

impl Transport for WsTransport {
    async fn send(&mut self, msg: TransportMessage) -> Result<(), TransportError> {
        let frame = match msg {
            TransportMessage::Parsed(m) => Message::Text(
                serde_json::to_string(&m).map_err(|e| TransportError::Protocol(e.to_string()))?,
            ),
            TransportMessage::Text(s) => Message::Text(s),
            TransportMessage::Binary(b) => Message::Binary(b),
        };
        self.inner
            .send(frame)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<TransportMessage>, TransportError> {
        loop {
            match self.inner.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Err(e)) => return Err(TransportError::Io(e.to_string())),
                Some(Ok(Message::Text(s))) => return Ok(Some(TransportMessage::Text(s))),
                Some(Ok(Message::Binary(b))) => return Ok(Some(TransportMessage::Binary(b))),
                Some(Ok(_)) => continue,
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner
            .close(None)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }
}
