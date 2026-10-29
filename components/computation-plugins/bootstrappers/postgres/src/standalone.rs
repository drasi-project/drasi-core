// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Ordinary read-only PostgreSQL snapshots, independent of replication.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapWatermark, ChangeEnvelope, ComponentId, ComputationBootstrapSnapshot,
    GraphChangeCodec, StreamId,
};
use drasi_computation_plugin_sdk::{NativeBootstrapProvider, Scope};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_postgres_common::{
    config::{SslMode, TableKeyConfig},
    replication::{connect_with_options, field, row_element, Connection, ConnectionOptions},
    types::{ColumnInfo, RelationInfo, ReplicaIdentity},
    PostgresValue,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    num::{NonZeroU64, NonZeroUsize},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::{watch, Mutex as AsyncMutex},
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresBootstrapConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    pub tables: Vec<String>,
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
    pub max_tables: NonZeroUsize,
    pub max_protocol_bytes: NonZeroUsize,
    pub io_timeout_ms: NonZeroU64,
    pub max_duration_ms: NonZeroU64,
}

fn default_host() -> String {
    "localhost".into()
}
fn default_port() -> u16 {
    5432
}

impl PostgresBootstrapConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.port > 0
                && !self.host.is_empty()
                && !self.database.is_empty()
                && !self.user.is_empty(),
            "PostgreSQL snapshot requires host, port, database and user"
        );
        anyhow::ensure!(
            [&self.host, &self.database, &self.user, &self.password]
                .into_iter()
                .all(|value| !value.contains('\0')),
            "PostgreSQL connection fields cannot contain NUL"
        );
        anyhow::ensure!(
            self.max_protocol_bytes.get() >= 64
                && self
                    .tls_ca_pem
                    .as_ref()
                    .is_none_or(|value| !value.is_empty()
                        && value.len() <= 65536
                        && value.len() <= self.max_protocol_bytes.get()
                        && !value.contains('\0')),
            "invalid PostgreSQL protocol or CA certificate limit"
        );
        anyhow::ensure!(
            !self.tables.is_empty() && self.tables.len() <= self.max_tables.get(),
            "PostgreSQL snapshot requires an explicit table selection within max_tables"
        );
        let mut tables = BTreeSet::new();
        for table in &self.tables {
            let (schema, name) = table_name(table);
            anyhow::ensure!(
                !schema.is_empty()
                    && !name.is_empty()
                    && !table.contains('\0')
                    && tables.insert((schema, name)),
                "invalid or duplicate PostgreSQL snapshot table"
            );
        }
        let mut keys = BTreeSet::new();
        for key in &self.table_keys {
            let table = table_name(&key.table);
            anyhow::ensure!(
                tables.contains(&table)
                    && keys.insert(table)
                    && !key.key_columns.is_empty()
                    && key
                        .key_columns
                        .iter()
                        .all(|column| !column.is_empty() && !column.contains('\0'))
                    && key.key_columns.iter().collect::<BTreeSet<_>>().len()
                        == key.key_columns.len(),
                "invalid PostgreSQL snapshot key configuration"
            );
        }
        for duration in [self.io_timeout_ms, self.max_duration_ms] {
            Instant::now()
                .checked_add(Duration::from_millis(duration.get()))
                .context("PostgreSQL snapshot duration is not representable")?;
        }
        Ok(())
    }
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }
    fn options(&self) -> ConnectionOptions<'_> {
        ConnectionOptions {
            host: &self.host,
            port: self.port,
            database: &self.database,
            user: &self.user,
            password: &self.password,
            ssl_mode: self.ssl_mode,
            tls_ca_pem: self.tls_ca_pem.as_deref(),
            max_protocol_bytes: self.max_protocol_bytes,
            timeout: self.timeout(),
        }
    }
}

struct Run {
    connection: AsyncMutex<Option<Connection>>,
    shutdown: watch::Sender<bool>,
    complete: AtomicBool,
}

