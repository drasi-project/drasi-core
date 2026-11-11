// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    ApplyUpdateRequest, ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan,
    UpdateRequest,
};
use abi::operation::*;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentRole, ComponentSemanticKind, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, ImplementationIdentity, InputEnvelope,
    OutputCursor, OutputEnvelope, OutputReadError, OutputReaders, OutputReplay, OutputSnapshot,
    PortDescriptor, PortId, QueryChangeCodec, QueryDescription, QueryOutputView,
    QueryResultSnapshot, QueryViewProvider, ResultNodeProjector, StreamId,
};
use std::num::NonZeroUsize;

const ID: &str = "d24a2d96-41ae-479c-9752-e7a7f6080a1e";

#[derive(Clone, Copy)]
enum Mutation {
    Capacity,
    ReplaceReader,
    RemoveCapability,
    ChangePort,
    FailHook,
}

#[derive(Clone)]
struct QueryState {
    description: QueryDescription,
    snapshot: OutputSnapshot,
    projector: ResultNodeProjector,
    retained: Vec<drasi_computation_contracts::ChangeEnvelope>,
    observations: bool,
}

struct QueryReader(Mutex<QueryState>);

impl QueryReader {
    fn new(request: &CreateRequest) -> anyhow::Result<Self> {
        let scope = request.scope.as_ref().unwrap();
        let stream = StreamId::try_new("query/out")?;
        let producer = GraphProducerIdentity::volatile(
            scope.instance_id.clone(),
            scope.graph_id.clone(),
            request.id.clone(),
            stream.clone(),
        )?;
        let projector = ResultNodeProjector::try_new("query-results", None)?;
        let record = QueryChangeCodec::encode_row(&projector, 1, &Default::default(), 42)?;
        let snapshot = OutputSnapshot::try_new(
            OutputCursor::try_new(producer.clone(), 2)?,
            projector.dataset(),
            GraphChangeCodec::schema().descriptor().clone(),
            [(record.identity().clone(), record)].into_iter().collect(),
        )?;
        let retained = (1..=2)
            .map(|sequence| {
                let mut output =
                    GraphChangeCodec::encode_changes(&[], stream.clone(), sequence, None)?;
                GraphProducerProgress::annotate(&mut output, &producer, sequence)?;
                Ok(output)
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self(Mutex::new(QueryState {
            description: QueryDescription {
                graph_id: scope.graph_id.clone(),
                query: "RETURN 1 AS value".into(),
                language: Default::default(),
                execution: Default::default(),
                output_stream: stream,
                outbox_capacity: NonZeroUsize::new(
                    request.configuration["outbox_capacity"].as_u64().unwrap() as usize,
                )
                .unwrap(),
                bootstrap: false,
                auto_reset: false,
                volatile: true,
                configuration_hash: 73,
            },
            snapshot,
            projector,
            retained,
            observations: true,
        })))
    }

    fn append_progress(&self) -> anyhow::Result<OutputEnvelope> {
        let mut state = self.0.lock().unwrap();
        let cursor = state
            .snapshot
            .cursor()
            .at_sequence(state.snapshot.cursor().sequence() + 1);
        let mut output = GraphChangeCodec::encode_changes(
            &[],
            cursor.producer().stream().clone(),
            cursor.sequence(),
            None,
        )?;
        GraphProducerProgress::annotate(&mut output, cursor.producer(), cursor.sequence())?;
        state.snapshot = OutputSnapshot::try_new(
            cursor,
            state.snapshot.dataset(),
            state.snapshot.schema().clone(),
            state.snapshot.records().clone(),
        )?;
        state.retained.push(output.clone());
        while state.retained.len() > state.description.outbox_capacity.get() {
            state.retained.remove(0);
        }
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: output,
            execution_context: Default::default(),
        })
    }
}

