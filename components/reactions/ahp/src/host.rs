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

//! Thin, reconnecting wrapper over [`ahp::Client`] exposing the handful of
//! AHP operations the reaction performs.

use std::time::Duration;

use ahp::ahp_types::actions::AutomationRunCancelRequestedAction;
use ahp::ahp_types::actions::{
    ChatPendingMessageSetAction, ChatTurnCancelledAction, ChatTurnStartedAction,
    SessionIsArchivedChangedAction, SessionTitleChangedAction, StateAction,
};
use ahp::ahp_types::commands::{
    AuthenticateParams, CreateChatParams, CreateSessionParams, DisposeSessionParams,
    RunAutomationParams, RunAutomationResult,
};
use ahp::ahp_types::common::JsonObject;
use ahp::ahp_types::errors::{ahp_error_codes, json_rpc_error_codes};
use ahp::ahp_types::state::{Message, MessageKind, MessageOrigin, PendingMessageKind};
use ahp::ahp_types::{ROOT_RESOURCE_URI, SUPPORTED_PROTOCOL_VERSIONS};
use ahp::{Client, ClientConfig, ClientError, SessionSubscription, SubscriptionEvent};
use anyhow::{anyhow, bail, Context};
use log::{debug, warn};
use serde_json::{json, Value};

use crate::transport::WsTransport;

/// Channel that hosts the automation catalogue and `runAutomation`.
pub const AUTOMATIONS_CHANNEL: &str = "ahp-automations://";

/// Connection and session-creation settings resolved from config.
#[derive(Debug, Clone)]
pub(crate) struct HostSettings {
    pub endpoint: String,
    pub token: Option<String>,
    pub auth_resource: Option<String>,
    pub client_id: String,
    pub timeout: Duration,
    pub ready_timeout: Duration,
    pub provider: Option<String>,
    pub working_directories: Vec<String>,
    pub session_config: Option<JsonObject>,
}

/// Outgoing message content for a turn or a follow-up.
#[derive(Debug, Clone)]
pub(crate) struct OutboundMessage {
    pub text: String,
    pub meta: JsonObject,
}

impl OutboundMessage {
    fn into_message(self) -> Message {
        Message {
            text: self.text,
            origin: MessageOrigin {
                kind: MessageKind::User,
                extra: Default::default(),
            },
            attachments: None,
            model: None,
            agent: None,
            meta: Some(self.meta),
            extra: Default::default(),
        }
    }
}

/// Lazily connected AHP host client. Any failed operation drops the
/// connection so the next operation reconnects.
pub(crate) struct AhpHost {
    settings: HostSettings,
    client: Option<Client>,
}

impl AhpHost {
    pub fn new(settings: HostSettings) -> Self {
        Self {
            settings,
            client: None,
        }
    }

    /// Drop the current connection (if any).
    pub async fn disconnect(&mut self) {
        if let Some(client) = self.client.take() {
            client.shutdown().await;
        }
    }

    async fn client(&mut self) -> anyhow::Result<Client> {
        if let Some(client) = &self.client {
            return Ok(client.clone());
        }
        let transport =
            WsTransport::connect(&self.settings.endpoint, self.settings.token.as_deref()).await?;
        let client = Client::connect(
            transport,
            ClientConfig {
                default_request_timeout: Some(self.settings.timeout),
                ..ClientConfig::default()
            },
        )
        .await
        .context("failed starting AHP client")?;
        client
            .initialize(
                self.settings.client_id.clone(),
                SUPPORTED_PROTOCOL_VERSIONS
                    .iter()
                    .map(|v| v.to_string())
                    .collect(),
                Vec::new(),
            )
            .await
            .map_err(rpc_error)
            .context("AHP initialize failed")?;
        authenticate(&client, &self.settings).await?;
        self.client = Some(client.clone());
        Ok(client)
    }

    async fn guarded<T>(&mut self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        if result.is_err() {
            self.disconnect().await;
        }
        result
    }

