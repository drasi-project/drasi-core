// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::non_atomic_store::SplitOutputProvider;
use super::*;
use drasi_host_sdk::computation::NativeBootstrapResource;

pub(super) const CRASH_PHASE: &str = "DRASI_QUERY_NON_ATOMIC_CRASH_PHASE";

fn split_query(
    database: &Path,
    provider: Arc<SplitOutputProvider>,
    generation: u64,
    recovery: &str,
) -> Result<NativeComponentProxy> {
    let mut query = factory()?.create_component_with_processing_publication(
        component_id()?,
        json!({
            "query": super::bootstrap::TEXT,
            "publication": "non_atomic",
            "bootstrap": true,
            "recovery": recovery,
        }),
        scope(generation),
        provider,
        None,
        QueryPublicationMode::NonAtomic,
    )?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(
        super::bootstrap::file_provider(database, generation)?,
    )))?;
    Ok(query)
}

fn live_input() -> Result<InputEnvelope> {
    input(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "live"),
                    labels: Arc::from([Arc::from("items")]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(json!({"amount": 7})),
            },
        },
        1,
    )
}

fn query(path: &Path, generation: u64) -> Result<NativeComponentProxy> {
    factory()?.create_component_with_processing_publication(
        component_id()?,
        json!({"query": QUERY, "publication": "non_atomic"}),
        scope(generation),
        provider(path)?,
        None,
        QueryPublicationMode::NonAtomic,
    )
}

