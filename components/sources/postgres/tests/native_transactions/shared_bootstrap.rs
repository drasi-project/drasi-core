// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use futures::StreamExt;
use tokio::sync::Notify;

pub(super) struct SnapshotGate {
    pub(super) begin: Arc<Semaphore>,
    pub(super) copy: Arc<Semaphore>,
    pub(super) opened: Notify,
}

impl SnapshotGate {
    pub(super) fn new(begin: usize, copy: usize) -> Arc<Self> {
        Arc::new(Self {
            begin: Arc::new(Semaphore::new(begin)),
            copy: Arc::new(Semaphore::new(copy)),
            opened: Notify::new(),
        })
    }
}

struct GatedBootstrap {
    inner: Arc<dyn ComputationBootstrapProvider>,
    gate: Arc<SnapshotGate>,
}

pub(super) fn gated(
    inner: Arc<dyn ComputationBootstrapProvider>,
    gate: Arc<SnapshotGate>,
) -> Arc<dyn ComputationBootstrapProvider> {
    Arc::new(GatedBootstrap { inner, gate })
}

impl GatedBootstrap {
    async fn open(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.gate.begin.clone().acquire_owned().await?.forget();
        let mut snapshot = match state {
            Some(state) => self.inner.snapshot_with_state(state).await?,
            None => self.inner.snapshot().await?,
        };
        self.gate.opened.notify_one();
        let gate = self.gate.copy.clone();
        snapshot.changes = Box::pin(futures::stream::try_unfold(
            snapshot.changes,
            move |mut stream| {
                let gate = gate.clone();
                async move {
                    gate.acquire_owned().await?.forget();
                    match stream.next().await {
                        Some(item) => Ok(Some((item?, stream))),
                        None => Ok(None),
                    }
                }
            },
        ));
        Ok(snapshot)
    }
}

#[async_trait]
impl ComputationBootstrapProvider for GatedBootstrap {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn freeze_for_retirement(&self) -> Result<Box<dyn BootstrapRetirement>> {
        self.inner.freeze_for_retirement()
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        self.inner.prepare().await
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        self.inner.has_pending_snapshot()
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.open(None).await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.open(Some(state)).await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.gate.begin.close();
        self.gate.copy.close();
        self.inner.stop().await
    }
}

