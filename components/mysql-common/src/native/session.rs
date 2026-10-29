// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog::field,
    connection::{Connection, MySqlServerError, Rows, SESSION_ATTRIBUTE},
    MySqlConnectionConfig,
};
use anyhow::{Context, Result};
use drasi_computation_contracts::OperationFailures;
use std::{collections::BTreeSet, time::Duration};

#[cfg(test)]
mod tests;

/// Borrowed access to the operation's real connections. Connection IDs are
/// captured from its handshakes and fenced by a per-connection nonce, not just
/// the persistent server UUID, which survives restarts that reuse numeric IDs.
pub struct SessionCleanup<'a> {
    pub config: &'a MySqlConnectionConfig,
    pub max_bytes: usize,
    pub timeout: Duration,
    pub control: &'a mut Option<Connection>,
    pub sessions: &'a mut Vec<Connection>,
    pub server: &'a Option<String>,
    pub killed: &'a mut BTreeSet<u32>,
}

pub async fn server_id(connection: &mut Connection) -> Result<String> {
    let rows = connection.query("SELECT @@server_uuid").await?;
    let [row] = rows.values.as_slice() else {
        anyhow::bail!("missing MySQL server identity");
    };
    Ok(field(row, 0)?.to_owned())
}

pub async fn connect(
    config: &MySqlConnectionConfig,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Connection> {
    let mut connection = Connection::connect(config, max_bytes, timeout).await?;
    let proof = async {
        let marker = connection
            .session_marker
            .clone()
            .context("MySQL server does not support session ownership attributes")?;
        let id = connection.connection_id;
        anyhow::ensure!(
            owns(&mut connection, id, &marker).await?,
            "MySQL session disappeared before ownership verification"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = proof {
        return match connection.close().await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(OperationFailures::new(
                "MySQL session ownership verification and close failed",
                vec![error, cleanup],
            )
            .into()),
        };
    }
    Ok(connection)
}

async fn owns(control: &mut Connection, id: u32, marker: &str) -> Result<bool> {
    let rows = control
        .query(&format!(
            "SELECT p.ID,a.ATTR_VALUE FROM information_schema.PROCESSLIST p \
             LEFT JOIN performance_schema.session_account_connect_attrs a \
             ON a.PROCESSLIST_ID=p.ID AND a.ATTR_NAME='{SESSION_ATTRIBUTE}' WHERE p.ID={id}"
        ))
        .await?;
    validate_owner(&rows, id, marker)
}

fn validate_owner(rows: &Rows, id: u32, marker: &str) -> Result<bool> {
    let [row] = rows.values.as_slice() else {
        anyhow::ensure!(
            rows.values.is_empty(),
            "ambiguous MySQL session ownership response"
        );
        return Ok(false);
    };
    anyhow::ensure!(
        row.len() == 2 && field(row, 0)?.parse::<u32>()? == id,
        "invalid MySQL session ownership response"
    );
    let actual = row[1]
        .as_deref()
        .context("MySQL session ownership attribute is missing; refusing KILL")?;
    anyhow::ensure!(
        actual == marker.as_bytes(),
        "MySQL session ownership changed; refusing KILL of reused connection ID {id}"
    );
    Ok(true)
}

impl SessionCleanup<'_> {
    pub async fn cleanup(&mut self) -> Result<()> {
        let mut failure = None;
        let mut close_failure = None;
        while !self.sessions.is_empty() {
            if let Err(error) = self.retire().await {
                log::error!("MySQL owned session cleanup pending: {error:#}");
                if failure.is_none() {
                    failure = Some(error);
                }
                if let Some(control) = self.control.take() {
                    if let Err(error) = control.close().await {
                        log::error!("MySQL cleanup control close failed: {error:#}");
                        if close_failure.is_none() {
                            close_failure = Some(error);
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if let Some(control) = self.control.take() {
            if let Err(error) = control.close().await {
                if close_failure.is_none() {
                    close_failure = Some(error);
                }
            }
        }
        let failures: Vec<_> = failure.into_iter().chain(close_failure).collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                OperationFailures::new("MySQL session cleanup encountered failures", failures)
                    .into(),
            )
        }
    }

    async fn retire(&mut self) -> Result<()> {
        if self.control.is_none() {
            let mut control = connect(self.config, self.max_bytes, self.timeout).await?;
            anyhow::ensure!(
                Some(server_id(&mut control).await?) == *self.server,
                "cannot retire owned sessions on another MySQL server"
            );
            *self.control = Some(control);
        }
        let control = self
            .control
            .as_mut()
            .context("missing MySQL cleanup connection")?;
        for connection in self.sessions.iter() {
            if self.killed.contains(&connection.connection_id) {
                continue;
            }
            let marker = connection
                .session_marker
                .as_deref()
                .context("MySQL session has no ownership attribute; refusing KILL")?;
            if !owns(control, connection.connection_id, marker).await? {
                continue;
            }
            if let Err(error) = control
                .execute(&format!("KILL CONNECTION {}", connection.connection_id))
                .await
            {
                if error
                    .downcast_ref::<MySqlServerError>()
                    .is_none_or(|error| error.code != 1094)
                {
                    return Err(error);
                }
            }
            self.killed.insert(connection.connection_id);
        }
        for connection in self.sessions.iter() {
            let rows = control
                .query(&format!(
                    "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID={}",
                    connection.connection_id
                ))
                .await?;
            anyhow::ensure!(
                rows.values.len() == 1,
                "invalid MySQL session retirement response"
            );
            if field(&rows.values[0], 0)? != "0" {
                tokio::time::sleep(Duration::from_millis(10)).await;
                return Ok(());
            }
        }
        while let Some(connection) = self.sessions.pop() {
            connection.close().await?;
        }
        Ok(())
    }
}
