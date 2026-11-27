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

//! A scripted, in-process AHP host so the example runs without a real agent
//! host. It speaks the real AHP wire protocol over WebSocket.

use std::time::Duration;

use anyhow::Result;
use drasi_ahp_common::ahp_types::actions::StateAction;
use drasi_ahp_common::ahp_types::state::{ChangesSummary, SessionState};
use drasi_ahp_common::mock::MockAhpHost;
use serde_json::json;

const FIX_LOGIN: &str = "ahp-session:/fix-login";
const API_DOCS: &str = "ahp-session:/api-docs";
const UPGRADE: &str = "ahp-session:/upgrade-tokio";

const IDLE: u32 = 1;
const ERROR: u32 = 2;
const IN_PROGRESS: u32 = 8;

fn session(id: &str, provider: &str, title: &str, status: u32) -> Result<SessionState> {
    Ok(serde_json::from_value(json!({
        "provider": provider,
        "title": title,
        "status": status,
        "lifecycle": "ready",
        "activeClients": [],
        "chats": [{
            "resource": format!("ahp-chat:/{id}"),
            "title": "Main",
            "status": status,
            "modifiedAt": "2026-01-01T00:00:00.000Z"
        }]
    }))?)
}

fn ask(id: &str, chat: &str, question: &str) -> Result<StateAction> {
    Ok(serde_json::from_value(json!({
        "type": "session/inputNeededSet",
        "request": {
            "kind": "chatInput", "id": id, "chat": chat,
            "request": {"id": id, "message": question}
        }
    }))?)
}

fn answered(id: &str) -> Result<StateAction> {
    Ok(serde_json::from_value(
        json!({"type": "session/inputNeededRemoved", "id": id}),
    )?)
}

fn files_changed(files: i64, additions: i64, deletions: i64) -> ChangesSummary {
    ChangesSummary {
        additions: Some(additions),
        deletions: Some(deletions),
        files: Some(files),
    }
}

/// Start the demo host with two agents and two sessions.
pub async fn start() -> Result<MockAhpHost> {
    let host = MockAhpHost::start().await?;
    host.set_agents(serde_json::from_value(json!([
        {"provider": "copilot", "displayName": "GitHub Copilot", "description": "", "models": []},
        {"provider": "claude", "displayName": "Claude Code", "description": "", "models": []}
    ]))?)?;
    host.add_session(
        FIX_LOGIN,
        session("fix-login", "copilot", "Fix flaky login test", IDLE)?,
    );
    host.add_session(
        API_DOCS,
        session("api-docs", "claude", "Write API docs", IN_PROGRESS)?,
    );
    Ok(host)
}

/// Play a short story of agent activity, one step every two seconds.
pub async fn simulate(host: MockAhpHost) -> Result<()> {
    let step = || tokio::time::sleep(Duration::from_secs(2));

    step().await;
    println!("\n▶ Copilot starts a new session: Upgrade to tokio 2");
    host.add_session(
        UPGRADE,
        session(
            "upgrade-tokio",
            "copilot",
            "Upgrade to tokio 2",
            IN_PROGRESS,
        )?,
    );

    step().await;
    println!("\n▶ Copilot asks a question");
    host.dispatch(
        UPGRADE,
        ask("q1", "ahp-chat:/upgrade-tokio", "Bump the MSRV to 1.80?")?,
    )?;

    step().await;
    println!("\n▶ Claude asks a question");
    host.dispatch(
        API_DOCS,
        ask(
            "q2",
            "ahp-chat:/api-docs",
            "Should the docs target users or contributors?",
        )?,
    )?;

    step().await;
    println!("\n▶ You answer Copilot");
    host.dispatch(UPGRADE, answered("q1")?)?;

    step().await;
    println!("\n▶ Copilot rewrites a lot of code");
    host.set_session_changes(UPGRADE, files_changed(37, 820, 410));

    step().await;
    println!("\n▶ Claude's session crashes before you answer");
    host.dispatch(API_DOCS, answered("q2")?)?;
    host.set_session_status(API_DOCS, ERROR);

    step().await;
    println!("\n▶ Copilot keeps going");
    host.set_session_changes(UPGRADE, files_changed(52, 1210, 530));

    step().await;
    println!("\n✔ Demo finished — press Ctrl+C to exit");
    Ok(())
}