    /// Create `session_uri` (idempotently), wait for it to become ready,
    /// optionally set its title, and return the chat to send turns to.
    pub async fn create_session(
        &mut self,
        session_uri: &str,
        title: Option<String>,
    ) -> anyhow::Result<String> {
        let result = self.create_session_inner(session_uri, title).await;
        self.guarded(result).await
    }

    async fn create_session_inner(
        &mut self,
        session_uri: &str,
        title: Option<String>,
    ) -> anyhow::Result<String> {
        let client = self.client().await?;
        let params = CreateSessionParams {
            channel: session_uri.to_string(),
            meta: None,
            provider: self.settings.provider.clone(),
            working_directories: (!self.settings.working_directories.is_empty())
                .then(|| self.settings.working_directories.clone()),
            config: self.settings.session_config.clone(),
            active_client: None,
            progress_token: None,
        };
        match call::<_, Value>(&client, &self.settings, "createSession", params).await {
            Ok(_) => {}
            Err(ClientError::Rpc(e)) if e.code == ahp_error_codes::SESSION_ALREADY_EXISTS => {
                debug!("AHP session '{session_uri}' already exists; reusing it");
            }
            Err(e) => return Err(rpc_error(e)).context("AHP createSession failed"),
        }

        let (mut sub, snapshot) = subscribe(&client, session_uri)
            .await?
            .ok_or_else(|| anyhow!("AHP session '{session_uri}' was not found after creation"))?;
        let mut lifecycle = snapshot
            .pointer("/state/lifecycle")
            .and_then(Value::as_str)
            .unwrap_or("ready")
            .to_string();
        let mut default_chat = default_chat_from_state(snapshot.get("state"));

        let deadline = tokio::time::Instant::now() + self.settings.ready_timeout;
        while lifecycle != "ready" {
            if lifecycle == "failed" {
                let _ = client.unsubscribe(session_uri.to_string()).await;
                bail!("AHP session '{session_uri}' failed to initialize");
            }
            let event = tokio::time::timeout_at(deadline, sub.recv())
                .await
                .map_err(|_| anyhow!("timed out waiting for AHP session '{session_uri}'"))?
                .ok_or_else(|| anyhow!("AHP connection closed while creating session"))?;
            if let SubscriptionEvent::Action(env) = event {
                match env.action {
                    StateAction::SessionReady(_) => lifecycle = "ready".to_string(),
                    StateAction::SessionCreationFailed(failed) => {
                        let _ = client.unsubscribe(session_uri.to_string()).await;
                        bail!(
                            "AHP session '{session_uri}' failed to initialize: {}",
                            failed.error.message
                        );
                    }
                    StateAction::SessionDefaultChatChanged(a) if a.default_chat.is_some() => {
                        default_chat = a.default_chat;
                    }
                    StateAction::SessionChatAdded(a) if default_chat.is_none() => {
                        default_chat = Some(a.summary.resource);
                    }
                    _ => {}
                }
            }
        }

        if default_chat.is_none() {
            if let Some((_, snapshot)) = subscribe(&client, session_uri).await? {
                default_chat = default_chat_from_state(snapshot.get("state"));
            }
        }
        let chat = match default_chat {
            Some(chat) => chat,
            None => {
                let chat = format!(
                    "ahp-chat:/{}",
                    session_uri.trim_start_matches("ahp-session:/")
                );
                let params = CreateChatParams {
                    channel: session_uri.to_string(),
                    meta: None,
                    chat: chat.clone(),
                    initial_message: None,
                    source: None,
                    working_directories: None,
                    title: None,
                    provider: None,
                };
                call::<_, Value>(&client, &self.settings, "createChat", params)
                    .await
                    .map_err(rpc_error)
                    .context("AHP createChat failed")?;
                chat
            }
        };

        if let Some(title) = title.filter(|t| !t.trim().is_empty()) {
            let action = StateAction::SessionTitleChanged(SessionTitleChangedAction { title });
            if let Some(reason) = dispatch_acked(
                &client,
                &self.settings.client_id,
                &mut sub,
                session_uri,
                action,
                self.settings.timeout,
            )
            .await?
            {
                warn!("AHP host rejected session title for '{session_uri}': {reason}");
            }
        }
        let _ = client.unsubscribe(session_uri.to_string()).await;
        Ok(chat)
    }

