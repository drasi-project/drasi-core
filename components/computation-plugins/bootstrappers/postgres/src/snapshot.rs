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

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ChangeEnvelope, ComponentId,
    ComputationBootstrapSnapshot, GraphChangeCodec, SourceProgressKey, SourceProgressView,
    StreamId,
};
use drasi_computation_plugin_sdk::{NativeBootstrapProvider, Scope};
use drasi_core_types::{interface::FailureMode, models::SourceChange};
use drasi_postgres_common::replication::rows::{row_checkpoint, RowPosition};
use drasi_postgres_common::{
    connection::{format_lsn, parse_lsn},
    replication::{
        checkpoint, connect, field, read_position, row_element, source_binding,
        validate_managed_slot, validate_retention, Connection, Position, PostgresTransactionConfig,
        TableKeys,
    },
    types::{ColumnInfo, PostgresValue, RelationInfo, ReplicaIdentity},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handover {
    version: u32,
    slot: String,
    binding: [u8; 32],
    boundary: Option<u64>,
    completed: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    rows: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Query-owned durable state authorizes only this provider's UUID-suffixed slots.
/// The committed watermark transfers the slot to the separately hosted source.
pub struct PostgresSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressView,
    handover: Arc<Mutex<Option<Handover>>>,
    pub(crate) query_scope: Option<(Scope, ComponentId)>,
    rows: bool,
}

impl PostgresSnapshot {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
    ) -> Result<Self> {
        config.validate_coordinated()?;
        Ok(Self {
            source,
            stream,
            config,
            progress,
            handover: Arc::new(Mutex::new(None)),
            query_scope: None,
            rows: false,
        })
    }

    pub fn rows(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
    ) -> Result<Self> {
        let mut provider = Self::new(source, stream, config, progress)?;
        provider.rows = true;
        Ok(provider)
    }

    fn current(&self) -> Result<Option<Handover>> {
        Ok(self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))?
            .clone())
    }

    fn config_for(&self, handover: &Handover) -> Result<PostgresTransactionConfig> {
        validate_managed_slot(&self.config.connection.slot_name, &handover.slot)?;
        anyhow::ensure!(
            handover.version == 1
                && handover.rows == self.rows
                && (!handover.completed || handover.boundary.is_some_and(|lsn| lsn > 0)),
            "invalid PostgreSQL handover state"
        );
        let mut config = self.config.clone();
        config.connection.slot_name = handover.slot.clone();
        config.start_lsn = format_lsn(handover.boundary.unwrap_or(0));
        Ok(config)
    }

    async fn begin(&self, state: &dyn BootstrapState) -> Result<ComputationBootstrapSnapshot> {
        let mut connection = connect(&self.config).await?;
        if let Some(previous) = self.current()? {
            anyhow::ensure!(!previous.completed, "completed PostgreSQL initialization requires explicit retirement, not automatic replacement");
            let config = self.config_for(&previous)?;
            let (_, binding) =
                source_binding(&mut connection, &config, &self.source, &self.stream).await?;
            anyhow::ensure!(
                binding == previous.binding,
                "unfinished PostgreSQL slot binding changed"
            );
            let rows = connection.query_rows(
                format!("SELECT plugin, database, temporary, active FROM pg_replication_slots WHERE slot_name = '{}'", previous.slot),
                config.max_protocol_bytes,
            ).await?;
            if let [row] = rows.as_slice() {
                anyhow::ensure!(
                    field(row, 0)? == "pgoutput"
                        && field(row, 1)? == config.connection.database
                        && field(row, 2)? == "f"
                        && field(row, 3)? == "f",
                    "unfinished PostgreSQL slot is incompatible or still active"
                );
                connection
                    .query_rows(
                        format!("DROP_REPLICATION_SLOT {}", previous.slot),
                        config.max_protocol_bytes,
                    )
                    .await?;
            } else {
                anyhow::ensure!(rows.is_empty(), "duplicate PostgreSQL slot identity");
            }
        }
        let slot = format!(
            "{}_{}",
            self.config.connection.slot_name,
            uuid::Uuid::new_v4().simple()
        );
        let mut config = self.config.clone();
        config.connection.slot_name = slot.clone();
        let (keys, binding) =
            source_binding(&mut connection, &config, &self.source, &self.stream).await?;
        validate_snapshot_publication(&mut connection, &config).await?;
        let mut handover = Handover {
            version: 1,
            slot,
            binding,
            boundary: None,
            completed: false,
            rows: self.rows,
        };
        // Live changes use PostgreSQL commit time. Sampling that same clock
        // before the snapshot boundary prevents host clock skew from making
        // subsequent WAL updates/deletes appear older than the initial rows.
        let timestamp = snapshot_timestamp(&mut connection, &config).await?;
        state
            .write(Bytes::from(serde_json::to_vec(&handover)?))
            .await?;
        *self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))? = Some(handover.clone());

        // No further command may use this connection before the second session
        // imports the exported snapshot: another command invalidates its name.
        let (boundary, snapshot) =
            export_snapshot(&mut connection, &config, &handover.slot, false).await?;
        let mut reader = connect(&config).await?;
        reader.query_rows(
            format!("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT '{snapshot}'"),
            config.max_protocol_bytes,
        ).await?;
        drop(connection);
        handover.boundary = Some(boundary);
        let mut reader = SnapshotReader::new(
            reader,
            config,
            keys,
            timestamp,
            self.source.clone(),
            self.stream.clone(),
        )
        .await?;
        reader.completed = Some((handover, self.handover.clone()));
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    reader
                        .next()
                        .await
                        .map(|item| item.map(|item| (item, reader)))
                },
            )),
            watermarks: Vec::new(),
        })
    }
}