pub(super) async fn no_temporary_slots(database: &Database) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = database
                .client()
                .query_one(
                    "SELECT count(*) FROM pg_replication_slots WHERE temporary",
                    &[],
                )
                .await?
                .get(0);
            if count == 0 {
                return Result::<()>::Ok(());
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

pub(super) async fn no_snapshot_transactions(database: &Database) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = database
                .client()
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() \
                 AND pid <> pg_backend_pid() AND xact_start IS NOT NULL",
                    &[],
                )
                .await?
                .get(0);
            if count == 0 {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn unequal_snapshots() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'before')")
            .await?;
        let original = database.feedback().await?;
        let gates = [SnapshotGate::new(1, 0), SnapshotGate::new(0, 0)];
        let graph = build_graph(&plugins, &database, None, 2, true, Some(&gates)).await?;
        let writes = async {
            tokio::time::timeout(Duration::from_secs(10), gates[0].opened.notified()).await?;
            database.client().batch_execute(
                "BEGIN; UPDATE person SET name='middle' WHERE id=1; INSERT INTO person VALUES(2,'middle'); COMMIT;"
            ).await?;
            gates[1].begin.add_permits(1);
            tokio::time::timeout(Duration::from_secs(10), gates[1].opened.notified()).await?;
            database.client().batch_execute(
                "BEGIN; UPDATE person SET name='after' WHERE id=1; DELETE FROM person WHERE id=2;
                 INSERT INTO person VALUES(3,'live'); COMMIT;"
            ).await?;
            assert_eq!(
                database.feedback().await?,
                original,
                "required initialization must fence the configured slot"
            );
            for owner in &graph.owners {
                assert!(!owner.snapshot().bootstrap_complete);
                assert!(position(owner)?.is_none());
            }
            no_temporary_slots(&database).await?;
            gates[0].copy.add_permits(128);
            gates[1].copy.add_permits(128);
            Result::<()>::Ok(())
        };
        let (started, writes) = tokio::join!(start_graph(&graph), writes);
        started?;
        writes?;
        wait_until("different snapshot replay boundaries", || {
            Ok(graph.outputs[0].lock().expect("fast outputs").len() == 5
                && graph.owners[1]
                    .snapshot()
                    .transport_sequences
                    .get(&stream())
                    == Some(&2))
        })
        .await?;
        let later = position(&graph.owners[1])?.context("later snapshot boundary")?;
        assert!(
            later.is_configured_snapshot(),
            "covered replay must preserve the later initialization position"
        );
        assert_eq!(graph.outputs[1].lock().expect("slow outputs").len(), 0);
        assert_eq!(graph.results[1].snapshot()?.rows.len(), 2);
        database.wait_feedback(later.feedback_lsn()).await?;
        graph.gates[1].add_permits(128);
        wait_until("post-snapshot changes in both queries", || {
            Ok(graph.outputs[1].lock().expect("slow outputs").len() == 3)
        })
        .await?;
        for (index, owner) in graph.owners.iter().enumerate() {
            let mut rows = values(&graph.results[index])?;
            rows.sort_by_key(Value::to_string);
            assert_eq!(
                rows,
                [
                    json!({"id":1,"name":"after"}),
                    json!({"id":3,"name":"live"})
                ]
            );
            assert!(owner.snapshot().bootstrap_complete);
            assert!(!owner.snapshot().persistent);
            assert_eq!(owner.snapshot().transport_sequences[&stream()], 5);
        }
        database
            .wait_feedback(
                position(&graph.owners[0])?
                    .context("live cursor")?
                    .feedback_lsn(),
            )
            .await?;
        graph.core.shutdown().await?;
        drop(graph);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unequal_configured_snapshots_current_thread_static_and_dynamic() -> Result<()> {
    unequal_snapshots().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unequal_configured_snapshots_multi_thread_static_and_dynamic() -> Result<()> {
    unequal_snapshots().await
}

#[tokio::test(flavor = "current_thread")]
async fn late_query_joins_by_replacing_the_actual_consumer_group() -> Result<()> {
    late_query_group(SharedOutput::Rows).await
}

pub(super) async fn late_query_group(output: SharedOutput) -> Result<()> {
    let feedback = |owner: &QuerySourceProgress| -> Result<Option<u64>> {
        match output {
            SharedOutput::Rows => Ok(position(owner)?.map(|position| position.feedback_lsn())),
            SharedOutput::Transactions => Ok(
                drasi_postgres_common::replication::shared_transactions::read_position(
                    &owner.snapshot(),
                    &id("source"),
                )?
                .map(|position| position.commit_lsn),
            ),
        }
    };
    for (dynamic, queued) in [(false, false), (true, false), (false, true), (true, true)] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'snapshot')")
            .await?;
        let path = (output == SharedOutput::Transactions)
            .then(tempfile::tempdir)
            .transpose()?;
        let path = path.as_ref().map(|directory| directory.path());
        let graph = build_graph_mode(&plugins, &database, path, 128, true, None, output).await?;
        start_graph(&graph).await?;
        database
            .client()
            .batch_execute("UPDATE person SET name='before enrollment'")
            .await?;
        wait_until("existing owner progress", || {
            Ok(graph
                .outputs
                .iter()
                .all(|events| events.lock().expect("output log").len() == 1))
        })
        .await?;
        let before = feedback(&graph.owners[0])?.context("existing cursor")?;
        database.wait_feedback(before).await?;
        let queued_count = if output == SharedOutput::Transactions {
            4
        } else {
            7
        };
        if queued {
            for gate in &graph.gates {
                gate.clone()
                    .acquire_many_owned(u32::try_from(gate.available_permits())?)
                    .await?
                    .forget();
            }
            database
                .client()
                .batch_execute(match output {
                    SharedOutput::Rows =>
                        "BEGIN; UPDATE person SET name='queued-1'; UPDATE person SET name='queued-2';
                         UPDATE person SET name='queued-3'; UPDATE person SET name='queued-4';
                         UPDATE person SET name='queued-5'; UPDATE person SET name='queued-6'; COMMIT;",
                    SharedOutput::Transactions =>
                        "BEGIN; UPDATE person SET name='queued-1'; UPDATE person SET name='queued-2'; COMMIT;
                         BEGIN; UPDATE person SET name='queued-3'; UPDATE person SET name='queued-4'; COMMIT;
                         BEGIN; UPDATE person SET name='queued-5'; UPDATE person SET name='queued-6'; COMMIT;",
                })
                .await?;
            wait_until("accepted rows held before group replacement", || {
                Ok(graph
                    .accepted
                    .iter()
                    .all(|events| events.lock().expect("accepted input log").len() == queued_count))
            })
            .await?;
            for (owner, accepted) in graph.owners.iter().zip(&graph.accepted) {
                let accepted = accepted.lock().expect("accepted input log");
                assert!(
                    owner.snapshot().transport_sequences[&stream()]
                        < accepted.last().context("queued input")?.system().sequence()
                );
            }
        }
        let previous_count = if queued { queued_count } else { 1 };
        let late_owner = Arc::new(QuerySourceProgress::new(
            ComponentBatch::GRAPH_ID,
            id("late"),
        )?);
        let bootstrap_factory = plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref() == match output {
                    SharedOutput::Rows => {
                        drasi_computation_bootstrap_postgres::POSTGRES_SHARED_ROW_BOOTSTRAP
                    }
                    SharedOutput::Transactions => {
                        drasi_computation_bootstrap_postgres::POSTGRES_SHARED_TRANSACTION_BOOTSTRAP
                    }
                }
            })
            .context("shared bootstrap factory")?;
        let gate = SnapshotGate::new(1, 0);
        let late = processing_query("late", path, late_owner.clone(), output)
            .await?
            .with_bootstrap(gated(
                bootstrap_factory.create_provider(
                    id("late"),
                    json!({"source":"source","stream":"changes","settings":database.config}),
                    Some(libraries::scope(ComponentBatch::GRAPH_ID)),
                    Some(late_owner.clone()),
                )?,
                gate.clone(),
            ));
        let late_results = late.results();
        let mut owners = graph.owners.clone();
        owners.push(late_owner.clone());
        let group = Arc::new(ConsumerProgressSet::new(
            ComponentBatch::GRAPH_ID,
            id("source"),
            owners,
        )?);
        let configuration = match output {
            SharedOutput::Rows => json!({
                "stream":"changes","settings":database.config,
                "persistent":false,"auto_create_slot":false,"start_from_slot":false,
            }),
            SharedOutput::Transactions => json!({"stream":"changes","settings":database.config}),
        };
        let source = factory_for(&plugins, output)?.create_component_with_consumer_progress(
            id("source"),
            configuration,
            libraries::scope(ComponentBatch::GRAPH_ID),
            group,
        )?;
        let received: Events = Arc::default();
        let sink = Collect {
            descriptor: ComponentDescriptor::try_new(
                id("late-sink"),
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            events: received.clone(),
        };
        let declaration =
            |descriptor: ComponentDescriptor, role, output: Option<StreamId>, completion| {
                let binding = descriptor.id().to_string();
                DesiredComponent {
                    descriptor,
                    role,
                    completion,
                    streams: output
                        .into_iter()
                        .map(|stream| (PortId::try_new("out").expect("output"), stream))
                        .collect(),
                    lifecycle: LifecyclePolicy::default(),
                    input_merge: InputMergePolicy::Arrival,
                    construction: ComponentConstruction::External { binding },
                }
            };
        let source_definition = declaration(
            source.descriptor().clone(),
            ComponentRole::Source,
            Some(stream()),
            None,
        );
        let late_definition = declaration(
            late.descriptor().clone(),
            ComponentRole::Query,
            Some(StreamId::try_new("late-results")?),
            None,
        );
        let sink_definition = declaration(
            sink.descriptor.clone(),
            ComponentRole::Sink,
            None,
            Some(SinkCompletion::Handled),
        );
        let relationship = |from, to| DesiredRelationship {
            definition: EdgeDefinition::new(endpoint(from, "out"), endpoint(to, "in")),
            policy: RelationshipPolicy::default(),
            pipe: DesiredPipe::Bounded { capacity: 32 },
        };
        let control = graph.core.computation_control()?;
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![
                    DesiredMutation::ReplaceComponent(source_definition),
                    DesiredMutation::PutComponent(late_definition),
                    DesiredMutation::PutComponent(sink_definition),
                    DesiredMutation::Bind(relationship("source", "late")),
                    DesiredMutation::Bind(relationship("late", "late-sink")),
                ],
            )
            .await?;
        let mut bindings = TopologyBindings::default();
        for relationship in &preview.desired().relationships {
            if let DesiredPipe::External { binding, .. } = &relationship.pipe {
                let index = graph
                    .owners
                    .iter()
                    .position(|owner| owner.component_id() == &relationship.definition.to.component)
                    .context("existing gated input owner")?;
                bindings.pipes.insert(
                    binding.clone(),
                    Box::new(GateProvider {
                        gate: graph.gates[index].clone(),
                        inputs: graph.inputs[index].clone(),
                        accepted: graph.accepted[index].clone(),
                    }),
                );
            }
        }
        bindings.components.insert(
            "source".into(),
            ConstructedComponent::source(Box::new(source)),
        );
        bindings
            .components
            .insert("late".into(), ConstructedComponent::query(Box::new(late)));
        bindings.components.insert(
            "late-sink".into(),
            ConstructedComponent::sink(Box::new(sink)),
        );
        let writes =
            async {
                let result =
                    async {
                        if queued {
                            let revision = control.desired_snapshot().revision;
                            let mut observed = control.subscribe_observed();
                            tokio::time::timeout(
                                Duration::from_secs(3),
                                observed.wait_for(|state| {
                                    state.components.get(&id("source")).is_some_and(|source| {
                                        matches!(
                                            source.lifecycle,
                                            ComponentLifecycle::Quiescing
                                                | ComponentLifecycle::Quiesced
                                                | ComponentLifecycle::Stopping
                                                | ComponentLifecycle::Stopped
                                        )
                                    })
                                }),
                            )
                            .await??;
                            assert_eq!(
                                control.desired_snapshot().revision,
                                revision,
                                "accepted inputs must drain before committing the replacement"
                            );
                            for old_gate in &graph.gates {
                                old_gate.add_permits(128);
                            }
                        }
                        tokio::time::timeout(Duration::from_secs(10), gate.opened.notified())
                            .await?;
                        let active: bool = database.client().query_one(
                    "SELECT active FROM pg_replication_slots WHERE slot_name='native_slot'", &[],
                ).await?.get(0);
                        assert!(
                            !active,
                            "the prior reader must be stopped before enrolling a required member"
                        );
                        let initialization_floor = database.feedback().await?;
                        database
                            .client()
                            .batch_execute("UPDATE person SET name='during enrollment'")
                            .await?;
                        assert_eq!(database.feedback().await?, initialization_floor);
                        assert!(!late_owner.snapshot().ready);
                        assert!(feedback(&late_owner)?.is_none());
                        Result::<()>::Ok(())
                    }
                    .await;
                gate.copy.add_permits(128);
                result
            };
        let (reconciled, writes) = tokio::join!(
            tokio::time::timeout(
                Duration::from_secs(25),
                control.reconcile(preview, bindings)
            ),
            writes,
        );
        let report = match reconciled {
            Ok(Ok(report)) => report,
            result => {
                graph.core.shutdown().await?;
                drop(graph);
                database.shutdown().await?;
                anyhow::bail!(
                    "consumer group reconciliation failed before snapshot completion: {result:?}"
                );
            }
        };
        if report.summary != OperationSummary::Completed {
            graph.core.shutdown().await?;
            drop(graph);
            database.shutdown().await?;
            anyhow::bail!("shared group replacement did not start: {report:?}");
        }
        writes.context("late configured snapshot never opened")?;
        wait_until("late query CDC after its independent snapshot", || {
            Ok(received.lock().expect("late output log").len() == 1
                && graph
                    .outputs
                    .iter()
                    .all(|events| events.lock().expect("output log").len() == previous_count + 1))
        })
        .await?;
        for accepted in &graph.accepted {
            let accepted = accepted.lock().expect("accepted input log");
            assert!(
                accepted[previous_count].system().sequence()
                    > accepted[previous_count - 1].system().sequence(),
                "replacement cannot reuse transport counters of previously admitted rows"
            );
        }
        assert_eq!(
            values(&late_results)?,
            [json!({"id":1,"name":"during enrollment"})]
        );
        for result in &graph.results {
            assert_eq!(
                values(result)?,
                [json!({"id":1,"name":"during enrollment"})]
            );
        }
        assert!(late_owner.snapshot().bootstrap_complete);
        assert_eq!(
            late_owner.snapshot().persistent,
            output == SharedOutput::Transactions
        );
        assert_eq!(
            control
                .desired_snapshot()
                .nodes
                .iter()
                .filter(|node| node.role == ComponentRole::Source)
                .count(),
            1
        );
        database
            .client()
            .batch_execute("UPDATE person SET name='after enrollment'")
            .await?;
        wait_until("three consumers of one source", || {
            Ok(received.lock().expect("late output log").len() == 2
                && graph
                    .outputs
                    .iter()
                    .all(|events| events.lock().expect("output log").len() == previous_count + 2))
        })
        .await?;
        database
            .wait_feedback(feedback(&late_owner)?.context("late live cursor")?)
            .await?;
        graph.core.shutdown().await?;
        drop(graph);
        no_temporary_slots(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn memory_owner_reenrollment_stops_rewinds_and_resubscribes_one_shared_source() -> Result<()>
{
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'snapshot')")
            .await?;
        let graph = build_graph(&plugins, &database, None, 128, true, None).await?;
        start_graph(&graph).await?;
        database
            .client()
            .batch_execute("UPDATE person SET name='before restart'")
            .await?;
        wait_until("pre-reenrollment query progress", || {
            Ok(graph
                .outputs
                .iter()
                .all(|events| events.lock().expect("output log").len() == 1))
        })
        .await?;
        let generation = graph.owners[0].snapshot().reset_generation;
        let control = graph.core.computation_control()?;
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::Restart(GraphSelection::Exact(vec![id(
                    "fast",
                )]))],
            )
            .await?;
        assert!(preview.restarted().contains(&id("source")));
        let report = tokio::time::timeout(
            Duration::from_secs(20),
            control.reconcile(preview, TopologyBindings::default()),
        )
        .await??;
        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
        assert!(graph.owners[0].snapshot().reset_generation > generation);
        assert!(position(&graph.owners[0])?
            .context("reenrollment boundary")?
            .is_configured_snapshot());
        assert!(!graph.owners[0].snapshot().persistent);
        database
            .client()
            .batch_execute("UPDATE person SET name='after restart'")
            .await?;
        wait_until("post-reenrollment shared input", || {
            Ok(graph
                .outputs
                .iter()
                .all(|events| events.lock().expect("output log").len() == 2))
        })
        .await?;
        for result in &graph.results {
            assert_eq!(values(result)?, [json!({"id":1,"name":"after restart"})]);
        }
        assert_eq!(
            control
                .desired_snapshot()
                .nodes
                .iter()
                .filter(|component| component.role == ComponentRole::Source)
                .count(),
            1
        );
        graph.core.shutdown().await?;
        drop(graph);
        no_temporary_slots(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_snapshot_reader_never_completes_or_acknowledges_its_partial_rows() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'one'),(2,'two')")
            .await?;
        let original = database.feedback().await?;
        let gates = [SnapshotGate::new(1, 1), SnapshotGate::new(1, 0)];
        let graph = build_graph(&plugins, &database, None, 128, true, Some(&gates)).await?;
        let interrupt = async {
            let result = async {
                tokio::time::timeout(Duration::from_secs(10), gates[0].opened.notified()).await?;
                wait_until("one row before snapshot failure", || Ok(graph.results[0].snapshot()?.rows.len() == 1)).await?;
                let readers = database.client().query(
                    "SELECT pid FROM pg_stat_activity WHERE datname=current_database() AND query=$1",
                    &[&"FETCH FORWARD 1 FROM drasi_snapshot"],
                ).await?;
                assert_eq!(readers.len(), 1, "only the owned partial snapshot reader must be terminated");
                let pid: i32 = readers[0].get(0);
                assert!(database.client().query_one("SELECT pg_terminate_backend($1)", &[&pid]).await?.get::<_, bool>(0));
                Result::<()>::Ok(())
            }.await;
            for gate in &gates {
                gate.copy.add_permits(128);
            }
            result
        };
        let (started, interrupted) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(20), graph.core.start()),
            interrupt,
        );
        interrupted?;
        assert!(
            started?.is_err(),
            "a broken snapshot must fail query initialization"
        );
        assert!(!graph.owners[0].snapshot().ready);
        assert!(!graph.owners[0].snapshot().bootstrap_complete);
        assert!(position(&graph.owners[0])?.is_none());
        graph.core.shutdown().await?;
        assert_eq!(database.feedback().await?, original);
        no_temporary_slots(&database).await?;
        no_snapshot_transactions(&database).await?;
        drop(graph);
        let recovered = build_graph(&plugins, &database, None, 128, true, None).await?;
        start_graph(&recovered).await?;
        for result in &recovered.results {
            assert_eq!(result.snapshot()?.rows.len(), 2);
        }
        recovered.core.shutdown().await?;
        drop(recovered);
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_memory_snapshot_publishes_no_cursor_or_slot_feedback() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(1,'one'),(2,'two')")
            .await?;
        let original = database.feedback().await?;
        let gates = [SnapshotGate::new(1, 1), SnapshotGate::new(1, 0)];
        let graph = build_graph(&plugins, &database, None, 128, true, Some(&gates)).await?;
        let mut starting = Box::pin(graph.core.start());
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! {
                result = &mut starting => anyhow::bail!("gated startup completed early: {result:?}"),
                _ = gates[0].opened.notified() => Ok(()),
            }
        }).await??;
        wait_until("one partially loaded snapshot row", || {
            Ok(graph.results[0].snapshot()?.rows.len() == 1)
        })
        .await?;
        for owner in &graph.owners {
            assert!(!owner.snapshot().ready);
            assert!(!owner.snapshot().bootstrap_complete);
            assert!(position(owner)?.is_none());
        }
        drop(starting);
        tokio::time::timeout(Duration::from_secs(15), graph.core.shutdown()).await??;
        for owner in &graph.owners {
            assert!(!owner.snapshot().bootstrap_complete);
            assert!(position(owner)?.is_none());
            assert!(!owner.snapshot().persistent);
        }
        no_temporary_slots(&database).await?;
        no_snapshot_transactions(&database).await?;
        assert_eq!(database.feedback().await?, original);
        drop(graph);
        let restarted = build_graph(&plugins, &database, None, 128, true, None).await?;
        start_graph(&restarted).await?;
        for result in &restarted.results {
            assert_eq!(result.snapshot()?.rows.len(), 2);
        }
        restarted.core.shutdown().await?;
        drop(restarted);
        database.shutdown().await?;
    }
    Ok(())
}