    /// Start turn `turn_id` on `chat_uri` unless that turn already exists.
    pub async fn start_turn(
        &mut self,
        chat_uri: &str,
        turn_id: &str,
        message: OutboundMessage,
    ) -> anyhow::Result<()> {
        let result = self.start_turn_inner(chat_uri, turn_id, message).await;
        self.guarded(result).await
    }

    async fn start_turn_inner(
        &mut self,
        chat_uri: &str,
        turn_id: &str,
        message: OutboundMessage,
    ) -> anyhow::Result<()> {
        let client = self.client().await?;
        let (mut sub, snapshot) = subscribe(&client, chat_uri)
            .await?
            .ok_or_else(|| anyhow!("AHP chat '{chat_uri}' was not found"))?;
        if chat_has_turn(snapshot.get("state"), turn_id) {
            debug!("AHP turn '{turn_id}' already started on '{chat_uri}'");
            let _ = client.unsubscribe(chat_uri.to_string()).await;
            return Ok(());
        }
        let action = StateAction::ChatTurnStarted(ChatTurnStartedAction {
            turn_id: turn_id.to_string(),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            message: message.into_message(),
            queued_message_id: None,
            meta: None,
        });
        let rejection = dispatch_acked(
            &client,
            &self.settings.client_id,
            &mut sub,
            chat_uri,
            action,
            self.settings.timeout,
        )
        .await;
        let _ = client.unsubscribe(chat_uri.to_string()).await;
        if let Some(reason) = rejection? {
            bail!("AHP host rejected chat/turnStarted on '{chat_uri}': {reason}");
        }
        Ok(())
    }

    /// Upsert pending message `id` on `chat_uri`. Returns `false` when the
    /// chat no longer exists.
    pub async fn set_pending_message(
        &mut self,
        chat_uri: &str,
        id: &str,
        kind: PendingMessageKind,
        message: OutboundMessage,
    ) -> anyhow::Result<bool> {
        let result = self
            .set_pending_message_inner(chat_uri, id, kind, message)
            .await;
        self.guarded(result).await
    }

    async fn set_pending_message_inner(
        &mut self,
        chat_uri: &str,
        id: &str,
        kind: PendingMessageKind,
        message: OutboundMessage,
    ) -> anyhow::Result<bool> {
        let client = self.client().await?;
        let Some((mut sub, _)) = subscribe(&client, chat_uri).await? else {
            return Ok(false);
        };
        let action = StateAction::ChatPendingMessageSet(ChatPendingMessageSetAction {
            kind,
            id: id.to_string(),
            message: message.into_message(),
        });
        let rejection = dispatch_acked(
            &client,
            &self.settings.client_id,
            &mut sub,
            chat_uri,
            action,
            self.settings.timeout,
        )
        .await;
        let _ = client.unsubscribe(chat_uri.to_string()).await;
        if let Some(reason) = rejection? {
            bail!("AHP host rejected chat/pendingMessageSet on '{chat_uri}': {reason}");
        }
        Ok(true)
    }

    /// Cancel the active turn on `chat_uri`, if any.
    pub async fn cancel_active_turn(&mut self, chat_uri: &str) -> anyhow::Result<()> {
        let result = self.cancel_active_turn_inner(chat_uri).await;
        self.guarded(result).await
    }

    async fn cancel_active_turn_inner(&mut self, chat_uri: &str) -> anyhow::Result<()> {
        let client = self.client().await?;
        let Some((mut sub, snapshot)) = subscribe(&client, chat_uri).await? else {
            return Ok(());
        };
        let active_turn = snapshot
            .pointer("/state/activeTurn/id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let result = match active_turn {
            Some(turn_id) => {
                let action = StateAction::ChatTurnCancelled(ChatTurnCancelledAction {
                    turn_id,
                    duration: 0,
                    meta: None,
                });
                dispatch_acked(
                    &client,
                    &self.settings.client_id,
                    &mut sub,
                    chat_uri,
                    action,
                    self.settings.timeout,
                )
                .await
                .map(|rejection| {
                    if let Some(reason) = rejection {
                        debug!("AHP host rejected chat/turnCancelled on '{chat_uri}': {reason}");
                    }
                })
            }
            None => Ok(()),
        };
        let _ = client.unsubscribe(chat_uri.to_string()).await;
        result
    }