#[async_trait]
impl NativeBootstrapProvider for PostgresSnapshot {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        Some(self.progress.clone())
    }

    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> Result<()> {
        anyhow::ensure!(
            self.progress.graph_id() == graph_id && self.progress.component_id() == component_id,
            "PostgreSQL snapshot belongs to another query"
        );
        if let Some((scope, query)) = &self.query_scope {
            anyhow::ensure!(
                scope.instance_id == instance_id
                    && scope.graph_id == graph_id
                    && query == component_id,
                "PostgreSQL snapshot query scope changed"
            );
        }
        Ok(())
    }

    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        state.durability().require(FailureMode::ProcessRestart)?;
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered && progress.persistent && progress.failure.is_none(),
            "PostgreSQL handover requires recovered persistent owner progress"
        );
        let handover = state
            .read()
            .await?
            .map(|bytes| serde_json::from_slice::<Handover>(&bytes))
            .transpose()?;
        if let Some(handover) = &handover {
            self.config_for(handover)?;
            let row_position = if self.rows {
                row_checkpoint(&progress, &self.source, handover.binding)?
            } else {
                None
            };
            let position = if self.rows {
                row_position.as_ref().map(|position| position.commit_lsn)
            } else {
                checkpoint(&progress, &self.source, handover.binding)?
            };
            anyhow::ensure!(
                handover.completed == position.is_some(),
                "PostgreSQL handover and committed owner position disagree; explicit recovery required"
            );
            if let Some(position) = position {
                anyhow::ensure!(
                    position >= handover.boundary.context("missing snapshot boundary")?
                        && if self.rows {
                            row_position.and_then(|position| position.managed_slot)
                        } else {
                            read_position(&progress, &self.source)?
                                .and_then(|position| position.managed_slot)
                        }
                        .as_deref()
                            == Some(handover.slot.as_str()),
                    "PostgreSQL committed position changed its managed slot or snapshot boundary"
                );
            }
        } else {
            anyhow::ensure!(
                !progress.bootstrap_complete
                    && !progress
                        .checkpoints
                        .contains_key(&SourceProgressKey::Source(self.source.to_string())),
                "PostgreSQL handover ownership state is missing"
            );
        }
        *self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))? = handover;
        Ok(BootstrapPreparation::Ready)
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("PostgreSQL snapshot requires query-owned handover state")
    }

    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        tokio::time::timeout(self.config.timeout(), self.begin(state))
            .await
            .context("PostgreSQL snapshot initialization timed out")?
    }

    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let handover = self.current()?.context("missing PostgreSQL handover")?;
        anyhow::ensure!(
            handover.completed,
            "PostgreSQL snapshot stream did not complete"
        );
        let boundary = handover
            .boundary
            .context("missing PostgreSQL snapshot boundary")?;
        let (sequence, position) = if self.rows {
            (
                0,
                serde_json::to_vec(&RowPosition::snapshot(
                    handover.binding,
                    handover.slot,
                    boundary,
                ))?,
            )
        } else {
            (
                boundary,
                serde_json::to_vec(&Position {
                    version: 1,
                    binding: handover.binding,
                    commit_lsn: boundary,
                    managed_slot: Some(handover.slot),
                })?,
            )
        };
        Ok(vec![BootstrapWatermark {
            producer: None,
            stream: self.stream.clone(),
            source_id: Some(self.source.to_string()),
            sequence,
            position: Some(Bytes::from(position)),
        }])
    }

    fn completion_state(&self) -> Result<Option<Bytes>> {
        let handover = self.current()?.context("missing PostgreSQL handover")?;
        anyhow::ensure!(handover.completed, "PostgreSQL snapshot is incomplete");
        Ok(Some(Bytes::from(serde_json::to_vec(&handover)?)))
    }
}

