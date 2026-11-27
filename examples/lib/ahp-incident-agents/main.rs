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

//! Incidents → Cypher query → AHP reaction: every critical incident gets its
//! own agent session on an Agent Host Protocol host.
//!
//! Runs against a built-in mock agent host, or a real host when
//! `AHP_ENDPOINT` is set (e.g. `ws://127.0.0.1:8080`).

use std::time::Duration;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_ahp::mock_host::{MockAgentHost, MockHostEvent};
use drasi_reaction_ahp::{AhpReaction, OnDelete};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error")).init();

    // 1. An agent host: a real one from AHP_ENDPOINT, or the built-in mock.
    let mock = match std::env::var("AHP_ENDPOINT") {
        Ok(_) => None,
        Err(_) => Some(start_mock_host().await?),
    };
    let endpoint = match &mock {
        Some(mock) => mock.url().to_string(),
        None => std::env::var("AHP_ENDPOINT")?,
    };

    // 2. A source the app pushes incidents into.
    let (source, incidents) = ApplicationSource::new(
        "incidents",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;

    // 3. A continuous query for critical, open incidents.
    let query = Query::cypher("critical-incidents")
        .query(
            r#"
            MATCH (i:Incident)
            WHERE i.status = 'open' AND i.severity <= 2
            RETURN i.id AS id, i.title AS title, i.service AS service, i.severity AS severity
            "#,
        )
        .from_source("incidents")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    // 4. The AHP reaction: one agent session per critical incident.
    let agents = AhpReaction::builder("incident-agents")
        .with_query("critical-incidents")
        .with_endpoint(&endpoint)
        .with_result_key_fields(["id"])
        .with_title_template("{{after.id}}: {{after.title}}")
        .with_message_template(
            "Incident {{after.id}} (sev {{after.severity}}) on {{after.service}}: \
             {{after.title}}. Find the root cause and propose a fix.",
        )
        .with_follow_up_template(
            "Update on {{after.id}}: severity changed from {{before.severity}} to {{after.severity}}.",
        )
        .with_on_delete(OnDelete::Archive)
        .build()?;

    let drasi = DrasiLib::builder()
        .with_id("ahp-incident-agents")
        .with_source(source)
        .with_query(query)
        .with_reaction(agents)
        .build()
        .await?;
    drasi.start().await?;
    println!("AHP host: {endpoint}");

    // 5. Play out an incident timeline.
    step("INC-1 opened: checkout API 500s (sev 2)").await;
    let props = incident("INC-1", "Checkout API returns 500s", "checkout", 2, "open");
    incidents
        .send_node_insert("INC-1", vec!["Incident"], props.build())
        .await?;

    step("INC-2 opened: slow dashboard (sev 4, not critical)").await;
    let props = incident("INC-2", "Dashboard is slow", "web", 4, "open");
    incidents
        .send_node_insert("INC-2", vec!["Incident"], props.build())
        .await?;

    step("INC-1 escalated to sev 1").await;
    let props = incident("INC-1", "Checkout API returns 500s", "checkout", 1, "open");
    incidents
        .send_node_update("INC-1", vec!["Incident"], props.build())
        .await?;

    step("INC-2 escalated to sev 2").await;
    let props = incident("INC-2", "Dashboard is slow", "web", 2, "open");
    incidents
        .send_node_update("INC-2", vec!["Incident"], props.build())
        .await?;

    step("INC-1 resolved").await;
    let props = incident(
        "INC-1",
        "Checkout API returns 500s",
        "checkout",
        1,
        "resolved",
    );
    incidents
        .send_node_update("INC-1", vec!["Incident"], props.build())
        .await?;

    tokio::time::sleep(Duration::from_secs(3)).await;
    drasi.stop().await?;
    println!("\nDone.");
    Ok(())
}

/// Properties of an `Incident` node.
fn incident(
    id: &str,
    title: &str,
    service: &str,
    severity: i64,
    status: &str,
) -> PropertyMapBuilder {
    PropertyMapBuilder::new()
        .with_string("id", id)
        .with_string("title", title)
        .with_string("service", service)
        .with_integer("severity", severity)
        .with_string("status", status)
}
async fn step(description: &str) {
    tokio::time::sleep(Duration::from_millis(1500)).await;
    println!("\n> {description}");
}

/// Start the mock agent host and print what the "agents" are doing.
async fn start_mock_host() -> Result<MockAgentHost> {
    let host = MockAgentHost::builder()
        .with_turn_delay(Duration::from_secs(2))
        .with_responder(|text| format!("Looked into it: {}", text.lines().next().unwrap_or("")))
        .start()
        .await?;
    let mut events = host.events();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                MockHostEvent::SessionTitled { title, .. } => {
                    println!("  [host]  new agent session \"{title}\"")
                }
                MockHostEvent::TurnStarted { text, .. } => {
                    println!("  [host]  turn started: {text}")
                }
                MockHostEvent::FollowUpQueued { text, .. } => {
                    println!("  [host]  follow-up queued: {text}")
                }
                MockHostEvent::TurnCompleted { response, .. } => println!("  [agent] {response}"),
                MockHostEvent::TurnCancelled { .. } => println!("  [host]  active turn cancelled"),
                MockHostEvent::SessionArchived { session } => {
                    println!("  [host]  session archived: {session}")
                }
                _ => {}
            }
        }
    });
    Ok(host)
}