    /// Archive `session_uri` (no-op when it no longer exists).
    pub async fn archive_session(&mut self, session_uri: &str) -> anyhow::Result<()> {
        let result = self.archive_session_inner(session_uri).await;
        self.guarded(result).await
    }

    async fn archive_session_inner(&mut self, session_uri: &str) -> anyhow::Result<()> {
        let client = self.client().await?;
        let Some((mut sub, _)) = subscribe(&client, session_uri).await? else {
            return Ok(());
        };
        let action = StateAction::SessionIsArchivedChanged(SessionIsArchivedChangedAction {
            is_archived: true,
        });
        let rejection = dispatch_acked(
            &client,
            &self.settings.client_id,
            &mut sub,
            session_uri,
            action,
            self.settings.timeout,
        )
        .await;
        let _ = client.unsubscribe(session_uri.to_string()).await;
        if let Some(reason) = rejection? {
            bail!("AHP host rejected session/isArchivedChanged on '{session_uri}': {reason}");
        }
        Ok(())
    }

    /// Dispose `session_uri` (no-op when it no longer exists).
    pub async fn dispose_session(&mut self, session_uri: &str) -> anyhow::Result<()> {
        let result = async {
            let client = self.client().await?;
            let params = DisposeSessionParams {
                channel: session_uri.to_string(),
                meta: None,
            };
            match call::<_, Value>(&client, &self.settings, "disposeSession", params).await {
                Ok(_) => Ok(()),
                Err(ClientError::Rpc(e)) if is_not_found(e.code) => Ok(()),
                Err(e) => Err(rpc_error(e)).context("AHP disposeSession failed"),
            }
        }
        .await;
        self.guarded(result).await
    }

    /// Run `automation` with an idempotent `request_id`; returns the run URI.
    pub async fn run_automation(
        &mut self,
        automation: &str,
        request_id: &str,
        meta: JsonObject,
    ) -> anyhow::Result<String> {
        let result = async {
            let client = self.client().await?;
            let params = RunAutomationParams {
                channel: AUTOMATIONS_CHANNEL.to_string(),
                meta: Some(meta),
                automation: automation.to_string(),
                request_id: request_id.to_string(),
            };
            let result: RunAutomationResult =
                call(&client, &self.settings, "runAutomation", params)
                    .await
                    .map_err(rpc_error)
                    .context("AHP runAutomation failed")?;
            Ok(result.resource)
        }
        .await;
        self.guarded(result).await
    }

    /// Request cancellation of `run_uri` (no-op when it no longer exists).
    pub async fn cancel_run(&mut self, run_uri: &str) -> anyhow::Result<()> {
        let result = async {
            let client = self.client().await?;
            let Some((mut sub, _)) = subscribe(&client, run_uri).await? else {
                return Ok(());
            };
            let action =
                StateAction::AutomationRunCancelRequested(AutomationRunCancelRequestedAction {});
            let rejection = dispatch_acked(
                &client,
                &self.settings.client_id,
                &mut sub,
                run_uri,
                action,
                self.settings.timeout,
            )
            .await;
            let _ = client.unsubscribe(run_uri.to_string()).await;
            if let Some(reason) = rejection? {
                debug!("AHP host rejected automationRun/cancelRequested on '{run_uri}': {reason}");
            }
            Ok(())
        }
        .await;
        self.guarded(result).await
    }
}