impl Run {
    fn check(&self) -> Result<()> {
        anyhow::ensure!(
            !*self.shutdown.borrow(),
            "PostgreSQL snapshot stream is retired"
        );
        Ok(())
    }
}

pub struct PostgresBootstrapProvider {
    source: ComponentId,
    stream: StreamId,
    config: Arc<PostgresBootstrapConfig>,
    run: Mutex<Option<Arc<Run>>>,
    lifecycle: AsyncMutex<()>,
    pub(crate) query_scope: Option<(Scope, ComponentId)>,
}

impl PostgresBootstrapProvider {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: PostgresBootstrapConfig,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            source,
            stream,
            config: Arc::new(config),
            run: Mutex::new(None),
            lifecycle: AsyncMutex::new(()),
            query_scope: None,
        })
    }

    fn current(&self) -> Result<Option<Arc<Run>>> {
        Ok(self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))?
            .clone())
    }

    async fn begin(&self, run: Arc<Run>, deadline: Instant) -> Result<Reader> {
        let connection = connect_with_options(self.config.options(), false).await?;
        let mut owner = run.connection.lock().await;
        *owner = Some(connection);
        let connection = owner
            .as_mut()
            .context("PostgreSQL snapshot connection missing")?;
        let rows = connection
            .query_rows(
                "SELECT floor(extract(epoch FROM pg_catalog.clock_timestamp()) * 1000)::bigint"
                    .into(),
                self.config.max_protocol_bytes,
            )
            .await?;
        let [row] = rows.as_slice() else {
            anyhow::bail!("missing PostgreSQL snapshot timestamp");
        };
        let timestamp = field(row, 0)?.parse::<u64>()?;
        connection
            .query_rows(
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY".into(),
                self.config.max_protocol_bytes,
            )
            .await?;
        let mut tables = VecDeque::new();
        let mut metadata_bytes = 0usize;
        for selected in &self.config.tables {
            let (schema, table) = table_name(selected);
            let rows = connection.query_rows(format!(
                "SELECT a.attname, CASE WHEN t.typtype='d' THEN t.typbasetype ELSE a.atttypid END::text \
                 FROM pg_attribute a JOIN pg_class c ON a.attrelid=c.oid \
                 JOIN pg_namespace n ON c.relnamespace=n.oid JOIN pg_type t ON a.atttypid=t.oid \
                 WHERE n.nspname='{}' AND c.relname='{}' AND c.relkind IN ('r','p','v','m','f') \
                 AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum",
                schema.replace('\'', "''"), table.replace('\'', "''"),
            ), self.config.max_protocol_bytes).await?;
            anyhow::ensure!(
                !rows.is_empty(),
                "PostgreSQL snapshot table '{selected}' has no readable columns"
            );
            let keys = match self
                .config
                .table_keys
                .iter()
                .find(|key| table_name(&key.table) == (schema, table))
            {
                Some(key) => key.key_columns.clone(),
                None => connection
                    .query_rows(
                        format!(
                            "SELECT * FROM ({}) AS keys WHERE schema_name='{}' AND table_name='{}'",
                            drasi_postgres_common::PRIMARY_KEY_QUERY,
                            schema.replace('\'', "''"),
                            table.replace('\'', "''"),
                        ),
                        self.config.max_protocol_bytes,
                    )
                    .await?
                    .iter()
                    .map(|row| field(row, 2))
                    .collect::<Result<Vec<_>>>()?,
            };
            let columns = rows
                .iter()
                .map(|row| {
                    let name = field(row, 0)?;
                    Ok(ColumnInfo {
                        is_key: keys.contains(&name),
                        name,
                        type_oid: field(row, 1)?.parse()?,
                        type_modifier: -1,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            anyhow::ensure!(
                keys.iter()
                    .all(|key| columns.iter().any(|column| &column.name == key)),
                "PostgreSQL snapshot key is not a selected column"
            );
            if keys.is_empty() {
                log::warn!("PostgreSQL snapshot table '{selected}' has no key; rows receive snapshot-local IDs");
            }
            let size = std::mem::size_of::<Table>()
                + schema.len()
                + table.len()
                + columns
                    .iter()
                    .map(|column| std::mem::size_of::<ColumnInfo>() + column.name.len())
                    .sum::<usize>()
                + keys
                    .iter()
                    .map(|key| std::mem::size_of::<String>() + key.len())
                    .sum::<usize>();
            metadata_bytes = metadata_bytes
                .checked_add(size)
                .context("PostgreSQL snapshot metadata overflow")?;
            anyhow::ensure!(
                metadata_bytes <= self.config.max_protocol_bytes.get(),
                "PostgreSQL snapshot metadata exceeds configured byte limit"
            );
            tables.push_back(Table {
                relation: RelationInfo {
                    id: 0,
                    namespace: schema.into(),
                    name: table.into(),
                    replica_identity: ReplicaIdentity::Full,
                    columns,
                },
                keys,
            });
        }
        drop(owner);
        Ok(Reader {
            source: self.source.clone(),
            stream: self.stream.clone(),
            config: self.config.clone(),
            run,
            tables,
            opened: false,
            ordinal: 0,
            timestamp,
            deadline,
        })
    }
}

#[async_trait]
impl NativeBootstrapProvider for PostgresBootstrapProvider {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        if let Some((scope, owner)) = &self.query_scope {
            anyhow::ensure!(
                scope.instance_id == instance && scope.graph_id == graph && owner == query,
                "PostgreSQL standalone snapshot belongs to another query"
            );
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(
            self.current()?.is_none(),
            "PostgreSQL snapshot requires awaited cleanup before reuse"
        );
        let run = Arc::new(Run {
            connection: AsyncMutex::new(None),
            shutdown: watch::channel(false).0,
            complete: AtomicBool::new(false),
        });
        *self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))? =
            Some(run.clone());
        let deadline = Instant::now() + Duration::from_millis(self.config.max_duration_ms.get());
        let reader = tokio::time::timeout_at(
            deadline.min(Instant::now() + self.config.timeout()),
            self.begin(run, deadline),
        )
        .await
        .context("PostgreSQL snapshot initialization timed out")??;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    let item = reader.next().await?;
                    Ok(item.map(|item| (item, reader)))
                },
            )),
            watermarks: Vec::new(),
        })
    }

    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        anyhow::ensure!(
            self.current()?
                .is_some_and(|run| run.complete.load(Ordering::Acquire)),
            "PostgreSQL snapshot was not exhausted"
        );
        Ok(Vec::new())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(run) = self.current()? {
            run.shutdown.send_replace(true);
        }
        let _lifecycle = tokio::time::timeout(self.config.timeout(), self.lifecycle.lock())
            .await
            .context("PostgreSQL snapshot initialization cleanup timed out")?;
        if let Some(run) = self.current()? {
            run.shutdown.send_replace(true);
            tokio::time::timeout(self.config.timeout(), run.connection.lock())
                .await
                .context("PostgreSQL snapshot I/O cleanup timed out")?
                .take();
        }
        self.run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))?
            .take();
        Ok(())
    }
}

