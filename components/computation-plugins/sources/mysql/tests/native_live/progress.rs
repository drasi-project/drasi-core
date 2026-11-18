// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Progress-only records of the shared row factories: a fresh group's origin
//! is committed before any row, a group mixing fresh and resumed members
//! starts, and quiet selected tables keep committed cursors inside binlog
//! retention.

use crate::lifecycle_ctx;

use super::*;
use anyhow::{Context, Result};
use drasi_core::computation::InMemoryComputationProvider;
use drasi_lib::computation::v1::*;
use drasi_mysql_common::native::rows::RowPosition;
use std::sync::Arc;

const GRAPH: &str = "native-mysql";

fn numeric(value: u128) -> ComponentId {
    ComponentId::try_new(format!("{value:032x}")).unwrap()
}

struct Group {
    database: Database,
    settings: MySqlConfig,
    consumers: Arc<ConsumerProgressSet>,
    queries: Vec<ContinuousQueryTransformer>,
}

impl Group {
    async fn new() -> Result<Self> {
        let database = Database::new(MySqlRetention::Server).await?;
        let mut settings = database.settings.clone();
        settings.output = MySqlOutput::Changes;
        settings.start = MySqlStartPosition::End;
        let owners: Vec<_> = [743, 465]
            .into_iter()
            .map(|id| Arc::new(QuerySourceProgress::new(GRAPH, numeric(id)).unwrap()))
            .collect();
        let consumers = Arc::new(ConsumerProgressSet::new(
            GRAPH,
            numeric(77),
            owners.iter().cloned(),
        )?);
        let mut queries = Vec::new();
        for owner in owners {
            let mut query = ContinuousQueryTransformer::new(
                ContinuousQueryDefinition {
                    graph_id: GRAPH.into(),
                    id: owner.component_id().clone(),
                    query: "MATCH (n:items) RETURN n.id AS id, n.value AS value".into(),
                    language: ComputationQueryLanguage::Cypher,
                    output_stream: StreamId::try_new(format!("{}/out", owner.component_id()))?,
                    outbox_capacity: NonZeroUsize::new(20).unwrap(),
                },
                Arc::new(InMemoryComputationProvider),
            )
            .await?
            .with_source_progress(owner)?;
            query.start(&lifecycle_ctx::start()).await?;
            queries.push(query);
        }
        Ok(Self {
            database,
            settings,
            consumers,
            queries,
        })
    }

    fn source(&self) -> Result<MySqlSource> {
        MySqlSource::shared_rows(
            numeric(77),
            StreamId::try_new("changes")?,
            self.settings.clone(),
            self.consumers.clone(),
            false,
        )
    }

    /// Applies `output` to the queries at `members`, as their nodes would.
    async fn apply(&mut self, output: &OutputEnvelope, members: &[usize]) -> Result<()> {
        for member in members {
            let results = self.queries[*member]
                .transform(InputEnvelope { execution_context: Default::default(),
                    port: PortId::try_new("in")?,
                    envelope: output.envelope.clone(),
                })
                .await?;
            self.queries[*member].delivery_completed(&results).await?;
        }
        Ok(())
    }

    fn ids(&self, member: usize) -> Result<Vec<i64>> {
        let snapshot = self.queries[member].results().snapshot()?;
        let mut ids = QueryChangeCodec::legacy_snapshot_rows(&snapshot.projector, snapshot.rows)?
            .map(|(_, row)| row["id"].as_i64().context("id"))
            .collect::<Result<Vec<_>>>()?;
        ids.sort_unstable();
        Ok(ids)
    }

    async fn finish(mut self) -> Result<()> {
        for query in &mut self.queries {
            query.stop(&lifecycle_ctx::stop()).await?;
        }
        self.database.admin.close().await
    }
}

fn position(output: &OutputEnvelope) -> Result<RowPosition> {
    RowPosition::decode(
        output
            .envelope
            .system()
            .source_position()
            .context("row position")?,
    )
}

fn progress(output: &OutputEnvelope) -> Result<RowPosition> {
    anyhow::ensure!(
        output.envelope.changes().operations().is_empty(),
        "expected a progress record"
    );
    position(output)
}