async fn restart_and_delete() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = input(
        SourceChange::Insert {
            element: item(5, 1),
        },
        1,
    )?;
    let mut first = query(directory.path(), 1)?;
    first.initialize().await?;
    first.start().await?;
    assert!(first.continue_transform().await?.is_empty());
    let pending = first.transform(original.clone()).await?;
    aggregate(&pending, Some(0.0), 5.0, 1)?;
    let progress = first
        .committed_input_progress()?
        .context("actual input progress")?;
    assert_eq!(
        progress.snapshot()?.checkpoints
            [&SourceProgressKey::Stream(StreamId::try_new("source/out")?)]
            .sequence,
        1,
    );
    first.stop().await?;
    drop(first);

    let mut reopened = query(directory.path(), 2)?;
    reopened.initialize().await?;
    reopened.start().await?;
    let replayed = reopened.continue_transform().await?;
    aggregate(&replayed, Some(0.0), 5.0, 1)?;
    assert!(replayed[0].envelope.system().sequence() > pending[0].envelope.system().sequence());
    reopened.delivery_completed(&replayed).await?;
    assert!(reopened.transform(original).await?.is_empty());
    let changed = reopened
        .transform(input(
            SourceChange::Replace {
                element: item(9, 2),
            },
            2,
        )?)
        .await?;
    aggregate(&changed, Some(5.0), 9.0, 2)?;
    reopened.delivery_completed(&changed).await?;
    let readers = reopened.output_readers();
    let output = readers.ports()[&PortId::try_new("out")?]
        .query()
        .context("query reader")?
        .clone();
    let (snapshot, generation) = output.query_snapshot().await?;
    assert_eq!(snapshot.cursor().sequence(), 2);
    assert_eq!(generation, 0);
    assert_eq!(snapshot.records().len(), 1);
    reopened.stop().await?;
    reopened.delete_owned_resources().await?;
    drop(output);
    drop(readers);
    drop(reopened);

    let mut fresh = query(directory.path(), 3)?;
    fresh.initialize().await?;
    fresh.start().await?;
    assert!(fresh.continue_transform().await?.is_empty());
    let readers = fresh.output_readers();
    let (snapshot, generation) = readers.ports()[&PortId::try_new("out")?]
        .query()
        .context("recreated query reader")?
        .query_snapshot()
        .await?;
    assert_eq!(snapshot.cursor().sequence(), 0);
    assert!(snapshot.records().is_empty());
    assert_eq!(
        generation, 1,
        "deletion must advance the numeric output generation"
    );
    fresh.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_query_restarts_replays_and_deletes_current_thread() -> Result<()> {
    restart_and_delete().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_atomic_query_restarts_replays_and_deletes_multi_thread() -> Result<()> {
    restart_and_delete().await
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_query_validates_input_before_marking_evaluation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut first = query(directory.path(), 1)?;
    first.initialize().await?;
    first.start().await?;
    let mut invalid = input(
        SourceChange::Insert {
            element: item(5, 1),
        },
        1,
    )?;
    let producer = GraphProducerIdentity::new(
        "input".into(),
        "upstream".into(),
        ComponentId::try_new("0539efe2-20f1-5267-bc33-53d354d01e65")?,
        StreamId::try_new("source/out")?,
        false,
    )?;
    GraphProducerProgress::annotate(&mut invalid.envelope, &producer, 1)?;
    assert!(first.transform(invalid).await.is_err());
    first.stop().await?;
    drop(first);
    let mut reopened = query(directory.path(), 2)?;
    reopened.initialize().await?;
    reopened.start().await?;
    assert!(reopened.continue_transform().await?.is_empty());
    let outputs = reopened
        .transform(input(
            SourceChange::Insert {
                element: item(7, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&outputs, Some(0.0), 7.0, 1)?;
    reopened.delivery_completed(&outputs).await?;
    reopened.stop().await?;
    Ok(())
}

async fn independent_output_recovery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    crate::bootstrap::provider::create_database(&database)?;
    let provider = Arc::new(SplitOutputProvider::new(&directory.path().join("query"))?);
    let create =
        |generation, recovery| split_query(&database, provider.clone(), generation, recovery);
    let mut first = create(1, "strict")?;
    first.initialize().await?;
    first.start().await?;
    assert!(first.continue_transform().await?.is_empty());
    let progress = first
        .committed_input_progress()?
        .context("actual input progress")?;
    provider.fail_after_next_output_commit();
    let failed = first
        .transform(live_input()?)
        .await
        .expect_err("independent publication failure");
    assert!(format!("{failed:#}").contains("injected failure after independent output commit"));
    assert_eq!(provider.independently_committed_output().await?, Some(1));
    let marker = provider.stored_publication().await?;
    assert_eq!(
        marker.sequence, 1,
        "evaluation must remain distinguishable from published output"
    );
    assert!(marker.source_position.is_none());
    assert!(
        progress.snapshot()?.checkpoints.is_empty(),
        "failed output must not publish input progress"
    );
    assert!(progress.snapshot()?.failure.is_some());
    assert!(first.output_readers().ports()[&PortId::try_new("out")?]
        .snapshot()
        .context("snapshot")?
        .snapshot()
        .await
        .is_err());
    first.stop().await?;
    drop(first);

    let mut strict = create(2, "strict")?;
    strict.initialize().await?;
    let failed = strict
        .start()
        .await
        .expect_err("partial publication cannot be adopted");
    assert!(format!("{failed:#}").contains("publication is incomplete"));
    strict.stop().await?;
    drop(strict);

    let mut recovered = create(3, "auto_reset")?;
    recovered.initialize().await?;
    recovered.start().await?;
    assert!(recovered.continue_transform().await?.is_empty());
    let readers = recovered.output_readers();
    let (snapshot, generation) = readers.ports()[&PortId::try_new("out")?]
        .query()
        .context("recovered query")?
        .query_snapshot()
        .await?;
    assert_eq!(super::bootstrap::total(&snapshot)?, 30.0);
    assert_eq!(generation, 1);
    assert_eq!(
        snapshot.cursor().sequence(),
        1,
        "reset retains the prepared logical position"
    );
    assert_eq!(provider.independently_committed_output().await?, None);
    drop(readers);
    recovered.stop().await?;
    drop(recovered);

    let mut reopened = create(4, "auto_reset")?;
    reopened.initialize().await?;
    reopened.start().await?;
    let readers = reopened.output_readers();
    let (snapshot, generation) = readers.ports()[&PortId::try_new("out")?]
        .query()
        .context("retained query")?
        .query_snapshot()
        .await?;
    assert_eq!(super::bootstrap::total(&snapshot)?, 30.0);
    assert_eq!(
        generation, 1,
        "completed reset must not be repeated on restart"
    );
    drop(readers);
    reopened.stop().await?;
    reopened.delete_owned_resources().await?;
    drop(reopened);
    assert_eq!(
        rusqlite::Connection::open(database)?.query_row(
            "SELECT count(*) FROM items",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2,
        "deletion must not erase the bootstrap source",
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_independent_output_failure_and_auto_reset_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), independent_output_recovery()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_atomic_independent_output_failure_and_auto_reset_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), independent_output_recovery()).await?
}

pub(super) async fn exit_child(root: &Path, phase: &str) -> Result<()> {
    let provider = Arc::new(SplitOutputProvider::new(&root.join("query"))?);
    let database = root.join("source.sqlite");
    match phase {
        "output" => {
            let mut query = split_query(&database, provider.clone(), 1, "strict")?;
            query.initialize().await?;
            query.start().await?;
            provider.exit_after_output_commit();
            query.transform(live_input()?).await?;
        }
        "reset" => {
            provider.exit_after_output_clear();
            let mut query = split_query(&database, provider, 2, "auto_reset")?;
            query.initialize().await?;
            query.start().await?;
        }
        _ => anyhow::bail!("unknown non-atomic crash phase"),
    }
    anyhow::bail!("non-atomic crash hook was not reached")
}

async fn process_exit_recovery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    crate::bootstrap::provider::create_database(&database)?;
    for phase in ["output", "reset"] {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "durable::indexed_commit_exit_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(CRASH_ROOT, directory.path())
                .env(CRASH_PHASE, phase)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("non-atomic child did not reach its commit boundary")??;
        anyhow::ensure!(
            output.status.code() == Some(86),
            "non-atomic {phase} child failed: {output:?}"
        );
        let provider = Arc::new(SplitOutputProvider::new(&directory.path().join("query"))?);
        let mut strict = split_query(&database, provider, 3, "strict")?;
        strict.initialize().await?;
        assert!(
            strict.start().await.is_err(),
            "unfinished {phase} cannot be adopted"
        );
        strict.stop().await?;
    }
    for generation in 4..=5 {
        let provider = Arc::new(SplitOutputProvider::new(&directory.path().join("query"))?);
        let mut recovered = split_query(&database, provider, generation, "auto_reset")?;
        recovered.initialize().await?;
        recovered.start().await?;
        let readers = recovered.output_readers();
        let (snapshot, reset_generation) = readers.ports()[&PortId::try_new("out")?]
            .query()
            .context("recovered query")?
            .query_snapshot()
            .await?;
        assert_eq!(super::bootstrap::total(&snapshot)?, 30.0);
        assert_eq!(
            snapshot.cursor().sequence(),
            1,
            "interrupted reset retains its logical baseline"
        );
        assert_eq!(
            reset_generation, 2,
            "interrupted reset must retain its generation"
        );
        recovered.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_process_exits_preserve_publication_and_reset_intent_current_thread(
) -> Result<()> {
    process_exit_recovery().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_atomic_process_exits_preserve_publication_and_reset_intent_multi_thread() -> Result<()>
{
    process_exit_recovery().await
}
