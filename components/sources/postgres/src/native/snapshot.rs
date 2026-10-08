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

use super::*;
use crate::types::{ColumnInfo, PostgresValue, RelationInfo, ReplicaIdentity};
use drasi_core::{interface::FailureMode, models::SourceChange};
use drasi_lib::computation::v1::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComputationBootstrapProvider,
    ComputationBootstrapSnapshot, GraphChangeCodec,
};
use std::{collections::VecDeque, sync::Mutex};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handover {
    version: u32,
    slot: String,
    binding: [u8; 32],
    boundary: Option<u64>,
    completed: bool,
}

/// Paired with `PostgresTransactionSource::coordinated` and attached to the same
/// query with `with_bootstrap`. Query-owned durable state authorizes only this
/// provider's UUID-suffixed slots, never a pre-existing externally managed slot.
pub struct PostgresSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressReader,
    handover: Arc<Mutex<Option<Handover>>>,
}

impl PostgresSnapshot {
    pub(super) fn new(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressReader,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.connection.slot_name.len() <= 30,
            "managed PostgreSQL slot prefix must contain at most 30 characters"
        );
        Ok(Self {
            source,
            stream,
            config,
            progress,
            handover: Arc::new(Mutex::new(None)),
        })
    }

    fn current(&self) -> Result<Option<Handover>> {
        Ok(self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))?
            .clone())
    }

    pub(super) fn validate_source(&self, source: &PostgresTransactionSource) -> Result<()> {
        anyhow::ensure!(
            self.source == *source.descriptor.id()
                && self.stream == source.stream
                && self.progress.same_owner(&source.progress)
                && serde_json::to_value(&self.config)? == serde_json::to_value(&source.config)?,
            "PostgreSQL snapshot resource does not match the source and its actual progress owner"
        );
        Ok(())
    }

    fn config_for(&self, handover: &Handover) -> Result<PostgresTransactionConfig> {
        let prefix = format!("{}_", self.config.connection.slot_name);
        let suffix = handover
            .slot
            .strip_prefix(&prefix)
            .context("managed slot prefix changed")?;
        anyhow::ensure!(
            handover.version == 1
                && suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                && (!handover.completed || handover.boundary.is_some_and(|lsn| lsn > 0)),
            "invalid PostgreSQL handover state"
        );
        let mut config = self.config.clone();
        config.connection.slot_name = handover.slot.clone();
        config.start_lsn = crate::connection::format_lsn(handover.boundary.unwrap_or(0));
        Ok(config)
    }

    pub(super) fn live_config(
        &self,
        progress: &SourceProgressSnapshot,
    ) -> Result<PostgresTransactionConfig> {
        let handover = self
            .current()?
            .context("PostgreSQL snapshot has not been prepared")?;
        anyhow::ensure!(
            handover.completed && progress.bootstrap_complete,
            "PostgreSQL snapshot is incomplete"
        );
        let position = checkpoint(progress, &self.source, handover.binding)?
            .context("completed PostgreSQL snapshot has no committed position")?;
        anyhow::ensure!(
            position >= handover.boundary.context("missing snapshot boundary")?,
            "PostgreSQL progress predates its snapshot"
        );
        self.config_for(&handover)
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
        };
        state
            .write(Bytes::from(serde_json::to_vec(&handover)?))
            .await?;
        *self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))? = Some(handover.clone());

        // No further command may use this connection before the second session
        // imports the exported snapshot: another command invalidates its name.
        let rows = connection
            .query_rows(
                format!(
                    "CREATE_REPLICATION_SLOT {} LOGICAL pgoutput EXPORT_SNAPSHOT",
                    handover.slot
                ),
                config.max_protocol_bytes,
            )
            .await?;
        let [row] = rows.as_slice() else {
            anyhow::bail!("PostgreSQL did not return one exported snapshot");
        };
        anyhow::ensure!(
            field(row, 0)? == handover.slot && field(row, 3)? == "pgoutput",
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
        let mut reader = connect(&config).await?;
        reader.query_rows(
            format!("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT '{snapshot}'"),
            config.max_protocol_bytes,
        ).await?;
        drop(connection);
        validate_retention(&mut reader, &config).await?;
        let tables = snapshot_tables(&mut reader, &config, keys).await?;
        handover.boundary = Some(boundary);
        let reader = SnapshotReader {
            connection: reader,
            config,
            tables,
            opened: false,
            ordinal: 0,
            timestamp: chrono::Utc::now().timestamp_millis().try_into()?,
            source: self.source.clone(),
            stream: self.stream.clone(),
            handover,
            completed: self.handover.clone(),
        };
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
impl ComputationBootstrapProvider for PostgresSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        state.durability().require(FailureMode::ProcessRestart)?;
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered && progress.persistent,
            "PostgreSQL handover requires recovered persistent owner progress"
        );
        let handover = state
            .read()
            .await?
            .map(|bytes| serde_json::from_slice::<Handover>(&bytes))
            .transpose()?;
        if let Some(handover) = &handover {
            self.config_for(handover)?;
            let position = checkpoint(&progress, &self.source, handover.binding)?;
            anyhow::ensure!(
                handover.completed == position.is_some(),
                "PostgreSQL handover and committed owner position disagree; explicit recovery required"
            );
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
        Ok(vec![BootstrapWatermark {
            stream: self.stream.clone(),
            source_id: Some(self.source.to_string()),
            sequence: boundary,
            position: Some(Bytes::from(serde_json::to_vec(&Position {
                version: 1,
                binding: handover.binding,
                commit_lsn: boundary,
            })?)),
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

struct SnapshotReader {
    connection: Connection,
    config: PostgresTransactionConfig,
    tables: VecDeque<SnapshotTable>,
    opened: bool,
    ordinal: u64,
    timestamp: u64,
    source: ComponentId,
    stream: StreamId,
    handover: Handover,
    completed: Arc<Mutex<Option<Handover>>>,
}

impl SnapshotReader {
    async fn next(&mut self) -> Result<Option<drasi_lib::computation::v1::ChangeEnvelope>> {
        loop {
            let Some(table) = self.tables.front() else {
                tokio::time::timeout(
                    self.config.timeout(),
                    self.connection
                        .query_rows("COMMIT".into(), self.config.max_protocol_bytes),
                )
                .await??;
                self.handover.completed = true;
                *self
                    .completed
                    .lock()
                    .map_err(|_| anyhow::anyhow!("PostgreSQL handover poisoned"))? =
                    Some(self.handover.clone());
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
            let element = transaction::row_element(
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

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn validate_snapshot_publication(
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
