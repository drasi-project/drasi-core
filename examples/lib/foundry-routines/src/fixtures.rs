// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0
use crate::{
    config::{Connection, Routine},
    store::Store,
};
use anyhow::Result;
use serde_json::Value;

pub async fn load(store: &Store, offline: bool) -> Result<()> {
    let agent = std::env::var("FOUNDRY_AGENT_NAME").ok();
    load_with_agent(store, offline, agent.as_deref()).await
}

async fn load_with_agent(store: &Store, offline: bool, agent: Option<&str>) -> Result<()> {
    let connections: Vec<Connection> =
        serde_json::from_str(include_str!("../fixtures/connections.json"))?;
    let mut routine: Routine = serde_json::from_str(include_str!("../fixtures/routine.json"))?;
    routine.query = include_str!("../fixtures/order-risk.cypher").into();
    let test_agent = routine.agent.clone();
    if !offline {
        routine.agent = agent.map(str::trim).unwrap_or_default().into();
    }
    store
        .update(|records| {
            for connection in connections {
                records
                    .connections
                    .entry(connection.id.clone())
                    .or_insert(connection);
            }
            records
                .routines
                .entry(routine.id.clone())
                .and_modify(|saved| {
                    if !offline
                        && (saved.agent == test_agent || saved.agent.is_empty())
                        && saved.agent != routine.agent
                    {
                        saved.agent = routine.agent.clone();
                        saved.revision += 1;
                        saved.status = "Draft".into();
                        saved.error = None;
                    }
                })
                .or_insert(routine);
            Ok(())
        })
        .await
}

pub fn actions() -> Result<Value> {
    Ok(serde_json::from_str(include_str!(
        "../fixtures/actions.json"
    ))?)
}

pub async fn execute(store: &Store, name: &str) -> Result<()> {
    use anyhow::{ensure, Context};
    let actions = actions()?;
    let action = actions
        .as_array()
        .context("Invalid fixture actions")?
        .iter()
        .find(|a| a["name"] == name)
        .context("Unknown fixture action")?;
    let id = action["connectionId"]
        .as_str()
        .context("Fixture connection missing")?;
    let expected: Vec<Connection> =
        serde_json::from_str(include_str!("../fixtures/connections.json"))?;
    let mut connection = store
        .read()
        .await
        .connections
        .get(id)
        .context("Fixture connection not found")?
        .clone();
    let expected = expected
        .iter()
        .find(|c| c.id == id)
        .context("Unknown fixture connection")?;
    ensure!(
        serde_json::to_value(&connection)? == serde_json::to_value(expected)?,
        "Fixture controls cannot target edited or external connections"
    );
    connection.user = if connection.kind == "postgres" {
        "postgres".into()
    } else {
        "root".into()
    };
    crate::metadata::execute(
        &connection,
        "local-fixture-admin",
        action["sql"].as_str().context("Missing fixture SQL")?,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::sync::Arc;

    #[tokio::test]
    async fn live_start_clears_the_test_agent_and_preserves_a_user_selected_agent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let provider = Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
            directory.path().join("state.redb"),
        )?);
        let store = Store::open(provider).await?;
        load_with_agent(&store, true, None).await?;
        let fixture: Routine = serde_json::from_str(include_str!("../fixtures/routine.json"))?;
        let original = store.read().await.routines[&fixture.id].clone();
        load_with_agent(&store, false, None).await?;
        let live = store.read().await.routines[&fixture.id].clone();
        assert!(live.agent.is_empty());
        assert_eq!(live.revision, original.revision + 1);
        assert_eq!(live.status, "Draft");
        load_with_agent(&store, false, Some("env-agent")).await?;
        assert_eq!(store.read().await.routines[&fixture.id].agent, "env-agent");
        store
            .update(|records| {
                records
                    .routines
                    .get_mut(&fixture.id)
                    .context("Fixture missing")?
                    .agent = "selected-agent".into();
                Ok(())
            })
            .await?;
        load_with_agent(&store, false, Some("another-env-agent")).await?;
        assert_eq!(
            store.read().await.routines[&fixture.id].agent,
            "selected-agent"
        );
        Ok(())
    }

    #[tokio::test]
    async fn configured_agent_replaces_only_the_saved_test_agent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let provider = Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(
            directory.path().join("state.redb"),
        )?);
        let store = Store::open(provider).await?;
        load_with_agent(&store, true, None).await?;
        load_with_agent(&store, false, Some(" configured-agent ")).await?;
        let fixture: Routine = serde_json::from_str(include_str!("../fixtures/routine.json"))?;
        assert_eq!(
            store.read().await.routines[&fixture.id].agent,
            "configured-agent"
        );
        Ok(())
    }
}