async fn authenticate(client: &Client, settings: &HostSettings) -> anyhow::Result<()> {
    let (Some(resource), Some(token)) = (&settings.auth_resource, &settings.token) else {
        return Ok(());
    };
    let params = AuthenticateParams {
        channel: ROOT_RESOURCE_URI.to_string(),
        meta: None,
        resource: resource.clone(),
        token: token.clone(),
        expires_in: None,
        scopes: None,
        target: None,
        account_id: None,
    };
    client
        .request::<_, Value>("authenticate", params)
        .await
        .map_err(rpc_error)
        .context("AHP authenticate failed")?;
    Ok(())
}

/// Send a request; on `AuthRequired` re-push the configured token and retry once.
async fn call<P, R>(
    client: &Client,
    settings: &HostSettings,
    method: &str,
    params: P,
) -> Result<R, ClientError>
where
    P: serde::Serialize + Clone,
    R: serde::de::DeserializeOwned,
{
    match client.request(method, params.clone()).await {
        Err(ClientError::Rpc(e))
            if e.code == ahp_error_codes::AUTH_REQUIRED && settings.auth_resource.is_some() =>
        {
            authenticate(client, settings)
                .await
                .map_err(|_| ClientError::Rpc(e))?;
            client.request(method, params).await
        }
        other => other,
    }
}

/// Subscribe to `uri`, returning the subscription and raw snapshot, or
/// `None` if the host reports the channel does not exist.
async fn subscribe(
    client: &Client,
    uri: &str,
) -> anyhow::Result<Option<(SessionSubscription, Value)>> {
    let sub = client.attach_subscription(uri).await;
    match client
        .request::<_, Value>("subscribe", json!({ "channel": uri }))
        .await
    {
        Ok(result) => Ok(Some((
            sub,
            result.get("snapshot").cloned().unwrap_or(Value::Null),
        ))),
        Err(ClientError::Rpc(e)) if is_not_found(e.code) => Ok(None),
        Err(e) => Err(rpc_error(e)).with_context(|| format!("AHP subscribe to '{uri}' failed")),
    }
}

/// Dispatch `action` and wait for the host's echo. Returns the
/// `rejectionReason` if the host rejected it.
async fn dispatch_acked(
    client: &Client,
    client_id: &str,
    sub: &mut SessionSubscription,
    channel: &str,
    action: StateAction,
    timeout: Duration,
) -> anyhow::Result<Option<String>> {
    let handle = client
        .dispatch(channel.to_string(), action)
        .await
        .map_err(rpc_error)
        .context("AHP dispatchAction failed")?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let event = tokio::time::timeout_at(deadline, sub.recv())
            .await
            .map_err(|_| {
                anyhow!("timed out waiting for AHP host to acknowledge action on '{channel}'")
            })?
            .ok_or_else(|| anyhow!("AHP connection closed before action was acknowledged"))?;
        if let SubscriptionEvent::Action(env) = event {
            if env
                .origin
                .as_ref()
                .is_some_and(|o| o.client_seq == handle.client_seq && o.client_id == client_id)
            {
                return Ok(env.rejection_reason);
            }
        }
    }
}

fn default_chat_from_state(state: Option<&Value>) -> Option<String> {
    let state = state?;
    state
        .get("defaultChat")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            state
                .get("chats")
                .and_then(Value::as_array)
                .and_then(|chats| chats.first())
                .and_then(|c| c.get("resource"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn chat_has_turn(state: Option<&Value>, turn_id: &str) -> bool {
    let Some(state) = state else {
        return false;
    };
    if state.pointer("/activeTurn/id").and_then(Value::as_str) == Some(turn_id) {
        return true;
    }
    state
        .get("turns")
        .and_then(Value::as_array)
        .is_some_and(|turns| {
            turns
                .iter()
                .any(|t| t.get("id").and_then(Value::as_str) == Some(turn_id))
        })
}

fn is_not_found(code: i32) -> bool {
    code == ahp_error_codes::SESSION_NOT_FOUND
        || code == ahp_error_codes::NOT_FOUND
        || code == json_rpc_error_codes::INVALID_PARAMS
}

fn rpc_error(error: ClientError) -> anyhow::Error {
    match error {
        ClientError::Rpc(e) => anyhow!("AHP error {}: {}", e.code, e.message),
        other => anyhow!(other),
    }
}