async fn quiet(source: &mut MySqlSource) -> bool {
    tokio::time::timeout(Duration::from_millis(300), source.next())
        .await
        .is_err()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_rows_commit_a_fresh_origin_first_and_resume_it_with_a_new_source_object(
) -> Result<()> {
    let mut group = Group::new().await?;
    let mut source = group.source()?;
    source.start(&lifecycle_ctx::start()).await?;
    let origin = next(&mut source).await?;
    assert_eq!(progress(&origin)?.position.version, 3, "an origin boundary");
    group
        .database
        .admin
        .execute("INSERT INTO items VALUES (1,'held')")
        .await?;
    assert!(
        quiet(&mut source).await,
        "no row is published before every member committed the origin"
    );
    group.apply(&origin, &[0, 1]).await?;
    assert_eq!(position(&next(&mut source).await?)?.offset, Some(0));
    // The process exits before any member commits a row.
    source.stop(&lifecycle_ctx::stop()).await?;
    drop(source);
    group
        .database
        .admin
        .execute("INSERT INTO items VALUES (2,'offline')")
        .await?;
    let mut source = group.source()?;
    source.start(&lifecycle_ctx::start()).await?;
    for _ in 0..2 {
        let row = next(&mut source).await?;
        group.apply(&row, &[0, 1]).await?;
    }
    assert!(quiet(&mut source).await);
    assert_eq!(group.ids(0)?, [1, 2], "the end was not re-resolved");
    assert_eq!(group.ids(1)?, [1, 2]);
    source.stop(&lifecycle_ctx::stop()).await?;
    group.finish().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_rows_restart_after_a_crash_between_the_members_first_commits() -> Result<()> {
    let mut group = Group::new().await?;
    let mut source = group.source()?;
    source.start(&lifecycle_ctx::start()).await?;
    let origin = next(&mut source).await?;
    progress(&origin)?;
    // The first member commits the origin; the process dies before the
    // second does.
    group.apply(&origin, &[0]).await?;
    source.stop(&lifecycle_ctx::stop()).await?;
    drop(source);
    group
        .database
        .admin
        .execute("INSERT INTO items VALUES (3,'after')")
        .await?;
    let mut source = group.source()?;
    source.start(&lifecycle_ctx::start()).await?;
    let row = next(&mut source).await?;
    assert_eq!(position(&row)?.offset, Some(0), "no second origin");
    group.apply(&row, &[0, 1]).await?;
    assert!(quiet(&mut source).await, "exactly the uncommitted range");
    assert_eq!(group.ids(0)?, [3]);
    assert_eq!(group.ids(1)?, [3]);
    source.stop(&lifecycle_ctx::stop()).await?;
    group.finish().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_rows_idle_selected_tables_commit_progress_through_binlog_purges() -> Result<()> {
    let mut group = Group::new().await?;
    group
        .database
        .admin
        .execute("CREATE TABLE other(id INT PRIMARY KEY)")
        .await?;
    let mut source = group.source()?;
    source.set_idle_progress(Duration::ZERO);
    source.start(&lifecycle_ctx::start()).await?;
    let origin = next(&mut source).await?;
    let origin_file = progress(&origin)?.position.file;
    group.apply(&origin, &[0, 1]).await?;
    // Only an unselected table changes, in a later binlog file.
    group.database.admin.execute("FLUSH BINARY LOGS").await?;
    group
        .database
        .admin
        .execute("INSERT INTO other VALUES (1)")
        .await?;
    let idle = next(&mut source).await?;
    let committed = progress(&idle)?;
    assert_eq!(committed.position.version, 1, "a transaction end");
    assert_ne!(committed.position.file, origin_file);
    group.apply(&idle, &[0, 1]).await?;
    source.stop(&lifecycle_ctx::stop()).await?;
    drop(source);
    group
        .database
        .admin
        .execute(&format!(
            "PURGE BINARY LOGS TO '{}'",
            committed.position.file
        ))
        .await?;
    let logs = group.database.admin.query("SHOW BINARY LOGS").await?;
    anyhow::ensure!(
        logs.values
            .iter()
            .all(|row| catalog::field(row, 0).is_ok_and(|file| file != origin_file)),
        "the origin's binlog file must be purged"
    );
    group
        .database
        .admin
        .execute("INSERT INTO items VALUES (4,'after purge')")
        .await?;
    let mut source = group.source()?;
    source.start(&lifecycle_ctx::start()).await?;
    let row = next(&mut source).await?;
    group.apply(&row, &[0, 1]).await?;
    assert_eq!(group.ids(0)?, [4]);
    source.stop(&lifecycle_ctx::stop()).await?;
    group.finish().await
}