impl Drop for PostgresBootstrapProvider {
    fn drop(&mut self) {
        if let Ok(Some(run)) = self.current() {
            run.shutdown.send_replace(true);
            match run.connection.try_lock() {
                Ok(mut connection) => {
                    connection.take();
                }
                Err(_) => {
                    log::warn!("PostgreSQL snapshot dropped during I/O without awaited cleanup")
                }
            };
        }
    }
}

struct Table {
    relation: RelationInfo,
    keys: Vec<String>,
}

struct Reader {
    source: ComponentId,
    stream: StreamId,
    config: Arc<PostgresBootstrapConfig>,
    run: Arc<Run>,
    tables: VecDeque<Table>,
    opened: bool,
    ordinal: u64,
    timestamp: u64,
    deadline: Instant,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
        if let Ok(mut connection) = self.run.connection.try_lock() {
            connection.take();
        }
    }
}

impl Reader {
    async fn next(&mut self) -> Result<Option<ChangeEnvelope>> {
        self.run.check()?;
        let run = self.run.clone();
        let mut shutdown = run.shutdown.subscribe();
        let mut owner = run.connection.lock().await;
        let connection = owner.as_mut().context("PostgreSQL snapshot is stopped")?;
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopped| *stopped) => anyhow::bail!("PostgreSQL snapshot is stopped"),
            result = tokio::time::timeout_at(
                self.deadline.min(Instant::now() + self.config.timeout()), self.read(connection),
            ) => result.context("PostgreSQL snapshot read timed out")?,
        }
    }

    async fn read(&mut self, connection: &mut Connection) -> Result<Option<ChangeEnvelope>> {
        loop {
            let Some(table) = self.tables.front() else {
                connection
                    .query_rows("COMMIT".into(), self.config.max_protocol_bytes)
                    .await?;
                self.run.complete.store(true, Ordering::Release);
                return Ok(None);
            };
            if !self.opened {
                let columns = table
                    .relation
                    .columns
                    .iter()
                    .map(|column| format!("{}::text", quote(&column.name)))
                    .collect::<Vec<_>>()
                    .join(", ");
                connection
                    .query_rows(
                        format!(
                    "DECLARE drasi_snapshot NO SCROLL CURSOR FOR SELECT {columns} FROM {}.{}",
                    quote(&table.relation.namespace), quote(&table.relation.name),
                ),
                        self.config.max_protocol_bytes,
                    )
                    .await?;
                self.opened = true;
            }
            let rows = connection
                .query_rows(
                    "FETCH FORWARD 1 FROM drasi_snapshot".into(),
                    self.config.max_protocol_bytes,
                )
                .await?;
            if rows.is_empty() {
                connection
                    .query_rows(
                        "CLOSE drasi_snapshot".into(),
                        self.config.max_protocol_bytes,
                    )
                    .await?;
                self.tables.pop_front();
                self.opened = false;
                continue;
            }
            let [row] = rows.as_slice() else {
                anyhow::bail!("PostgreSQL cursor returned multiple rows");
            };
            anyhow::ensure!(
                row.len() == table.relation.columns.len(),
                "PostgreSQL snapshot columns changed"
            );
            let values = row
                .iter()
                .zip(&table.relation.columns)
                .map(|(value, column)| match value {
                    None => Ok(PostgresValue::Null),
                    Some(bytes) => drasi_postgres_common::decode_text_to_postgres_value(
                        std::str::from_utf8(bytes)?,
                        column.type_oid,
                    ),
                })
                .collect::<Result<Vec<_>>>()?;
            let element = if table.keys.is_empty() {
                let mut properties = ElementPropertyMap::new();
                for (column, value) in table.relation.columns.iter().zip(&values) {
                    properties.insert(&column.name, value.to_element_value());
                }
                let label = if table.relation.namespace == "public" {
                    table.relation.name.clone()
                } else {
                    format!("{}.{}", table.relation.namespace, table.relation.name)
                };
                Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(
                            self.source.as_str(),
                            &format!("{label}:{}", uuid::Uuid::new_v4()),
                        ),
                        labels: Arc::from([Arc::from(label)]),
                        effective_from: self.timestamp,
                    },
                    properties,
                }
            } else {
                row_element(
                    &self.source,
                    &table.relation,
                    &table.keys,
                    &values,
                    self.timestamp,
                )?
            };
            self.ordinal = self
                .ordinal
                .checked_add(1)
                .context("snapshot ordinal exhausted")?;
            return Ok(Some(GraphChangeCodec::encode_change(
                SourceChange::Insert { element },
                self.stream.clone(),
                self.ordinal,
                None,
            )?));
        }
    }
}

fn table_name(table: &str) -> (&str, &str) {
    table.split_once('.').unwrap_or(("public", table))
}
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