#[async_trait]
impl QueryViewProvider for QueryReader {
    fn description(&self) -> QueryDescription {
        self.0.lock().unwrap().description.clone()
    }
    fn supports_observations(&self) -> bool {
        self.0.lock().unwrap().observations
    }
    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError> {
        Ok((self.0.lock().unwrap().snapshot.cursor().clone(), 7))
    }
    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError> {
        Ok((self.0.lock().unwrap().snapshot.clone(), 7))
    }
    async fn query_view(&self) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        let state = self.0.lock().unwrap();
        let cursor = state.snapshot.cursor().clone();
        Ok((
            cursor.clone(),
            QueryOutputView {
                snapshot: QueryResultSnapshot {
                    projector: state.projector.clone(),
                    as_of_sequence: cursor.sequence(),
                    generation: 7,
                    rows: state.snapshot.records().clone(),
                },
                retained: state.retained.clone(),
                config_hash: state.description.configuration_hash,
                incarnation: Some(cursor.epoch()),
            },
        ))
    }
    async fn query_replay(
        &self,
        after: &OutputCursor,
    ) -> Result<(OutputReplay, u64), OutputReadError> {
        let state = self.0.lock().unwrap();
        Ok((
            OutputReplay::try_new(
                after.clone(),
                state.snapshot.cursor().clone(),
                state
                    .retained
                    .iter()
                    .filter(|output| output.system().sequence() > after.sequence())
                    .cloned()
                    .collect(),
            )?,
            7,
        ))
    }
}

struct QueryFactory(Mutation);

impl Factory for QueryFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/reflected-query", "1")
                .unwrap(),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: crate::ConfigSchema::from_fields([(
                "outbox_capacity",
                crate::ConfigType::Integer,
                true,
            )]),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).unwrap(),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        Default::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: crate::Capabilities {
                semantic_kind: Some(ComponentSemanticKind::Query),
                output_queries: vec![PortId::try_new("out").unwrap()],
                query_observations: true,
                update_modes: vec![ComponentUpdateMode::InPlace],
                ..Default::default()
            },
        }
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn plan_update(&self, request: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            request.proposed.configuration["outbox_capacity"].as_u64()
                > request.current.configuration["outbox_capacity"].as_u64(),
            "only capacity increases are supported"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::InPlace,
            data: ComponentUpdateData::Preserve,
        })
    }
    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Transformer(Box::new(QueryComponent {
            descriptor: self.metadata().descriptor(request.id.clone())?,
            configuration: request.configuration.clone(),
            reader: Arc::new(QueryReader::new(request)?),
            mutation: self.0,
        }))
        .into())
    }
}

struct QueryComponent {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    reader: Arc<QueryReader>,
    mutation: Mutation,
}

#[async_trait]
impl ComputationComponent for QueryComponent {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    fn supports_apply_update(&self) -> bool {
        true
    }
    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_query(PortId::try_new("out").unwrap(), self.reader.clone())
    }
    async fn apply_update(&mut self, update: &ApplyUpdateRequest) -> anyhow::Result<()> {
        self.configuration = update.request.proposed.configuration.clone();
        self.reader.0.lock().unwrap().description.outbox_capacity =
            NonZeroUsize::new(self.configuration["outbox_capacity"].as_u64().unwrap() as usize)
                .unwrap();
        match self.mutation {
            Mutation::Capacity => {}
            Mutation::ReplaceReader => {
                let state = self.reader.0.lock().unwrap().clone();
                self.reader = Arc::new(QueryReader(Mutex::new(state)));
            }
            Mutation::RemoveCapability => self.reader.0.lock().unwrap().observations = false,
            Mutation::ChangePort => {
                self.descriptor = ComponentDescriptor::try_new(
                    self.descriptor.id().clone(),
                    vec![PortDescriptor::new(
                        PortId::try_new("changed")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        Default::default(),
                    )],
                )?
            }
            Mutation::FailHook => anyhow::bail!("injected update failure after metadata mutation"),
        }
        Ok(())
    }
}

#[async_trait]
impl Transformer for QueryComponent {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(vec![self.reader.append_progress()?])
    }
}

struct Fixture {
    exported: ExportedPlugin,
    factory: abi::FactoryHandle,
    component: abi::ComponentHandle,
    request: CreateRequest,
}

