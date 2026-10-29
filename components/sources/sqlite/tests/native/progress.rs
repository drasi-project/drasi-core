// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use tokio::sync::watch;

struct Boundary {
    owner: Arc<QuerySourceProgress>,
    revoked: watch::Sender<bool>,
}

impl Boundary {
    fn new(owner: Arc<QuerySourceProgress>) -> Arc<Self> {
        Arc::new(Self {
            owner,
            revoked: watch::channel(false).0,
        })
    }

    fn reader(self: &Arc<Self>) -> SourceProgressReader {
        SourceProgressReader::External(self.clone())
    }
}

fn check(revoked: bool) -> Result<()> {
    if revoked {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "progress capability revoked",
        )
        .into());
    }
    Ok(())
}

impl SourceProgressProvider for Boundary {
    fn graph_id(&self) -> &str {
        self.owner.graph_id()
    }

    fn component_id(&self) -> &ComponentId {
        self.owner.component_id()
    }

    fn snapshot(&self) -> Result<Arc<SourceProgressSnapshot>> {
        check(*self.revoked.borrow())?;
        Ok(self.owner.snapshot())
    }

    fn subscribe(&self) -> Result<Box<dyn SourceProgressUpdates>> {
        check(*self.revoked.borrow())?;
        Ok(Box::new(Updates {
            progress: self.owner.subscribe(),
            revoked: self.revoked.subscribe(),
        }))
    }
}

struct Updates {
    progress: watch::Receiver<Arc<SourceProgressSnapshot>>,
    revoked: watch::Receiver<bool>,
}

#[async_trait]
impl SourceProgressUpdates for Updates {
    fn snapshot(&mut self) -> Result<Arc<SourceProgressSnapshot>> {
        check(*self.revoked.borrow())?;
        Ok(self.progress.borrow_and_update().clone())
    }

    async fn changed(&mut self) -> Result<()> {
        tokio::select! {
            biased;
            revoked = self.revoked.wait_for(|revoked| *revoked) => {
                drop(revoked?);
            }
            changed = self.progress.changed() => {
                changed?;
            }
        }
        check(*self.revoked.borrow())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn graph_recovery_rejects_external_and_same_named_substitute_readers() -> Result<()> {
    for kind in ["local", "substitute", "external"] {
        let directory = tempfile::tempdir()?;
        let owner = super::progress();
        let reader = match kind {
            "local" => SourceProgressReader::from(owner.clone()),
            "substitute" => SourceProgressReader::from(super::progress()),
            "external" => Boundary::new(owner.clone()).reader(),
            _ => unreachable!(),
        };
        let producer = SqliteSource::new_replayable(
            id("source"),
            stream("changes"),
            config(directory.path()),
            reader,
        )?;
        let consumer = query(directory.path(), owner).await?;
        let (sink, _received) = drasi_reaction_application::NativeApplicationReaction::channel(
            "sink",
            QueryChangeCodec::schema().descriptor().clone(),
            size(1),
        )?;
        let endpoint = |component, port| {
            Endpoint::new(id(component), PortId::try_new(port).expect("test port"))
        };
        let graph = ComputationGraph::builder("native-sqlite")
            .source(Box::new(producer))
            .query(Box::new(consumer))
            .sink(Box::new(sink))
            .bind_stream(endpoint("source", "out"), stream("changes"))
            .bind_stream(endpoint("query", "out"), stream("results"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()?;
        let report = graph.recovery_report(&RecoveryRequirement {
            consumer: id("query"),
            scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
            guarantees: std::collections::BTreeSet::from([RecoveryGuarantee::Replay]),
        })?;
        assert_eq!(report.satisfied(), kind == "local", "{kind}: {report:?}");
        assert_eq!(
            report
                .issues
                .iter()
                .any(|issue| issue.reason == RecoveryIncompatibility::MismatchedProgressResource),
            kind != "local",
            "{kind}: {report:?}"
        );
        assert!(!directory.path().join("source.db").exists());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn external_reader_initializes_and_replays_without_fabricating_local_ownership() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    rusqlite::Connection::open(directory.path().join("source.db"))?.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
         INSERT INTO items VALUES(1,'initial')",
    )?;
    let owner = super::progress();
    let boundary = Boundary::new(owner.clone());
    let (mut producer, snapshot) = SqliteSource::coordinated(
        id("source"),
        stream("changes"),
        config(directory.path()),
        boundary.reader(),
    )?;
    assert!(producer.recovery_progress().is_none());
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    assert_eq!(
        rows(&consumer),
        [serde_json::json!({"id":1,"value":"initial"})]
    );
    producer.start().await?;
    producer
        .handle()
        .execute_batch(
            "UPDATE items SET value='final' WHERE id=1; INSERT INTO items VALUES(2,'second')",
        )
        .await?;
    let original = next(&mut producer).await?;
    producer.stop().await?;
    consumer.stop().await?;
    drop(producer);
    drop(consumer);
    drop(boundary);
    drop(owner);

    let owner = super::progress();
    let boundary = Boundary::new(owner.clone());
    let (mut producer, snapshot) = SqliteSource::coordinated(
        id("source"),
        stream("changes"),
        config(directory.path()),
        boundary.reader(),
    )?;
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    assert_eq!(
        rows(&consumer),
        [serde_json::json!({"id":1,"value":"initial"})]
    );
    producer.start().await?;
    let replay = next(&mut producer).await?;
    assert_eq!(
        original.envelope.system().source_position(),
        replay.envelope.system().source_position()
    );
    apply(&mut consumer, replay).await?;
    assert_eq!(
        rows(&consumer),
        [
            serde_json::json!({"id":1,"value":"final"}),
            serde_json::json!({"id":2,"value":"second"}),
        ]
    );
    producer.handle().query("SELECT id FROM items").await?;
    producer.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    consumer.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn revoked_external_reader_fails_before_sqlite_open_and_retains_uncommitted_input(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner = super::progress();
    let mut consumer = query(directory.path(), owner.clone()).await?;
    consumer.start().await?;
    let boundary = Boundary::new(owner.clone());
    boundary.revoked.send_replace(true);
    let mut producer = SqliteSource::new_replayable(
        id("source"),
        stream("changes"),
        config(directory.path()),
        boundary.reader(),
    )?;
    let error = producer.start().await.expect_err("revoked startup");
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(!directory.path().join("source.db").exists());
    producer.stop().await?;

    boundary.revoked.send_replace(false);
    producer.start().await?;
    producer
        .handle()
        .execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
             INSERT INTO items VALUES(1,'retained')",
        )
        .await?;
    let original = next(&mut producer).await?;
    boundary.revoked.send_replace(true);
    let error = next(&mut producer).await.expect_err("revoked idle reader");
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(owner.snapshot().checkpoints.is_empty());
    producer.stop().await?;
    assert_eq!(journal_state(directory.path())?.0, 1);
    assert_eq!(journal_state(directory.path())?.1, 0);
    drop(producer);

    let mut producer = source(config(directory.path()), owner.clone())?;
    assert!(Arc::ptr_eq(&producer.recovery_progress().unwrap(), &owner));
    producer.start().await?;
    let replay = next(&mut producer).await?;
    assert_eq!(
        original.envelope.system().source_position(),
        replay.envelope.system().source_position()
    );
    apply(&mut consumer, replay).await?;
    assert_eq!(
        rows(&consumer),
        [serde_json::json!({"id":1,"value":"retained"})]
    );
    producer.stop().await?;
    consumer.stop().await?;
    Ok(())
}
