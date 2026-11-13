// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::num::NonZeroUsize;

pub(super) async fn observation(
    query: &NativeComponentProxy,
) -> Result<drasi_computation_contracts::QueryOutputObservation> {
    query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.query())
        .context("canonical query observations")?
        .query_observation()
        .await
        .map_err(Into::into)
}

async fn follow(
    query: &NativeComponentProxy,
    capacity: usize,
) -> Result<(OutputSnapshot, Box<dyn OutputFollow>)> {
    let readers = query.output_readers();
    Ok(readers
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.snapshot_follow())
        .context("loaded query does not expose the shared snapshot/follow provider")?
        .snapshot_and_follow(OutputFollowLimits {
            max_envelopes: NonZeroUsize::new(capacity).context("positive follow capacity")?,
            max_bytes: NonZeroUsize::new(1 << 20).expect("positive byte capacity"),
            lifetime: Duration::from_secs(5),
        })
        .await?
        .into_parts())
}

async fn shared_output_lifecycle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let first = query
        .transform(input(
            SourceChange::Insert {
                element: item(10, 1),
            },
            1,
        )?)
        .await?;
    aggregate(&first, Some(0.0), 10.0, 1)?;
    query.stop().await?;
    drop(query);

    let mut query = create(directory.path(), QUERY, 2)?;
    query.initialize().await?;
    query.start().await?;
    let before_replay = observation(&query).await?;
    let replayed = query.continue_transform().await?;
    query.delivery_completed(&replayed).await?;
    let after_replay = observation(&query).await?;
    assert_eq!(after_replay.cursor, before_replay.cursor);
    assert_eq!(after_replay.generation, before_replay.generation);
    assert_eq!(after_replay.live_results, 1);
    assert_eq!(after_replay.outbox_size, 1);
    assert_eq!(after_replay.oldest_sequence, Some(1));
    let (snapshot, mut tail) = follow(&query, 1).await?;
    assert_eq!(snapshot.cursor().sequence(), 1);
    assert_eq!(snapshot.records().len(), 1);
    assert!(tokio::time::timeout(Duration::from_millis(20), tail.next())
        .await
        .is_err());
    for (sequence, value, before) in [(2, 12, 10.0), (3, 15, 12.0)] {
        let output = query
            .transform(input(
                SourceChange::Replace {
                    element: item(value, sequence),
                },
                sequence,
            )?)
            .await?;
        assert!(output[0].envelope.system().sequence() > sequence);
        query.delivery_completed(&output).await?;
        let followed = tokio::time::timeout(Duration::from_secs(1), tail.next()).await??;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: followed.clone(),
            }],
            Some(before),
            value as f64,
            sequence,
        )?;
        assert_eq!(
            GraphProducerProgress::from_envelope(&followed)?
                .context("followed producer")?
                .identity(),
            snapshot.cursor().producer()
        );
    }
    query.stop().await?;
    assert!(tokio::time::timeout(Duration::from_secs(1), tail.next())
        .await?
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_output_follow_survives_replay_and_fences_stop_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), shared_output_lifecycle()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_query_output_follow_survives_replay_and_fences_stop_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), shared_output_lifecycle()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_output_reports_slow_followers_without_failing_processing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(directory.path(), QUERY, 1)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let (_, mut tail) = follow(&query, 1).await?;
    for sequence in 1..=2 {
        let output = query
            .transform(input(
                SourceChange::Insert {
                    element: item_with_id(&sequence.to_string(), sequence as i64, sequence),
                },
                sequence,
            )?)
            .await?;
        query.delivery_completed(&output).await?;
    }
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), tail.next()).await?,
        Err(OutputReadError::Gap { .. })
    ));
    let (snapshot, _) = follow(&query, 1).await?;
    assert_eq!(snapshot.cursor().sequence(), 2);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_output_history_reopens_and_capacity_is_operational() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let factory = factory()?;
    let create = |capacity, generation| {
        factory.create_component_with_processing(
            component_id()?,
            json!({"query":QUERY, "outbox_capacity":capacity}),
            scope(generation),
            provider(directory.path())?,
        )
    };
    let mut query = create(4, 1)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    for sequence in 1..=6 {
        let output = query
            .transform(input(
                SourceChange::Insert {
                    element: item(sequence as i64, sequence),
                },
                sequence,
            )?)
            .await?;
        query.delivery_completed(&output).await?;
    }
    let readers = query.output_readers();
    let replay = readers.ports()[&PortId::try_new("out")?]
        .replay()
        .context("shared replay reader")?;
    let head = replay.committed_head().await?;
    let observed = observation(&query).await?;
    assert_eq!(observed.cursor, head);
    assert_eq!(observed.live_results, 1);
    assert_eq!(observed.outbox_size, 4);
    assert_eq!(observed.oldest_sequence, Some(3));
    let history = replay.replay_after(&head.at_sequence(2)).await?;
    assert_eq!(
        history
            .envelopes()
            .iter()
            .map(QueryChangeCodec::query_sequence)
            .collect::<Result<Vec<_>, _>>()?,
        vec![3, 4, 5, 6]
    );
    assert!(matches!(
        replay.replay_after(&head.at_sequence(1)).await,
        Err(OutputReadError::Gap {
            oldest: Some(3),
            latest: 6,
            ..
        })
    ));
    query.stop().await?;
    drop(query);

    let mut query = create(6, 2)?;
    query.initialize().await?;
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let readers = query.output_readers();
    let replay = readers.ports()[&PortId::try_new("out")?]
        .replay()
        .context("reopened shared replay reader")?;
    assert_eq!(replay.committed_head().await?, head);
    let restored = observation(&query).await?;
    assert_eq!(restored.cursor, head);
    assert_eq!(restored.live_results, 1);
    assert_eq!(restored.outbox_size, 4);
    assert_eq!(restored.oldest_sequence, Some(3));
    assert_eq!(
        replay
            .replay_after(&head.at_sequence(2))
            .await?
            .envelopes()
            .len(),
        4
    );
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(7, 7),
            },
            7,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let advanced = observation(&query).await?;
    assert_eq!(advanced.cursor.sequence(), 7);
    assert_eq!(advanced.outbox_size, 5);
    assert_eq!(advanced.oldest_sequence, Some(3));
    assert_eq!(
        replay
            .replay_after(&head.at_sequence(2))
            .await?
            .envelopes()
            .len(),
        5
    );
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_query_definition_scopes_same_named_results_and_preserves_labels() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut datasets = Vec::new();
    for instance in ["first", "second"] {
        let id = ComponentId::try_new("966a0c5c-4c70-51da-abaf-2b6b3d34be10")?;
        let mut query = factory()?.create_component_with_processing(
            id.clone(),
            json!({
                "query":"MATCH (n:Item) RETURN n.value AS value",
                "stream":"selected/out", "result":{"label":"SelectedRow"}
            }),
            Scope {
                instance_id: instance.into(),
                graph_id: "shared-graph".into(),
                generation: 1,
            },
            provider(&directory.path().join(instance))?,
        )?;
        query.initialize().await?;
        query.start().await?;
        assert!(query.continue_transform().await?.is_empty());
        let output = query
            .transform(input(
                SourceChange::Insert {
                    element: item(1, 1),
                },
                1,
            )?)
            .await?;
        let projection = QueryChangeCodec::projection(&output[0].envelope)?;
        assert_eq!(projection.projector().label(), "SelectedRow");
        assert_eq!(
            projection.projector().dataset(),
            drasi_computation_contracts::output_identity::dataset_namespace(
                instance,
                "shared-graph",
                &id,
                &PortId::try_new("out")?,
            )?
        );
        assert_eq!(
            output[0].envelope.system().stream().as_str(),
            "selected/out"
        );
        datasets.push(projection.projector().dataset().to_owned());
        query.delivery_completed(&output).await?;
        query.stop().await?;
    }
    assert_ne!(datasets[0], datasets[1]);
    Ok(())
}