impl Fixture {
    fn new(mutation: Mutation) -> anyhow::Result<Self> {
        let exported = ExportedPlugin::new(PluginDefinition::new(
            "fixture/query-metadata",
            "1",
            vec![Arc::new(QueryFactory(mutation))],
            vec![GraphChangeCodec::schema()],
        )?)?;
        let request = CreateRequest {
            id: ComponentId::try_new(ID)?,
            implementation: exported.state.definition.metadata.factories[0]
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: serde_json::json!({"outbox_capacity": 2}),
            scope: Some(crate::Scope {
                instance_id: "instance".into(),
                graph_id: "graph".into(),
                generation: 1,
            }),
            inputs: Vec::new(),
            activation_credentials_bound: false,
            update_directive: None,
        };
        let mut plugin = abi::PluginHandle::null();
        let mut factory = abi::FactoryHandle::null();
        let mut component = abi::ComponentHandle::null();
        let bytes = wire::encode_create_request(&request)?;
        unsafe {
            exported.entry(&mut plugin)?;
            transport::take_status((*plugin.vtable).factory.unwrap()(
                plugin.state,
                0,
                &mut factory,
            ))?;
            (*plugin.vtable).release.unwrap()(plugin.state);
            transport::take_status((*factory.vtable).create.unwrap()(
                factory.state,
                BorrowedBytes::new(&bytes),
                &mut component,
            ))?;
        }
        Ok(Self {
            exported,
            factory,
            component,
            request,
        })
    }

    fn inspect(&self) -> anyhow::Result<InstanceMetadata> {
        unsafe {
            Ok(serde_json::from_slice(&transport::take_reply((*self
                .component
                .vtable)
                .inspect
                .unwrap()(
                self.component.state,
            ))?)?)
        }
    }

    async fn call(&self, code: u32, input: &[u8]) -> anyhow::Result<transport::ReceivedBytes> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            transport::take_status((*self.component.vtable).begin.unwrap()(
                self.component.state,
                code,
                BorrowedBytes::new(input),
                std::ptr::null(),
                &mut operation,
            ))?;
            Ok(transport::OperationFuture::new(operation)?.await?)
        }
    }

    fn approved(&self) -> anyhow::Result<ApplyUpdateRequest> {
        let mut proposed = self.request.clone();
        proposed.configuration = serde_json::json!({"outbox_capacity": 8});
        let request = UpdateRequest {
            current: self.request.clone(),
            proposed,
        };
        let bytes = serde_json::to_vec(&request)?;
        let plan = unsafe {
            serde_json::from_slice(&transport::take_reply((*self.factory.vtable)
                .plan_update
                .unwrap()(
                self.factory.state,
                BorrowedBytes::new(&bytes),
            ))?)?
        };
        Ok(ApplyUpdateRequest {
            operation_id: "grow-outbox".into(),
            request,
            plan,
        })
    }

    fn query(&self) -> anyhow::Result<QueryHandle> {
        let mut handle = abi::output_reads::OutputReadHandle::null();
        unsafe {
            transport::take_status((*self.exported.output_reads()).open.unwrap()(
                self.component.state,
                BorrowedBytes::new(b"out"),
                abi::output_reads::capability::QUERY,
                &mut handle,
            ))?;
        }
        Ok(QueryHandle(handle))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unsafe {
            (*self.component.vtable).release.unwrap()(self.component.state);
            (*self.factory.vtable).release.unwrap()(self.factory.state);
        }
    }
}

struct QueryHandle(abi::output_reads::OutputReadHandle);

impl QueryHandle {
    async fn read(&self, code: u32) -> anyhow::Result<transport::ReceivedBytes> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            transport::take_status((*self.0.vtable).request.unwrap()(
                self.0.state,
                code,
                BorrowedBytes::empty(),
                &mut operation,
            ))?;
            Ok(transport::OperationFuture::new(operation)?.await?)
        }
    }
}

impl Drop for QueryHandle {
    fn drop(&mut self) {
        unsafe { (*self.0.vtable).release.unwrap()(self.0.state) }
    }
}

