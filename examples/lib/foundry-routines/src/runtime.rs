// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::{Connection, Routine},
    reaction::{FoundryReaction, Handle},
    store::Store,
};
use anyhow::{ensure, Context, Result};
use drasi_lib::{
    config::{QueryJoinConfig, QueryJoinKeyConfig, SourceSubscriptionConfig},
    DrasiLib, Source,
};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

pub struct Live {
    pub handle: Handle,
    pub reaction_id: String,
}

pub enum StartMode {
    Preview,
    Active,
    Paused,
}

pub struct Runtime {
    pub core: Arc<DrasiLib>,
    pub live: Mutex<HashMap<String, Live>>,
    pub store: Arc<Store>,
}

pub fn source(config: &Connection) -> Result<Box<dyn Source>> {
    let password = config.password()?;
    match config.kind.as_str() {
        "postgres" => {
            let tls = if config.tls == "disabled" {
                drasi_source_postgres::SslMode::Disable
            } else {
                drasi_source_postgres::SslMode::Require
            };
            let keys: Vec<_> = config
                .table_keys
                .iter()
                .map(|k| drasi_source_postgres::TableKeyConfig {
                    table: k.table.clone(),
                    key_columns: k.columns.clone(),
                })
                .collect();
            let bootstrap = drasi_bootstrap_postgres::PostgresBootstrapProvider::builder()
                .with_host(&config.host)
                .with_port(config.port)
                .with_database(&config.database)
                .with_user(&config.user)
                .with_password(&password)
                .with_tables(config.tables.clone())
                .with_slot_name(&config.slot)
                .with_publication_name(&config.publication)
                .with_ssl_mode(if config.tls == "disabled" {
                    drasi_bootstrap_postgres::SslMode::Disable
                } else {
                    drasi_bootstrap_postgres::SslMode::Require
                })
                .with_table_keys(
                    keys.iter()
                        .map(|k| drasi_bootstrap_postgres::TableKeyConfig {
                            table: k.table.clone(),
                            key_columns: k.key_columns.clone(),
                        })
                        .collect(),
                )
                .build();
            Ok(Box::new(
                drasi_source_postgres::PostgresReplicationSource::builder(&config.id)
                    .with_host(&config.host)
                    .with_port(config.port)
                    .with_database(&config.database)
                    .with_user(&config.user)
                    .with_password(password)
                    .with_tables(config.tables.clone())
                    .with_slot_name(&config.slot)
                    .with_publication_name(&config.publication)
                    .with_ssl_mode(tls)
                    .with_table_keys(keys)
                    .with_bootstrap_provider(bootstrap)
                    .with_auto_start(false)
                    .build()?,
            ))
        }
        "mysql" => {
            let tls = if config.tls == "disabled" {
                drasi_mysql_common::config::SslMode::Disabled
            } else {
                drasi_mysql_common::config::SslMode::RequireVerifyFull
            };
            let keys: Vec<_> = config
                .table_keys
                .iter()
                .map(|k| drasi_source_mysql::TableKeyConfig {
                    table: k.table.clone(),
                    key_columns: k.columns.clone(),
                })
                .collect();
            let bootstrap = drasi_bootstrap_mysql::MySqlBootstrapProvider::builder()
                .with_host(&config.host)
                .with_port(config.port)
                .with_database(&config.database)
                .with_user(&config.user)
                .with_password(&password)
                .with_tables(config.tables.clone())
                .with_ssl_mode(tls)
                .with_table_keys(
                    keys.iter()
                        .map(|k| drasi_bootstrap_mysql::TableKeyConfig {
                            table: k.table.clone(),
                            key_columns: k.key_columns.clone(),
                        })
                        .collect(),
                )
                .build()?;
            Ok(Box::new(
                drasi_source_mysql::MySqlReplicationSource::builder(&config.id)
                    .with_host(&config.host)
                    .with_port(config.port)
                    .with_database(&config.database)
                    .with_user(&config.user)
                    .with_password(password)
                    .with_tables(config.tables.clone())
                    .with_ssl_mode(tls)
                    .with_table_keys(keys)
                    .with_server_id(config.server_id)
                    .with_bootstrap_provider(bootstrap)
                    .with_auto_start(false)
                    .build()?,
            ))
        }
        _ => anyhow::bail!("Unsupported source"),
    }
}

impl Runtime {
    pub async fn new(store: Arc<Store>) -> Result<Arc<Self>> {
        let core = DrasiLib::builder()
            .with_state_store_provider(store.provider.clone())
            .build()
            .await?;
        core.start().await?;
        Ok(Arc::new(Self {
            core: Arc::new(core),
            live: Mutex::new(HashMap::new()),
            store,
        }))
    }

