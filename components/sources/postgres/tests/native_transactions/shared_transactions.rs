// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_postgres_common::replication::shared_transactions::{
    self as cursor, SharedTransactionPosition, TransactionBoundary,
};

fn position(owner: &QuerySourceProgress) -> Result<Option<SharedTransactionPosition>> {
    cursor::read_position(&owner.snapshot(), &id("source"))
}

fn output_count(events: &Events) -> usize {
    events.lock().expect("output log").len()
}

fn sorted_values(results: &QueryResults) -> Result<Vec<Value>> {
    let mut rows = values(results)?;
    rows.sort_by_key(|row| row["id"].as_i64());
    Ok(rows)
}

fn input_position(envelope: &ChangeEnvelope) -> Result<SharedTransactionPosition> {
    let transaction = SourceTransactionCodec::decode(envelope, limits(16))?;
    serde_json::from_slice(transaction.position()).map_err(Into::into)
}

async fn whole_commit_fanout() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let path = tempfile::tempdir()?;
        let graph = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            0,
            false,
            None,
            SharedOutput::Transactions,
        )
        .await?;
        start_graph(&graph).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1,'intermediate');
             UPDATE person SET name='final' WHERE id=1;
             INSERT INTO person VALUES(9,'temporary'); DELETE FROM person WHERE id=9;",
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        for index in 0..2 {
            assert!(graph.results[index].snapshot()?.rows.is_empty());
            assert_eq!(output_count(&graph.accepted[index]), 0);
            assert!(position(&graph.owners[index])?.is_none());
        }
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        database.client().batch_execute("COMMIT").await?;
        wait_until(
            "fast whole transaction and one queued slow transaction",
            || Ok(output_count(&graph.outputs[0]) == 1 && output_count(&graph.accepted[1]) == 1),
        )
        .await?;
        assert_eq!(values(&graph.results[0])?, [json!({"id":1,"name":"final"})]);
        assert!(graph.results[1].snapshot()?.rows.is_empty());
        let first = position(&graph.owners[0])?.context("fast whole-commit cursor")?;
        assert_eq!(first.mode, TransactionBoundary::WholeCommit);
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        for inputs in &graph.accepted {
            let inputs = inputs.lock().expect("accepted transaction log");
            assert_eq!(
                inputs.len(),
                1,
                "one source emission, not one emission per row"
            );
            let transaction = SourceTransactionCodec::decode(&inputs[0], limits(16))?;
            assert_eq!(transaction.into_changes().len(), 4);
            assert_eq!(input_position(&inputs[0])?, first);
        }
        graph.gates[1].add_permits(1);
        wait_until("slow atomic handling", || {
            Ok(output_count(&graph.outputs[1]) == 1)
        })
        .await?;
        assert_eq!(position(&graph.owners[1])?, Some(first.clone()));
        database.wait_feedback(first.commit_lsn).await?;

        database
            .client()
            .batch_execute(
                "BEGIN; UPDATE person SET name='second' WHERE id=1;
             INSERT INTO person VALUES(2,'two'); COMMIT;",
            )
            .await?;
        wait_until("second whole commit held by the slow member", || {
            Ok(output_count(&graph.outputs[0]) == 2 && output_count(&graph.accepted[1]) == 2)
        })
        .await?;
        let second = position(&graph.owners[0])?.context("second commit")?;
        assert!(second.commit_lsn > first.commit_lsn);
        assert_eq!(lsn(&database.feedback().await?), first.commit_lsn);

        let control = graph.core.computation_control()?;
        let revision = control.desired_snapshot().revision;
        let stopped = tokio::time::timeout(
            Duration::from_secs(10),
            control.stop_components(revision, GraphSelection::Exact(vec![id("source")])),
        )
        .await??;
        assert!(matches!(
            stopped.components.get(&id("source")),
            Some(StopOutcome::Stopped)
        ));
        let active: bool = database
            .client()
            .query_one(
                "SELECT active FROM pg_replication_slots WHERE slot_name='native_slot'",
                &[],
            )
            .await?
            .get(0);
        assert!(
            !active,
            "source stop must await its real replication worker"
        );
        let started = control
            .start_components(revision, GraphSelection::Exact(vec![id("source")]))
            .await?;
        assert!(matches!(
            started.components.get(&id("source")),
            Some(StartOutcome::Started)
        ));
        wait_until("whole-commit replay above retained allocations", || {
            Ok(graph
                .accepted
                .iter()
                .all(|events| output_count(events) == 3))
        })
        .await?;
        for inputs in &graph.accepted {
            let inputs = inputs.lock().expect("accepted transaction log");
            let replay = input_position(&inputs[2])?;
            assert_eq!(replay.commit_lsn, second.commit_lsn);
            assert!(replay.sequence > second.sequence);
            assert!(inputs[2].system().sequence() > inputs[1].system().sequence());
        }
        graph.gates[1].add_permits(128);
        wait_until(
            "slow commit and duplicate replay handled independently",
            || {
                Ok(output_count(&graph.inputs[1]) == 3
                    && output_count(&graph.outputs[1]) == 2
                    && graph.owners.iter().all(|owner| {
                        owner
                            .snapshot()
                            .transport_sequences
                            .get(&stream())
                            .is_some_and(|value| *value >= 3)
                    }))
            },
        )
        .await?;
        assert_eq!(
            output_count(&graph.outputs[0]),
            2,
            "fast query must filter physical replay"
        );
        database.wait_feedback(second.commit_lsn).await?;

        database.client().batch_execute(
            "BEGIN; DELETE FROM person WHERE id=1; UPDATE person SET name='two-final' WHERE id=2;
             INSERT INTO person VALUES(3,'three'); COMMIT;",
        ).await?;
        wait_until("insert/update/delete commit at both consumers", || {
            Ok(graph.outputs.iter().all(|events| output_count(events) == 3))
        })
        .await?;
        let expected = [
            json!({"id":2,"name":"two-final"}),
            json!({"id":3,"name":"three"}),
        ];
        let last = position(&graph.owners[0])?.context("final commit")?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(sorted_values(&graph.results[index])?, expected);
            assert!(owner.snapshot().persistent);
            assert_eq!(position(owner)?, Some(last.clone()));
        }
        database.wait_feedback(last.commit_lsn).await?;
        graph.core.shutdown().await?;
        drop(graph);

        // This restart is deliberately drained. It does not claim reconstruction
        // of undrained persistent input pipes from handled checkpoints alone.
        let restarted = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            128,
            false,
            None,
            SharedOutput::Transactions,
        )
        .await?;
        start_graph(&restarted).await?;
        for (index, owner) in restarted.owners.iter().enumerate() {
            assert_eq!(sorted_values(&restarted.results[index])?, expected);
            assert_eq!(position(owner)?, Some(last.clone()));
            assert_eq!(output_count(&restarted.outputs[index]), 0);
        }
        database.client().batch_execute(
            "BEGIN; UPDATE person SET name='restart' WHERE id=2; DELETE FROM person WHERE id=3; COMMIT;",
        ).await?;
        wait_until("drained persistent restart", || {
            Ok(restarted
                .outputs
                .iter()
                .all(|events| output_count(events) == 1))
        })
        .await?;
        for (index, owner) in restarted.owners.iter().enumerate() {
            assert_eq!(
                values(&restarted.results[index])?,
                [json!({"id":2,"name":"restart"})]
            );
            assert!(position(owner)?.context("restarted cursor")?.sequence > last.sequence);
        }
        restarted.core.shutdown().await?;
        drop(restarted);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn whole_commit_current_thread_static_and_dynamic() -> Result<()> {
    whole_commit_fanout().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_commit_multi_thread_static_and_dynamic() -> Result<()> {
    whole_commit_fanout().await
}

async fn different_snapshots() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'before')")
            .await?;
        let original = database.feedback().await?;
        let path = tempfile::tempdir()?;
        let gates = [
            bootstrap::SnapshotGate::new(1, 0),
            bootstrap::SnapshotGate::new(0, 0),
        ];
        let graph = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            1,
            true,
            Some(&gates),
            SharedOutput::Transactions,
        )
        .await?;
        let writes = async {
            let result = async {
                tokio::time::timeout(Duration::from_secs(10), gates[0].opened.notified()).await?;
                database
                    .client()
                    .batch_execute(
                        "BEGIN; UPDATE person SET name='middle' WHERE id=1;
                     INSERT INTO person VALUES(2,'temporary'); DELETE FROM person WHERE id=2;
                     INSERT INTO person VALUES(2,'middle'); COMMIT;",
                    )
                    .await?;
                gates[1].begin.add_permits(1);
                tokio::time::timeout(Duration::from_secs(10), gates[1].opened.notified()).await?;
                database
                    .client()
                    .batch_execute(
                        "BEGIN; UPDATE person SET name='after' WHERE id=1;
                     DELETE FROM person WHERE id=2; INSERT INTO person VALUES(3,'live'); COMMIT;",
                    )
                    .await?;
                assert_eq!(database.feedback().await?, original);
                bootstrap::no_temporary_slots(&database).await?;
                gates[0].copy.add_permits(128);
                wait_until("first independent persistent snapshot", || {
                    Ok(graph.owners[0].snapshot().bootstrap_complete)
                })
                .await?;
                let first = position(&graph.owners[0])?.context("first snapshot boundary")?;
                assert_eq!(first.mode, TransactionBoundary::ConfiguredSnapshot);
                assert_eq!(first.sequence, 0);
                assert!(!graph.owners[1].snapshot().ready);
                assert_eq!(database.feedback().await?, original);
                Result::<SharedTransactionPosition>::Ok(first)
            }
            .await;
            for gate in &gates {
                gate.begin.add_permits(1);
                gate.copy.add_permits(128);
            }
            result
        };
        let (started, first) = tokio::join!(start_graph(&graph), writes);
        let first = first?;
        started?;
        wait_until("newer snapshot skips the first whole commit", || {
            Ok(output_count(&graph.outputs[0]) == 2 && output_count(&graph.inputs[1]) == 1)
        })
        .await?;
        let later = position(&graph.owners[1])?.context("later snapshot boundary")?;
        assert_eq!(later.mode, TransactionBoundary::ConfiguredSnapshot);
        assert_eq!(later.sequence, 0);
        assert_eq!(later.binding, first.binding);
        assert!(later.commit_lsn > first.commit_lsn);
        assert_eq!(output_count(&graph.outputs[1]), 0);
        assert_eq!(
            sorted_values(&graph.results[1])?,
            [
                json!({"id":1,"name":"middle"}),
                json!({"id":2,"name":"middle"})
            ],
        );
        database.wait_feedback(later.commit_lsn).await?;
        let first_live = input_position(&graph.inputs[0].lock().expect("input log")[0])?;
        assert!(first_live.commit_lsn > first.commit_lsn);
        assert!(first_live.commit_lsn <= later.commit_lsn);
        graph.gates[1].add_permits(128);
        wait_until(
            "late snapshot applies only the uncovered whole transaction",
            || Ok(output_count(&graph.outputs[1]) == 1),
        )
        .await?;
        let final_position = position(&graph.owners[0])?.context("latest whole commit")?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(
                sorted_values(&graph.results[index])?,
                [
                    json!({"id":1,"name":"after"}),
                    json!({"id":3,"name":"live"})
                ],
            );
            assert_eq!(position(owner)?, Some(final_position.clone()));
            assert!(owner.snapshot().persistent);
            let inputs = graph.inputs[index].lock().expect("input log");
            assert_eq!(inputs.len(), 2);
            for (envelope, changes) in inputs.iter().zip([4, 3]) {
                assert_eq!(
                    SourceTransactionCodec::decode(envelope, limits(16))?
                        .into_changes()
                        .len(),
                    changes
                );
            }
        }
        database.wait_feedback(final_position.commit_lsn).await?;
        graph.core.shutdown().await?;
        drop(graph);
        bootstrap::no_temporary_slots(&database).await?;
        bootstrap::no_snapshot_transactions(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn configured_snapshots_current_thread_static_and_dynamic() -> Result<()> {
    different_snapshots().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_snapshots_multi_thread_static_and_dynamic() -> Result<()> {
    different_snapshots().await
}

#[tokio::test(flavor = "current_thread")]
async fn membership_replacement_current_thread_static_and_dynamic() -> Result<()> {
    bootstrap::late_query_group(SharedOutput::Transactions).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn membership_replacement_multi_thread_static_and_dynamic() -> Result<()> {
    bootstrap::late_query_group(SharedOutput::Transactions).await
}

fn renumber_transaction(
    input: &OutputEnvelope,
    logical: u64,
    transport: u64,
) -> Result<OutputEnvelope> {
    let transaction = SourceTransactionCodec::decode(&input.envelope, limits(16))?;
    let transaction_id = transaction.transaction_id().to_vec().into();
    let mut position: SharedTransactionPosition = serde_json::from_slice(transaction.position())?;
    position.sequence = logical;
    let mut builder = SourceTransactionBuilder::new("source", limits(16))?;
    for change in transaction.into_changes() {
        builder.push(change)?;
    }
    Ok(OutputEnvelope {
        port: input.port.clone(),
        envelope: builder
            .commit(transaction_id, serde_json::to_vec(&position)?.into())?
            .into_replay_envelope(
                stream(),
                logical,
                transport,
                input
                    .envelope
                    .system()
                    .timestamp()
                    .context("transaction timestamp")?,
            )?,
    })
}

async fn different_counters() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let path = tempfile::tempdir()?;
        let owners = ["fast", "slow"].map(|name| {
            Arc::new(
                QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name))
                    .expect("test query owner"),
            )
        });
        let mut fast = processing_query(
            "fast",
            Some(path.path()),
            owners[0].clone(),
            SharedOutput::Transactions,
        )
        .await?;
        let mut slow = processing_query(
            "slow",
            Some(path.path()),
            owners[1].clone(),
            SharedOutput::Transactions,
        )
        .await?;
        fast.start().await?;
        slow.start().await?;
        let mut source = factory_for(&plugins, SharedOutput::Transactions)?
            .create_component_with_consumer_progress(
                id("source"),
                json!({"stream":"changes","settings":database.config}),
                libraries::scope(ComponentBatch::GRAPH_ID),
                Arc::new(ConsumerProgressSet::new(
                    ComponentBatch::GRAPH_ID,
                    id("source"),
                    owners.clone(),
                )?),
            )?;
        source.start().await?;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1,'one'); COMMIT;
             BEGIN; UPDATE person SET name='two' WHERE id=1; COMMIT;
             BEGIN; DELETE FROM person WHERE id=1; INSERT INTO person VALUES(2,'three'); COMMIT;",
            )
            .await?;
        let mut transactions = Vec::new();
        for _ in 0..3 {
            transactions.push(
                tokio::time::timeout(Duration::from_secs(5), source.next())
                    .await??
                    .context("seed transaction")?,
            );
        }
        source.stop().await?;
        // Real, stopped-source transactions seed each actual persistent query.
        // These are intentionally unrelated logical counters, not fake progress.
        apply(
            &mut fast,
            renumber_transaction(&transactions[0], 9000, 20_000)?,
        )
        .await?;
        apply(&mut slow, renumber_transaction(&transactions[0], 1, 1)?).await?;
        apply(&mut slow, renumber_transaction(&transactions[1], 2, 2)?).await?;
        let fast_position = position(&owners[0])?.context("older physical cursor")?;
        let slow_position = position(&owners[1])?.context("newer physical cursor")?;
        assert!(fast_position.commit_lsn < slow_position.commit_lsn);
        assert!(fast_position.sequence > slow_position.sequence);
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        fast.stop().await?;
        slow.stop().await?;
        drop((fast, slow, source));

        let graph = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            128,
            false,
            None,
            SharedOutput::Transactions,
        )
        .await?;
        start_graph(&graph).await?;
        wait_until(
            "physical minimum replay with independent logical and transport counters",
            || {
                Ok(output_count(&graph.outputs[0]) == 2
                    && output_count(&graph.outputs[1]) == 1
                    && graph.owners.iter().all(|owner| {
                        owner.snapshot().transport_sequences.get(&stream()) == Some(&20_002)
                    }))
            },
        )
        .await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            assert_eq!(
                values(&graph.results[index])?,
                [json!({"id":2,"name":"three"})]
            );
            let inputs = graph.inputs[index].lock().expect("input log");
            assert_eq!(inputs.len(), 2);
            for (input, sequence) in inputs.iter().zip([20_001, 20_002]) {
                assert_eq!(input.system().sequence(), sequence);
                assert_eq!(input_position(input)?.sequence, sequence);
            }
            assert_eq!(
                position(owner)?.context("replayed cursor")?.sequence,
                20_002
            );
        }
        database
            .wait_feedback(
                position(&graph.owners[0])?
                    .context("latest cursor")?
                    .commit_lsn,
            )
            .await?;
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unrelated_counters_current_thread_static_and_dynamic() -> Result<()> {
    different_counters().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_counters_multi_thread_static_and_dynamic() -> Result<()> {
    different_counters().await
}

async fn interrupted_snapshots() -> Result<()> {
    for (dynamic, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'one'),(2,'two')")
            .await?;
        let original = database.feedback().await?;
        let path = tempfile::tempdir()?;
        let gates = [
            bootstrap::SnapshotGate::new(1, 1),
            bootstrap::SnapshotGate::new(1, 0),
        ];
        let graph = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            128,
            true,
            Some(&gates),
            SharedOutput::Transactions,
        )
        .await?;
        let mut starting = Box::pin(graph.core.start());
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                result = &mut starting => anyhow::bail!("gated snapshot completed early: {result:?}"),
                _ = gates[0].opened.notified() => Ok(()),
            }
        }).await??;
        wait_until(
            "one persistent row before interrupted initialization",
            || Ok(graph.results[0].snapshot()?.rows.len() == 1),
        )
        .await?;
        let copied_id = i32::try_from(
            values(&graph.results[0])?[0]["id"]
                .as_i64()
                .context("copied row id")?,
        )?;
        if !cancel {
            let readers = database.client().query(
                "SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND query=$1",
                &[&"FETCH FORWARD 1 FROM drasi_snapshot"],
            ).await?;
            assert_eq!(readers.len(), 1);
            let pid: i32 = readers[0].get(0);
            assert!(database
                .client()
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await?
                .get::<_, bool>(0));
            for gate in &gates {
                gate.copy.add_permits(128);
            }
            let started = tokio::time::timeout(Duration::from_secs(15), &mut starting).await?;
            assert!(started.is_err(), "failed snapshot must fail initialization");
        }
        drop(starting);
        tokio::time::timeout(Duration::from_secs(15), graph.core.shutdown()).await??;
        assert!(!graph.owners[0].snapshot().bootstrap_complete);
        assert!(position(&graph.owners[0])?.is_none());
        if cancel {
            assert!(!graph.owners[1].snapshot().bootstrap_complete);
            assert!(position(&graph.owners[1])?.is_none());
        }
        assert_eq!(database.feedback().await?, original);
        bootstrap::no_temporary_slots(&database).await?;
        bootstrap::no_snapshot_transactions(&database).await?;
        drop(graph);
        database
            .client()
            .execute("DELETE FROM person WHERE id=$1", &[&copied_id])
            .await?;
        database
            .client()
            .batch_execute("UPDATE person SET name='fresh'")
            .await?;
        let refused = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            128,
            true,
            None,
            SharedOutput::Transactions,
        )
        .await?;
        let error = start_graph(&refused)
            .await
            .expect_err("strict incomplete bootstrap");
        assert!(
            format!("{error:#}").contains("bootstrap did not complete"),
            "{error:#}"
        );
        drop(refused);
        assert_eq!(database.feedback().await?, original);
        let factory = plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == drasi_computation_bootstrap_postgres::POSTGRES_SHARED_TRANSACTION_BOOTSTRAP
            })
            .context("shared transaction snapshot")?;
        for name in ["fast", "slow"] {
            let owner = Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(name),
            )?);
            let mut query = processing_query_recovery(
                name,
                Some(path.path()),
                owner.clone(),
                SharedOutput::Transactions,
                QueryRecoveryPolicy::AutoReset,
            )
            .await?
            .with_bootstrap(factory.create_provider(
                id(name),
                json!({"source":"source","stream":"changes","settings":database.config}),
                Some(libraries::scope(ComponentBatch::GRAPH_ID)),
                Some(owner),
            )?);
            query.start().await?;
            query.stop().await?;
        }
        let restarted = build_graph_mode(
            &plugins,
            &database,
            Some(path.path()),
            128,
            true,
            None,
            SharedOutput::Transactions,
        )
        .await?;
        start_graph(&restarted).await?;
        wait_until("explicit rebootstrap and any older owner replay", || {
            for result in &restarted.results {
                if values(result)? != [json!({"id":3-copied_id,"name":"fresh"})] {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .await?;
        restarted.core.shutdown().await?;
        drop(restarted);
        bootstrap::no_temporary_slots(&database).await?;
        bootstrap::no_snapshot_transactions(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_snapshots_current_thread_static_and_dynamic() -> Result<()> {
    interrupted_snapshots().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_snapshots_multi_thread_static_and_dynamic() -> Result<()> {
    interrupted_snapshots().await
}

#[tokio::test(flavor = "current_thread")]
async fn actual_owner_and_persistent_mode_guards_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        let factory = factory_for(&plugins, SharedOutput::Transactions)?;
        let configuration = json!({"stream":"changes","settings":database.config});
        assert!(factory
            .create_component(id("source"), configuration.clone())
            .is_err());
        let owners = ["fast", "slow"].map(|name| {
            Arc::new(
                QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name))
                    .expect("test query owner"),
            )
        });
        let group = Arc::new(ConsumerProgressSet::new(
            ComponentBatch::GRAPH_ID,
            id("source"),
            owners.clone(),
        )?);
        assert!(
            plugins.source.factories()[0]
                .create_component_with_consumer_progress(
                    id("source"),
                    configuration.clone(),
                    libraries::scope(ComponentBatch::GRAPH_ID),
                    group.clone(),
                )
                .is_err(),
            "single-owner transaction factory must not adopt a consumer group"
        );
        assert!(factory
            .create_component_with_consumer_progress(
                id("source"),
                configuration.clone(),
                libraries::scope("wrong-graph"),
                group.clone(),
            )
            .is_err());
        let mut fast = ordinary_query("fast", None, owners[0].clone()).await?;
        let mut slow = ordinary_query("slow", None, owners[1].clone()).await?;
        fast.start().await?;
        slow.start().await?;
        let mut source = factory.create_component_with_consumer_progress(
            id("source"),
            configuration.clone(),
            libraries::scope(ComponentBatch::GRAPH_ID),
            group,
        )?;
        let error = source.start().await.expect_err("volatile consumer");
        assert!(format!("{error:#}").contains("persistent"), "{error:#}");
        source.stop().await?;
        fast.stop().await?;
        slow.stop().await?;
        drop((source, fast, slow));

        let snapshot_factory = plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == drasi_computation_bootstrap_postgres::POSTGRES_SHARED_TRANSACTION_BOOTSTRAP
            })
            .context("shared transaction bootstrap")?;
        let snapshot_configuration =
            json!({"source":"source","stream":"changes","settings":database.config});
        let owner = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("fast"),
        )?);
        let mut memory_query = ordinary_query("fast", None, owner.clone())
            .await?
            .with_bootstrap(snapshot_factory.create_provider(
                id("fast"),
                snapshot_configuration.clone(),
                Some(libraries::scope(ComponentBatch::GRAPH_ID)),
                Some(owner.clone()),
            )?);
        let error = memory_query
            .start()
            .await
            .expect_err("volatile transaction snapshot");
        assert!(format!("{error:#}").contains("persistent"), "{error:#}");
        memory_query.stop().await?;
        assert!(owner.snapshot().checkpoints.is_empty());

        let path = tempfile::tempdir()?;
        let expected = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("fast"),
        )?);
        let actual = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("fast"),
        )?);
        let mut substituted = processing_query(
            "fast",
            Some(path.path()),
            actual,
            SharedOutput::Transactions,
        )
        .await?
        .with_bootstrap(snapshot_factory.create_provider(
            id("fast"),
            snapshot_configuration,
            Some(libraries::scope(ComponentBatch::GRAPH_ID)),
            Some(expected.clone()),
        )?);
        let error = substituted
            .start()
            .await
            .expect_err("same-name different snapshot owner");
        assert!(
            format!("{error:#}").contains("actual progress owner"),
            "{error:#}"
        );
        substituted.stop().await?;
        assert!(expected.snapshot().checkpoints.is_empty());
        drop(substituted);

        let actual_fast = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("fast"),
        )?);
        let actual_slow = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("slow"),
        )?);
        let source = factory.create_component_with_consumer_progress(
            id("source"),
            configuration,
            libraries::scope(ComponentBatch::GRAPH_ID),
            Arc::new(ConsumerProgressSet::new(
                ComponentBatch::GRAPH_ID,
                id("source"),
                [expected, actual_slow.clone()],
            )?),
        )?;
        let fast = processing_query(
            "fast",
            Some(path.path()),
            actual_fast,
            SharedOutput::Transactions,
        )
        .await?;
        let slow = processing_query(
            "slow",
            Some(path.path()),
            actual_slow,
            SharedOutput::Transactions,
        )
        .await?;
        let batch = ComponentBatch::builder()
            .source(Box::new(source))
            .query(Box::new(fast))
            .query(Box::new(slow))
            .bind_stream(endpoint("source", "out"), stream())
            .bind_stream(endpoint("fast", "out"), StreamId::try_new("fast-results")?)
            .bind_stream(endpoint("slow", "out"), StreamId::try_new("slow-results")?)
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("fast", "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("slow", "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .build();
        let error = match batch {
            Err(error) => error,
            Ok(_) => anyhow::bail!("same-name consumer substitution must fail graph validation"),
        };
        assert!(
            format!("{error:#}").contains("actual committed-input owner"),
            "{error:#}"
        );
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        bootstrap::no_temporary_slots(&database).await?;
        bootstrap::no_snapshot_transactions(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}