async fn query_metadata_reflection() -> anyhow::Result<()> {
    use crate::output_reads::{Outcome, WireQueryObservation, WireQuerySnapshot};
    use abi::output_reads::operation::{QUERY_OBSERVATION, QUERY_SNAPSHOT};

    let fixture = Fixture::new(Mutation::Capacity)?;
    let query = fixture.query()?;
    let before = fixture.inspect()?;
    let out = PortId::try_new("out")?;
    assert_eq!(before.queries[&out].outbox_capacity.get(), 2);
    fixture.call(INITIALIZE, &[]).await?;
    fixture.call(START, &[]).await?;
    let snapshot = query.read(QUERY_SNAPSHOT).await?;
    let Outcome::Ok(rows): Outcome<WireQuerySnapshot> = wire::decode(&snapshot)? else {
        anyhow::bail!("snapshot must be readable");
    };
    assert_eq!(rows.snapshot.records.len(), 1);
    fixture.call(PAUSE_PROCESSING, &[]).await?;
    let update = wire::encode(&fixture.approved()?)?;
    fixture.call(APPLY_UPDATE, &update).await?;
    let after = fixture.inspect()?;
    assert_eq!(after.queries[&out].outbox_capacity.get(), 8);
    assert_eq!(after.descriptor, before.descriptor);
    assert_eq!(after.capabilities, before.capabilities);
    let mut previous_description = serde_json::to_value(&before.queries[&out])?;
    previous_description["outbox_capacity"] = serde_json::json!(8);
    assert_eq!(
        serde_json::to_value(&after.queries[&out])?,
        previous_description
    );
    fixture.call(APPLY_UPDATE, &update).await?;
    assert_eq!(fixture.inspect()?.queries[&out].outbox_capacity.get(), 8);
    fixture.call(RESUME_PROCESSING, &[]).await?;
    let preserved = query.read(QUERY_SNAPSHOT).await?;
    assert_eq!(
        &*preserved, &*snapshot,
        "same old reader handle must retain its rows, cursor, generation and epoch"
    );

    let codec = wire::codec(&[GraphChangeCodec::schema()])?;
    let input = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(&[], StreamId::try_new("source/out")?, 1, None)?,
        execution_context: Default::default(),
    };
    let output = fixture
        .call(
            TRANSFORM,
            &wire::encode(&wire::Envelope::input(&input, &codec)?)?,
        )
        .await?;
    let batch: wire::OutputBatch = wire::decode(&output)?;
    assert_eq!(
        batch.query_observation.unwrap().outbox_size,
        3,
        "processing must validate against the refreshed capacity, not 2"
    );
    let observation = query.read(QUERY_OBSERVATION).await?;
    let Outcome::Ok(observation): Outcome<WireQueryObservation> = wire::decode(&observation)?
    else {
        anyhow::bail!("the retained read binding must validate refreshed query metadata");
    };
    assert_eq!(observation.outbox_size, 3);
    assert_eq!(observation.cursor.producer, rows.snapshot.cursor.producer);
    assert_eq!(observation.live_results, 1);
    fixture.call(STOP, &[]).await?;
    fixture.call(DISPOSE, &[]).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn query_metadata_refresh_preserves_reader_rows_and_epoch_current_thread(
) -> anyhow::Result<()> {
    query_metadata_reflection().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_metadata_refresh_preserves_reader_rows_and_epoch_multi_thread() -> anyhow::Result<()>
{
    query_metadata_reflection().await
}

#[tokio::test]
async fn failed_updates_do_not_publish_descriptions_or_replace_captured_readers(
) -> anyhow::Result<()> {
    for (mutation, message) in [
        (
            Mutation::ReplaceReader,
            "replaced the registered output reader",
        ),
        (
            Mutation::RemoveCapability,
            "observations have no implementation",
        ),
        (Mutation::ChangePort, "descriptor differs"),
        (Mutation::FailHook, "injected update failure"),
    ] {
        let fixture = Fixture::new(mutation)?;
        fixture.call(INITIALIZE, &[]).await?;
        fixture.call(START, &[]).await?;
        fixture.call(PAUSE_PROCESSING, &[]).await?;
        let update = wire::encode(&fixture.approved()?)?;
        let error = fixture.call(APPLY_UPDATE, &update).await.unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        assert_eq!(
            fixture.inspect()?.queries[&PortId::try_new("out")?]
                .outbox_capacity
                .get(),
            2
        );
        assert!(fixture.call(RESUME_PROCESSING, &[]).await.is_err());
        fixture.call(STOP, &[]).await?;
        fixture.call(DISPOSE, &[]).await?;
    }
    Ok(())
}