    pub async fn start(&self, routine: &Routine, mode: StartMode) -> Result<Handle> {
        routine.validate()?;
        let enabled = matches!(mode, StartMode::Active);
        let mut live = self.live.lock().await;
        ensure!(
            !live.contains_key(&routine.id),
            "Routine is already loaded; pause or remove it before replacing"
        );
        let records = self.store.read().await;
        for binding in &routine.sources {
            let config = records
                .connections
                .get(&binding.connection_id)
                .context("Configured connection not found")?;
            ensure!(
                binding.labels.iter().all(|l| config.tables.contains(l)),
                "Query labels must refer to configured source tables"
            );
            if !self
                .core
                .list_sources()
                .await?
                .iter()
                .any(|(id, _)| id == &config.id)
            {
                let report = crate::metadata::validate(config).await?;
                ensure!(
                    report["checks"]
                        .as_array()
                        .context("Missing readiness checks")?
                        .iter()
                        .all(|c| c["ok"] == true),
                    "CDC readiness checks failed: {}",
                    report["checks"]
                );
                let mut resolved = config.clone();
                for table in &resolved.tables {
                    if !resolved.table_keys.iter().any(|key| &key.table == table) {
                        let columns = report["tables"]
                            .as_array()
                            .context("Missing schema")?
                            .iter()
                            .find(|row| row["name"] == *table)
                            .and_then(|row| row["columns"].as_array())
                            .context("Selected table was not found")?;
                        let keys: Vec<String> = columns
                            .iter()
                            .filter(|column| column["primaryKey"] == true)
                            .filter_map(|column| column["name"].as_str().map(String::from))
                            .collect();
                        ensure!(
                            !keys.is_empty(),
                            "Configure key columns for table '{table}'"
                        );
                        resolved.table_keys.push(crate::config::TableKey {
                            table: table.clone(),
                            columns: keys,
                        });
                    }
                }
                self.core.add_source(source(&resolved)?).await?;
                self.core.start_source(&config.id).await?;
            } else {
                ensure!(self.core.get_source_status(&config.id).await?==drasi_lib::channels::ComponentStatus::Running,
                    "Source '{}' is not running; repair its configuration or restart the backend before resuming",config.id);
            }
        }
        let query_id = format!("q-{}", uuid::Uuid::new_v4());
        let reaction_id = format!("r-{}", uuid::Uuid::new_v4());
        let query_text = routine.query.replace("\r\n", "\n").replace('\r', "\n");
        let mut query = drasi_lib::Query::cypher(&query_id)
            .query(query_text)
            .auto_start(false)
            .with_bootstrap_timeout_secs(30)
            .build();
        query.sources = routine
            .sources
            .iter()
            .map(|b| SourceSubscriptionConfig {
                source_id: b.connection_id.clone(),
                nodes: b.labels.clone(),
                relations: vec![],
                pipeline: vec![],
            })
            .collect();
        query.joins = Some(
            routine
                .joins
                .iter()
                .map(|j| QueryJoinConfig {
                    id: j.name.clone(),
                    keys: vec![
                        QueryJoinKeyConfig {
                            label: j.left_label.clone(),
                            property: j.left_property.clone(),
                        },
                        QueryJoinKeyConfig {
                            label: j.right_label.clone(),
                            property: j.right_property.clone(),
                        },
                    ],
                })
                .collect(),
        );
        self.core.add_query(query).await?;
        let reaction = FoundryReaction::new(
            reaction_id.clone(),
            query_id.clone(),
            routine.clone(),
            self.store.clone(),
            enabled,
        );
        let handle = reaction.handle.clone();
        let started = async {
            self.core.add_reaction(reaction).await?;
            self.core.start_query(&query_id).await?;
            // Snapshot validates bootstrap before the host attaches its atomic live boundary.
            let snapshot = handle.snapshot().await?;
            let mut keys = std::collections::HashSet::new();
            for row in snapshot["rows"].as_array().context("Invalid snapshot")? {
                if !matches!(mode, StartMode::Preview) {
                    ensure!(
                        keys.insert(routine.key(row)?.to_string()),
                        "Result keys are not unique"
                    );
                }
            }
            self.core.start_reaction(&reaction_id).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = started {
            if let Err(e) = self.core.remove_reaction(&reaction_id, false).await {
                log::error!("Reaction cleanup failed: {e}");
            }
            if let Err(e) = self.core.remove_query(&query_id).await {
                log::error!("Query cleanup failed: {e}");
            }
            return Err(error);
        }
        live.insert(
            routine.id.clone(),
            Live {
                handle: handle.clone(),
                reaction_id,
            },
        );
        Ok(handle)
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        let mut live = self.live.lock().await;
        if let Some(runtime) = live.get(id) {
            runtime.handle.gate.lock().await.enabled = false;
            self.core
                .remove_reaction(&runtime.reaction_id, false)
                .await?;
            self.core.remove_query(&runtime.handle.query_id).await?;
            live.remove(id);
        }
        Ok(())
    }
}