struct SnapshotTable {
    relation: RelationInfo,
    keys: Vec<String>,
}

pub(super) struct SnapshotReader {
    connection: Connection,
    config: PostgresTransactionConfig,
    tables: VecDeque<SnapshotTable>,
    opened: bool,
    ordinal: u64,
    timestamp: u64,
    source: ComponentId,
    stream: StreamId,
    completed: Option<(Handover, Arc<Mutex<Option<Handover>>>)>,
}

impl SnapshotReader {
    pub(super) async fn new(
        mut connection: Connection,
        config: PostgresTransactionConfig,
        keys: TableKeys,
        timestamp: u64,
        source: ComponentId,
        stream: StreamId,
    ) -> Result<Self> {
        validate_retention(&mut connection, &config).await?;
        let tables = snapshot_tables(&mut connection, &config, keys).await?;
        Ok(Self {
            connection,
            config,
            tables,
            opened: false,
            ordinal: 0,
            timestamp,
            source,
            stream,
            completed: None,
        })
    }

    pub(super) fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub(super) async fn next(&mut self) -> Result<Option<ChangeEnvelope>> {
        loop {
            let Some(table) = self.tables.front() else {
                tokio::time::timeout(
                    self.config.timeout(),
                    self.connection
                        .query_rows("COMMIT".into(), self.config.max_protocol_bytes),
                )
                .await??;
                if let Some((handover, completed)) = &mut self.completed {
                    handover.completed = true;
                    *completed
                        .lock()
                        .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))? =
                        Some(handover.clone());
                }
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
                let statement = format!(
                    "DECLARE drasi_snapshot NO SCROLL CURSOR FOR SELECT {columns} FROM ONLY {}.{}",
                    quote(&table.relation.namespace),
                    quote(&table.relation.name)
                );
                tokio::time::timeout(
                    self.config.timeout(),
                    self.connection
                        .query_rows(statement, self.config.max_protocol_bytes),
                )
                .await??;
                self.opened = true;
            }
            let rows = tokio::time::timeout(
                self.config.timeout(),
                self.connection.query_rows(
                    "FETCH FORWARD 1 FROM drasi_snapshot".into(),
                    self.config.max_protocol_bytes,
                ),
            )
            .await??;
            if rows.is_empty() {
                tokio::time::timeout(
                    self.config.timeout(),
                    self.connection.query_rows(
                        "CLOSE drasi_snapshot".into(),
                        self.config.max_protocol_bytes,
                    ),
                )
                .await??;
                self.tables.pop_front();
                self.opened = false;
                continue;
            }
            let [row] = rows.as_slice() else {
                anyhow::bail!("PostgreSQL cursor returned more than one row");
            };
            anyhow::ensure!(
                row.len() == table.relation.columns.len(),
                "PostgreSQL snapshot column count changed"
            );
            let tuple = row
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
            let element = row_element(
                &self.source,
                &table.relation,
                &table.keys,
                &tuple,
                self.timestamp,
            )?;
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

pub(super) async fn snapshot_timestamp(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<u64> {
    let rows = connection
        .query_rows(
            "SELECT floor(extract(epoch FROM pg_catalog.clock_timestamp()) * 1000)::bigint".into(),
            config.max_protocol_bytes,
        )
        .await?;
    let [row] = rows.as_slice() else {
        anyhow::bail!("PostgreSQL did not return one snapshot timestamp");
    };
    field(row, 0)?
        .parse::<u64>()
        .context("invalid PostgreSQL snapshot timestamp")
}

pub(super) async fn export_snapshot(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    slot: &str,
    temporary: bool,
) -> Result<(u64, String)> {
    let temporary = if temporary { " TEMPORARY" } else { "" };
    let rows = connection
        .query_rows(
            format!("CREATE_REPLICATION_SLOT {slot}{temporary} LOGICAL pgoutput EXPORT_SNAPSHOT"),
            config.max_protocol_bytes,
        )
        .await?;
    let [row] = rows.as_slice() else {
        anyhow::bail!("PostgreSQL did not return one exported snapshot");
    };
    anyhow::ensure!(
        field(row, 0)? == slot && field(row, 3)? == "pgoutput",
        "unexpected exported slot identity"
    );
    let boundary = parse_lsn(&field(row, 1)?)?;
    anyhow::ensure!(boundary > 0, "invalid PostgreSQL snapshot boundary");
    let snapshot = field(row, 2)?;
    anyhow::ensure!(
        !snapshot.is_empty()
            && snapshot
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "invalid PostgreSQL exported snapshot name"
    );
    Ok((boundary, snapshot))
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(super) async fn validate_snapshot_publication(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<()> {
    let publication = config.connection.publication_name.replace('\'', "''");
    let rows = connection.query_rows(
        format!("SELECT current_setting('server_version_num')::int >= 150000, pubinsert, pubupdate, pubdelete, NOT pubviaroot FROM pg_publication WHERE pubname = '{publication}'"),
        config.max_protocol_bytes,
    ).await?;
    let [row] = rows.as_slice() else {
        anyhow::bail!("missing PostgreSQL publication");
    };
    anyhow::ensure!((0..5).all(|index| field(row, index).is_ok_and(|value| value == "t")), "coordinated PostgreSQL snapshots require PostgreSQL 15+ and a complete insert/update/delete publication without partition-root remapping");
    let rows = connection.query_rows(
        format!("SELECT 1 FROM pg_publication_tables p JOIN pg_namespace n ON n.nspname = p.schemaname JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = p.tablename WHERE p.pubname = '{publication}' AND (p.rowfilter IS NOT NULL OR cardinality(p.attnames) <> (SELECT count(*) FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = '')) LIMIT 1"),
        config.max_protocol_bytes,
    ).await?;
    anyhow::ensure!(
        rows.is_empty(),
        "coordinated PostgreSQL snapshots do not support filtered publication rows or columns"
    );
    Ok(())
}

async fn snapshot_tables(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    keys: TableKeys,
) -> Result<VecDeque<SnapshotTable>> {
    let mut tables = VecDeque::new();
    let mut bytes = 0usize;
    for ((namespace, name), keys) in keys {
        connection
            .query_rows(
                format!(
                    "LOCK TABLE {}.{} IN ACCESS SHARE MODE",
                    quote(&namespace),
                    quote(&name)
                ),
                config.max_protocol_bytes,
            )
            .await?;
        let rows = connection.query_rows(
            format!("SELECT a.attname, a.atttypid::text FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '{}' AND c.relname = '{}' AND c.relkind = 'r' AND NOT c.relispartition AND NOT c.relhassubclass AND a.attnum > 0 AND NOT a.attisdropped AND a.attgenerated = '' ORDER BY a.attnum", namespace.replace('\'', "''"), name.replace('\'', "''")),
            config.max_protocol_bytes,
        ).await?;
        anyhow::ensure!(
            !rows.is_empty(),
            "coordinated PostgreSQL snapshots require ordinary non-partitioned tables"
        );
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
            "PostgreSQL snapshot key is incomplete"
        );
        bytes = bytes
            .checked_add(
                std::mem::size_of::<SnapshotTable>()
                    + namespace.len()
                    + name.len()
                    + columns
                        .iter()
                        .map(|column| std::mem::size_of::<ColumnInfo>() + column.name.len())
                        .sum::<usize>()
                    + keys
                        .iter()
                        .map(|key| std::mem::size_of::<String>() + key.len())
                        .sum::<usize>(),
            )
            .context("snapshot metadata overflow")?;
        anyhow::ensure!(
            bytes <= config.max_protocol_bytes.get(),
            "PostgreSQL snapshot metadata exceeds configured byte limit"
        );
        tables.push_back(SnapshotTable {
            relation: RelationInfo {
                id: 0,
                namespace,
                name,
                replica_identity: ReplicaIdentity::Full,
                columns,
            },
            keys,
        });
    }
    Ok(tables)
}

#[cfg(test)]
mod tests;
