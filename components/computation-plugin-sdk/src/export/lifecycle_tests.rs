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
    ComponentDescriptor, ComponentId, ComponentRole, DeliveryHandler, DeliveryItem,
    GraphChangeCodec, ImplementationIdentity, InputEnvelope, OutputEnvelope, OutputReadError,
    OutputReaders, OutputSnapshot, PortDescriptor, PortId, SinkCompletion, SnapshotProvider,
    SourceSchemaReader,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::atomic::AtomicUsize,
    time::Duration,
};

const ID: &str = "c086926b-5952-44d3-a129-c6ea5826f8b4";
const OTHER_ID: &str = "06d4d7de-318c-49dd-a825-f8f139ced3a0";

#[derive(Clone, Copy)]
enum Fault {
    Error,
    Pending,
    Panic,
}

#[derive(Default)]
struct ProbeState {
    calls: Mutex<Vec<&'static str>>,
    directives: Mutex<Vec<Option<crate::UpdateDirective>>>,
    faults: Mutex<BTreeMap<&'static str, VecDeque<Fault>>>,
    instances: Arc<AtomicUsize>,
    activations: Arc<AtomicUsize>,
    readers: Arc<AtomicUsize>,
    skip_configuration_update: AtomicBool,
}

impl ProbeState {
    fn fail(&self, hook: &'static str, fault: Fault) {
        self.faults
            .lock()
            .unwrap()
            .entry(hook)
            .or_default()
            .push_back(fault);
    }

    fn enter(&self, hook: &'static str) -> Option<Fault> {
        self.calls.lock().unwrap().push(hook);
        self.faults
            .lock()
            .unwrap()
            .get_mut(hook)
            .and_then(VecDeque::pop_front)
    }

    async fn call(&self, hook: &'static str) -> anyhow::Result<()> {
        match self.enter(hook) {
            Some(Fault::Error) => anyhow::bail!("injected {hook} failure"),
            Some(Fault::Pending) => std::future::pending().await,
            Some(Fault::Panic) => panic!("injected {hook} panic"),
            None => Ok(()),
        }
    }

    fn count(&self, hook: &'static str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| **call == hook)
            .count()
    }
}

struct Resource {
    count: Arc<AtomicUsize>,
    _allocation: Box<[u8]>,
}

impl Resource {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self {
            count,
            _allocation: vec![0; 128].into_boxed_slice(),
        }
    }
}

impl Drop for Resource {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct Options {
    role: ComponentRole,
    modes: Vec<ComponentUpdateMode>,
    planner: bool,
    apply: bool,
    plan: ComponentUpdatePlan,
    defaults: bool,
    consumer: bool,
    control: bool,
    reads: bool,
    wakeups: bool,
    deletion: bool,
    credentials: bool,
    transactional: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            role: ComponentRole::Source,
            modes: Vec::new(),
            planner: false,
            apply: false,
            plan: ComponentUpdatePlan {
                mode: ComponentUpdateMode::Restart,
                data: ComponentUpdateData::Preserve,
            },
            defaults: false,
            consumer: false,
            control: false,
            reads: false,
            wakeups: false,
            deletion: false,
            credentials: false,
            transactional: false,
        }
    }
}

impl Options {
    fn update(mode: ComponentUpdateMode) -> Self {
        Self {
            modes: vec![mode],
            planner: true,
            apply: mode != ComponentUpdateMode::Recreate,
            plan: ComponentUpdatePlan {
                mode,
                data: ComponentUpdateData::Preserve,
            },
            ..Self::default()
        }
    }
}

struct ProbeFactory {
    state: Arc<ProbeState>,
    options: Options,
}

impl Factory for ProbeFactory {
    fn metadata(&self) -> FactoryMetadata {
        let directions = match self.options.role {
            ComponentRole::Source => vec![("out", PortDirection::Output)],
            ComponentRole::Transformer => {
                vec![("in", PortDirection::Input), ("out", PortDirection::Output)]
            }
            ComponentRole::Sink => vec![("in", PortDirection::Input)],
            ComponentRole::Service => Vec::new(),
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new("fixture/lifecycle", "1").unwrap(),
            role: self.options.role,
            configuration_version: 1,
            configuration: crate::ConfigSchema::from_fields([(
                "value",
                crate::ConfigType::Integer,
                true,
            )]),
            ports: directions
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
            completion: (self.options.role == ComponentRole::Sink)
                .then_some(SinkCompletion::Handled),
            capabilities: crate::Capabilities {
                transactional: self.options.transactional,
                control: self.options.control,
                wakeups: self.options.wakeups,
                output_snapshots: if self.options.reads {
                    vec![PortId::try_new("out").unwrap()]
                } else {
                    Vec::new()
                },
                delete_owned_resources: self.options.deletion,
                update_modes: self.options.modes.clone(),
                ..Default::default()
            },
        }
    }

    fn supports_update_planning(&self) -> bool {
        self.options.planner
    }

    fn supports_activation_credentials(&self) -> bool {
        self.options.credentials
    }

    fn plan_update(&self, request: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan> {
        assert!(request.current.update_directive.is_none());
        assert!(request.proposed.update_directive.is_none());
        assert!(!request.current.activation_credentials_bound);
        assert!(!request.proposed.activation_credentials_bound);
        match self.state.enter("plan") {
            Some(Fault::Error | Fault::Pending) => anyhow::bail!("injected planner failure"),
            Some(Fault::Panic) => panic!("injected planner panic"),
            None => {}
        }
        anyhow::ensure!(
            request.proposed.configuration["value"].as_i64().unwrap() >= 0,
            "unsupported negative value"
        );
        Ok(self.options.plan)
    }

    fn consumer_mode(&self) -> Option<crate::ConsumerMode> {
        self.options
            .consumer
            .then_some(crate::ConsumerMode::External)
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        self.state.enter("create");
        self.state
            .directives
            .lock()
            .unwrap()
            .push(request.update_directive.clone());
        let descriptor = self.metadata().descriptor(request.id.clone())?;
        if self.options.defaults {
            return Ok(Component::Source(Box::new(Defaults {
                descriptor,
                configuration: request.configuration.clone(),
                apply: self.options.apply,
            }))
            .into());
        }
        let component = Box::new(Probe {
            descriptor,
            configuration: request.configuration.clone(),
            state: self.state.clone(),
            options: self.options.clone(),
            instance_resource: None,
            worker: None,
        });
        let component = if self.options.consumer {
            Component::Consumer(component)
        } else if self.options.transactional {
            Component::Transactional(component)
        } else {
            match self.options.role {
                ComponentRole::Source => Component::Source(component),
                ComponentRole::Transformer => Component::Transformer(component),
                ComponentRole::Sink => Component::Sink(component),
                ComponentRole::Service => Component::Service(component),
            }
        };
        Ok(CreatedComponent {
            component,
            control_handler: self.options.control.then(|| {
                Arc::new(Independent(self.state.clone())) as Arc<dyn NativeControlHandler>
            }),
        })
    }
}

struct Defaults {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    apply: bool,
}

#[async_trait]
impl ComputationComponent for Defaults {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    fn supports_apply_update(&self) -> bool {
        self.apply
    }
}

#[async_trait]
impl EnvelopeSource for Defaults {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}

struct Probe {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    state: Arc<ProbeState>,
    options: Options,
    instance_resource: Option<Resource>,
    worker: Option<Worker>,
}

struct Worker {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

#[async_trait]
impl ComputationComponent for Probe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    fn supports_apply_update(&self) -> bool {
        self.options.apply
    }
    fn supports_delete_owned_resources(&self) -> bool {
        self.options.deletion
    }

    fn output_readers(&self) -> OutputReaders {
        if self.options.reads {
            OutputReaders::new().with_snapshot(
                PortId::try_new("out").unwrap(),
                Arc::new(Independent(self.state.clone())),
            )
        } else {
            OutputReaders::default()
        }
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.instance_resource = Some(Resource::new(self.state.instances.clone()));
        self.state.call("initialize").await
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        let resource = Resource::new(self.state.activations.clone());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let _resource = resource;
            let _ = stopped.await;
        });
        self.worker = Some(Worker {
            stop: Some(stop),
            handle: worker,
        });
        self.state.call("start").await
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.state.call("stop").await?;
        if let Some(worker) = &mut self.worker {
            if let Some(stop) = worker.stop.take() {
                let _ = stop.send(());
            }
            (&mut worker.handle).await?;
        }
        self.worker = None;
        Ok(())
    }

    async fn start_with_credentials(
        &mut self,
        credentials: crate::ActivationCredentials,
    ) -> anyhow::Result<()> {
        credentials.validate()?;
        self.start().await
    }

    async fn dispose(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.worker.is_none(),
            "activation worker has not been joined"
        );
        anyhow::ensure!(
            self.state.readers.load(Ordering::SeqCst) == 0,
            "independent work is still using instance resources"
        );
        self.state.call("dispose").await?;
        self.instance_resource = None;
        Ok(())
    }

    async fn pause_processing(&mut self) -> anyhow::Result<()> {
        self.state.call("pause").await
    }
    async fn resume_processing(&mut self) -> anyhow::Result<()> {
        self.state.call("resume").await
    }

    async fn apply_update(&mut self, update: &ApplyUpdateRequest) -> anyhow::Result<()> {
        if !self.state.skip_configuration_update.load(Ordering::Acquire) {
            self.configuration = update.request.proposed.configuration.clone();
        }
        self.state.call("apply").await
    }

    async fn delete_owned_resources(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.worker.is_none(), "deletion raced activation cleanup");
        self.state.call("delete").await
    }
}

#[async_trait]
impl EnvelopeSource for Probe {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        self.options
            .reads
            .then(|| Arc::new(Independent(self.state.clone())) as Arc<dyn SourceSchemaReader>)
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.state.call("next").await?;
        Ok(None)
    }
}

#[async_trait]
impl Transformer for Probe {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        self.options
            .wakeups
            .then(|| Arc::new(Independent(self.state.clone())) as Arc<dyn WakeupSource>)
    }
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.state.call("transform").await?;
        Ok(Vec::new())
    }
}

#[async_trait]
impl crate::TransactionalComponent for Probe {
    fn transaction_input_schema(&self) -> Arc<drasi_computation_contracts::Schema> {
        GraphChangeCodec::schema()
    }
    fn transaction_output_schema(&self) -> Arc<drasi_computation_contracts::Schema> {
        GraphChangeCodec::schema()
    }
    async fn transform_in_transaction(
        &self,
        input: drasi_computation_contracts::ChangeEnvelope,
        context: &crate::NativeTransactionContext<'_>,
    ) -> anyhow::Result<drasi_computation_contracts::ChangeEnvelope> {
        anyhow::ensure!(self.worker.is_none(), "participant has a standalone worker");
        assert_eq!(context.step_id(), self.descriptor.id());
        self.state.call("transact").await?;
        assert_eq!(
            context.get("probe").await?,
            Some(drasi_core_types::models::ElementValue::Integer(7))
        );
        Ok(input)
    }
}

#[async_trait]
impl EnvelopeSink for Probe {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.state.call("handle").await
    }
}

#[async_trait]
impl DeliveryHandler for Probe {
    async fn handle(&mut self, _: DeliveryItem<'_>) -> anyhow::Result<()> {
        self.state.call("handle").await
    }
}

#[async_trait]
impl ComputationService for Probe {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.state.call("run").await
    }
}

struct Independent(Arc<ProbeState>);

impl Independent {
    async fn pending(&self, label: &'static str) {
        let _resource = Resource::new(self.0.readers.clone());
        self.0.enter(label);
        std::future::pending::<()>().await;
    }
}

#[async_trait]
impl NativeControlHandler for Independent {
    async fn on_message(&self, _: crate::ControlMessage, _: ControlSender) -> anyhow::Result<()> {
        self.pending("control").await;
        Ok(())
    }
}

#[async_trait]
impl SourceSchemaReader for Independent {
    async fn describe_schema(
        &self,
    ) -> anyhow::Result<Option<drasi_core_types::schema::SourceSchema>> {
        self.pending("schema").await;
        Ok(None)
    }
}

#[async_trait]
impl SnapshotProvider for Independent {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        self.pending("snapshot").await;
        unreachable!()
    }
}

#[async_trait]
impl WakeupSource for Independent {
    async fn wait(&self) -> anyhow::Result<()> {
        self.pending("wake").await;
        Ok(())
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
}

struct Harness {
    exported: ExportedPlugin,
    factory: abi::FactoryHandle,
    request: CreateRequest,
    state: Arc<ProbeState>,
    consumer: bool,
}

impl Harness {
    fn new(options: Options) -> anyhow::Result<Self> {
        let state = Arc::new(ProbeState::default());
        let consumer = options.consumer;
        let exported = ExportedPlugin::new(PluginDefinition::new(
            "fixture/native-lifecycle",
            "1",
            vec![Arc::new(ProbeFactory {
                state: state.clone(),
                options,
            })],
            vec![GraphChangeCodec::schema()],
        )?)?;
        let request = CreateRequest {
            id: ComponentId::try_new(ID)?,
            implementation: exported.state.definition.metadata.factories[0]
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: serde_json::json!({"value": 1}),
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
        unsafe {
            exported.entry(&mut plugin)?;
            transport::take_status((*plugin.vtable).factory.unwrap()(
                plugin.state,
                0,
                &mut factory,
            ))?;
            (*plugin.vtable).release.unwrap()(plugin.state);
            (*factory.vtable).header.validate::<abi::FactoryVTable>()?;
        }
        Ok(Self {
            exported,
            factory,
            request,
            state,
            consumer,
        })
    }

    fn create(&self) -> anyhow::Result<RawComponent> {
        let bytes = wire::encode_create_request(&self.request)?;
        self.create_bytes(&bytes)
    }

    fn create_bytes(&self, bytes: &[u8]) -> anyhow::Result<RawComponent> {
        let mut component = abi::ComponentHandle::null();
        unsafe {
            let create = if self.consumer {
                (*self.exported.consumer()).create.unwrap()
            } else {
                (*self.factory.vtable).create.unwrap()
            };
            let status = create(
                self.factory.state,
                BorrowedBytes::new(bytes),
                &mut component,
            );
            let result = transport::take_status(status);
            if result.is_err() {
                assert!(component.state.is_null() && component.vtable.is_null());
            }
            result?;
        }
        Ok(RawComponent(component))
    }

    fn request(&self, value: i64) -> UpdateRequest {
        let mut proposed = self.request.clone();
        proposed.configuration = serde_json::json!({"value": value});
        UpdateRequest {
            current: self.request.clone(),
            proposed,
        }
    }

    fn plan_bytes(&self, bytes: BorrowedBytes) -> Result<transport::ReceivedBytes, Failure> {
        unsafe {
            transport::take_reply((*self.factory.vtable).plan_update.unwrap()(
                self.factory.state,
                bytes,
            ))
        }
    }

    fn plan(&self, request: &UpdateRequest) -> anyhow::Result<ComponentUpdatePlan> {
        Ok(serde_json::from_slice(&self.plan_bytes(
            BorrowedBytes::new(&serde_json::to_vec(request)?),
        )?)?)
    }

    fn approved(&self) -> anyhow::Result<ApplyUpdateRequest> {
        let request = self.request(2);
        let plan = self.plan(&request)?;
        Ok(ApplyUpdateRequest {
            operation_id: "update-1".into(),
            request,
            plan,
        })
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        unsafe { (*self.factory.vtable).release.unwrap()(self.factory.state) }
    }
}

struct RawComponent(abi::ComponentHandle);

impl RawComponent {
    fn begin(&self, code: u32, input: &[u8]) -> Result<transport::OperationFuture, Failure> {
        self.begin_with_transaction(code, input, std::ptr::null())
    }

    fn begin_with_transaction(
        &self,
        code: u32,
        input: &[u8],
        transaction: *const abi::Transaction,
    ) -> Result<transport::OperationFuture, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            let status = (*self.0.vtable).begin.unwrap()(
                self.0.state,
                code,
                BorrowedBytes::new(input),
                transaction,
                &mut operation,
            );
            let result = transport::take_status(status);
            if result.is_err() {
                assert!(operation.state.is_null() && operation.vtable.is_null());
            }
            result?;
            transport::OperationFuture::new(operation)
        }
    }

    async fn call(&self, code: u32, input: &[u8]) -> Result<transport::ReceivedBytes, Failure> {
        tokio::time::timeout(Duration::from_secs(5), self.begin(code, input)?)
            .await
            .expect("native operation must settle")
    }

    async fn run(&self, code: u32) -> anyhow::Result<()> {
        assert!(self.call(code, &[]).await?.is_empty());
        Ok(())
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        unsafe {
            Ok(serde_json::from_slice(&transport::take_reply((*self
                .0
                .vtable)
                .configuration
                .unwrap()(
                self.0.state,
            ))?)?)
        }
    }

    async fn apply(&self, update: &ApplyUpdateRequest) -> anyhow::Result<()> {
        assert!(self
            .call(APPLY_UPDATE, &wire::encode(update)?)
            .await?
            .is_empty());
        Ok(())
    }

    async fn interrupted(&self, code: u32, fault: Fault) -> anyhow::Result<()> {
        match fault {
            Fault::Pending => {
                let mut operation = self.begin(code, &[])?;
                assert!(futures_util::poll!(&mut operation).is_pending());
                operation.cancel()?;
                assert_eq!(operation.await.unwrap_err().code, abi::status::CANCELLED);
            }
            Fault::Error => assert_eq!(
                self.call(code, &[]).await.unwrap_err().code,
                abi::status::FAILED
            ),
            Fault::Panic => assert_eq!(
                self.call(code, &[]).await.unwrap_err().code,
                abi::status::PANICKED
            ),
        }
        Ok(())
    }
}

impl Drop for RawComponent {
    fn drop(&mut self) {
        unsafe { (*self.0.vtable).release.unwrap()(self.0.state) }
    }
}

struct BorrowedRpc {
    active: AtomicBool,
    pending: bool,
    entered: tokio::sync::Notify,
    requests: AtomicUsize,
    retained: AtomicUsize,
    released: AtomicUsize,
    inflight: AtomicUsize,
}

impl BorrowedRpc {
    fn new(pending: bool) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            pending,
            entered: tokio::sync::Notify::new(),
            requests: AtomicUsize::new(0),
            retained: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
        })
    }

    fn table(self: &Arc<Self>) -> abi::Transaction {
        abi::Transaction {
            header: abi::Header::new::<abi::Transaction>(),
            context: Arc::as_ptr(self).cast_mut().cast(),
            retain: Some(retain_borrowed_rpc),
            release: Some(release_borrowed_rpc),
            request: Some(request_borrowed_rpc),
        }
    }
}

unsafe extern "C" fn retain_borrowed_rpc(context: *mut c_void) {
    transport::drop_boundary(|| unsafe {
        let pointer = context.cast::<BorrowedRpc>();
        Arc::increment_strong_count(pointer);
        (*pointer).retained.fetch_add(1, Ordering::SeqCst);
    });
}

unsafe extern "C" fn release_borrowed_rpc(context: *mut c_void) {
    transport::drop_boundary(|| unsafe {
        let pointer = context.cast::<BorrowedRpc>();
        (*pointer).released.fetch_add(1, Ordering::SeqCst);
        Arc::decrement_strong_count(pointer);
    });
}

struct InflightRpc(Arc<BorrowedRpc>);

impl Drop for InflightRpc {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

unsafe extern "C" fn request_borrowed_rpc(
    context: *mut c_void,
    code: u32,
    input: abi::BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        require_output(out)?;
        if code != abi::transaction::GET {
            return Err(Failure::protocol("unexpected witness RPC operation"));
        }
        let key: String =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? })
                .map_err(Failure::from)?;
        if key != "probe" {
            return Err(Failure::protocol("unexpected witness RPC key"));
        }
        unsafe { Arc::increment_strong_count(context.cast::<BorrowedRpc>()) };
        let owner = unsafe { Arc::from_raw(context.cast::<BorrowedRpc>()) };
        let operation = transport::export_operation(
            async move {
                owner.requests.fetch_add(1, Ordering::SeqCst);
                owner.inflight.fetch_add(1, Ordering::SeqCst);
                let _inflight = InflightRpc(owner.clone());
                owner.entered.notify_one();
                if owner.pending {
                    std::future::pending::<()>().await;
                }
                if !owner.active.load(Ordering::Acquire) {
                    return Err(Failure::closed());
                }
                wire::encode(&Some(drasi_core_types::models::ElementValue::Integer(7)))
                    .map_err(Failure::from)
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

fn participant_options() -> Options {
    Options {
        role: ComponentRole::Transformer,
        transactional: true,
        ..Options::default()
    }
}

fn borrowed_input() -> anyhow::Result<Vec<u8>> {
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    let input = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("fixture-data", "row"),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: 1,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"value": 7})),
            },
        },
        drasi_computation_contracts::StreamId::try_new("fixture/out")?,
        1,
        None,
    )?;
    Ok(wire::codec(&[GraphChangeCodec::schema()])?.encode(&input)?)
}

async fn borrowed_participant_enters_rpc() -> anyhow::Result<()> {
    let harness = Harness::new(participant_options())?;
    let component = harness.create()?;
    let rpc = BorrowedRpc::new(false);
    let transaction = rpc.table();
    let input = borrowed_input()?;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        component.begin_with_transaction(TRANSACT, &input, &transaction)?,
    )
    .await??;
    let codec = wire::codec(&[GraphChangeCodec::schema()])?;
    let expected = codec.decode(&input)?;
    let actual = codec.decode(&output)?;
    assert_eq!(actual.changes().id(), expected.changes().id());
    assert_eq!(
        actual.changes().operations(),
        expected.changes().operations()
    );
    assert_eq!(actual.system().stream(), expected.system().stream());
    assert_eq!(actual.system().sequence(), expected.system().sequence());
    assert_eq!(harness.state.count("transact"), 1);
    assert_eq!(rpc.requests.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.inflight.load(Ordering::SeqCst), 0);
    assert_eq!(rpc.retained.load(Ordering::SeqCst), 1);
    assert_eq!(rpc.released.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&rpc), 1);
    for hook in ["initialize", "start", "stop", "pause", "resume", "dispose"] {
        assert_eq!(harness.state.count(hook), 0, "{hook}");
    }
    assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
    assert_eq!(harness.state.activations.load(Ordering::SeqCst), 0);
    assert!(component.begin(START, &[]).is_err());
    assert!(component.begin(TRANSFORM, &input).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn borrowed_transact_enters_real_rpc_without_standalone_lifecycle_current_thread(
) -> anyhow::Result<()> {
    borrowed_participant_enters_rpc().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn borrowed_transact_enters_real_rpc_without_standalone_lifecycle_multi_thread(
) -> anyhow::Result<()> {
    borrowed_participant_enters_rpc().await
}

#[tokio::test(flavor = "current_thread")]
async fn borrowed_transact_preserves_lifecycle_fences_busy_revocation_and_cancellation(
) -> anyhow::Result<()> {
    let input = borrowed_input()?;
    for phase in [
        "initialize",
        "start",
        "pause",
        "resume",
        "dispose",
        "panic",
        "update",
    ] {
        let options = if phase == "update" {
            Options {
                role: ComponentRole::Transformer,
                transactional: true,
                ..Options::update(ComponentUpdateMode::InPlace)
            }
        } else {
            participant_options()
        };
        let harness = Harness::new(options)?;
        let component = harness.create()?;
        match phase {
            "initialize" => {
                harness.state.fail("initialize", Fault::Error);
                component.interrupted(INITIALIZE, Fault::Error).await?;
            }
            "start" => {
                component.run(INITIALIZE).await?;
                harness.state.fail("start", Fault::Pending);
                component.interrupted(START, Fault::Pending).await?;
            }
            "pause" | "resume" => {
                component.run(INITIALIZE).await?;
                component.run(START).await?;
                component.run(PAUSE_PROCESSING).await?;
                if phase == "resume" {
                    harness.state.fail("resume", Fault::Pending);
                    component
                        .interrupted(RESUME_PROCESSING, Fault::Pending)
                        .await?;
                }
            }
            "dispose" => component.run(DISPOSE).await?,
            "panic" => {
                harness.state.fail("initialize", Fault::Panic);
                component.interrupted(INITIALIZE, Fault::Panic).await?;
            }
            "update" => {
                component.run(INITIALIZE).await?;
                component.run(START).await?;
                component.run(PAUSE_PROCESSING).await?;
                let update = harness.approved()?;
                harness.state.fail("apply", Fault::Pending);
                let mut work = component.begin(APPLY_UPDATE, &wire::encode(&update)?)?;
                assert!(futures_util::poll!(&mut work).is_pending());
                work.cancel()?;
                assert_eq!(work.await.unwrap_err().code, abi::status::CANCELLED);
            }
            _ => unreachable!(),
        }
        let rpc = BorrowedRpc::new(false);
        let error = component
            .begin_with_transaction(TRANSACT, &input, &rpc.table())
            .err()
            .expect("unfinished, active, paused or retired participant must reject");
        let expected = match phase {
            "dispose" => abi::status::CLOSED,
            "panic" => abi::status::PANICKED,
            _ => abi::status::FAILED,
        };
        assert_eq!(error.code, expected, "{phase}: {error}");
        assert_eq!(harness.state.count("transact"), 0);
        assert_eq!(rpc.requests.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&rpc), 1);
        component.run(STOP).await?;
        component.run(DISPOSE).await?;
    }

    let harness = Harness::new(participant_options())?;
    let component = harness.create()?;
    let rpc = BorrowedRpc::new(true);
    let transaction = rpc.table();
    let ordinary = Harness::new(Options {
        role: ComponentRole::Transformer,
        ..Options::default()
    })?;
    let ordinary_component = ordinary.create()?;
    assert_eq!(
        ordinary_component
            .begin_with_transaction(TRANSACT, &input, &transaction)
            .err()
            .unwrap()
            .code,
        abi::status::UNSUPPORTED
    );
    assert_eq!(
        component.begin(TRANSACT, &input).err().unwrap().code,
        abi::status::PROTOCOL
    );
    let mut invalid = transaction;
    invalid.request = None;
    assert_eq!(
        component
            .begin_with_transaction(TRANSACT, &input, &invalid)
            .err()
            .unwrap()
            .code,
        abi::status::PROTOCOL
    );
    let mut work = component.begin_with_transaction(TRANSACT, &input, &transaction)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut work => anyhow::bail!("participant ended before real pending RPC: {result:?}"),
            _ = rpc.entered.notified() => Ok::<_, anyhow::Error>(()),
        }
    }).await??;
    assert_eq!(rpc.inflight.load(Ordering::SeqCst), 1);
    assert_eq!(
        component
            .begin_with_transaction(TRANSACT, &input, &transaction)
            .err()
            .unwrap()
            .code,
        abi::status::BUSY
    );
    rpc.active.store(false, Ordering::Release);
    work.cancel()?;
    assert_eq!(work.await.unwrap_err().code, abi::status::CANCELLED);
    assert_eq!(rpc.inflight.load(Ordering::SeqCst), 0);
    assert_eq!(rpc.requests.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&rpc), 1);
    assert_eq!(
        rpc.retained.load(Ordering::SeqCst),
        rpc.released.load(Ordering::SeqCst)
    );
    let revoked = BorrowedRpc::new(false);
    revoked.active.store(false, Ordering::Release);
    let failure = component
        .begin_with_transaction(TRANSACT, &input, &revoked.table())?
        .await
        .unwrap_err();
    assert_eq!(failure.code, abi::status::CLOSED);
    assert_eq!(revoked.requests.load(Ordering::SeqCst), 1);
    assert_eq!(revoked.inflight.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&revoked), 1);
    assert_eq!(
        revoked.retained.load(Ordering::SeqCst),
        revoked.released.load(Ordering::SeqCst)
    );
    assert_eq!(harness.state.count("start"), 0);
    Ok(())
}

async fn lifecycle_cycles() -> anyhow::Result<()> {
    let harness = Harness::new(Options::default())?;
    let component = harness.create()?;
    assert!(component.begin(START, &[]).is_err());
    component.run(INITIALIZE).await?;
    component.run(INITIALIZE).await?;
    for cycle in 0..2 {
        component.run(START).await?;
        component.run(START).await?;
        if cycle == 0 {
            component.run(PAUSE_PROCESSING).await?;
            component.run(PAUSE_PROCESSING).await?;
            assert!(component.begin(NEXT, &[]).is_err());
            assert_eq!(harness.state.activations.load(Ordering::SeqCst), 1);
            component.run(RESUME_PROCESSING).await?;
            component.run(RESUME_PROCESSING).await?;
        }
        component.run(STOP).await?;
        component.run(STOP).await?;
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 1);
        assert_eq!(harness.state.activations.load(Ordering::SeqCst), 0);
    }
    component.run(DISPOSE).await?;
    component.run(DISPOSE).await?;
    assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
    assert_eq!(
        *harness.state.calls.lock().unwrap(),
        vec![
            "create",
            "initialize",
            "start",
            "pause",
            "resume",
            "stop",
            "start",
            "stop",
            "dispose"
        ]
    );
    assert_eq!(
        component.begin(START, &[]).err().unwrap().code,
        abi::status::CLOSED
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn exported_lifetime_order_and_owned_resources_current_thread() -> anyhow::Result<()> {
    lifecycle_cycles().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exported_lifetime_order_and_owned_resources_multi_thread() -> anyhow::Result<()> {
    lifecycle_cycles().await
}

#[tokio::test]
async fn defaults_and_disposal_without_activation_are_real_exported_operations(
) -> anyhow::Result<()> {
    let harness = Harness::new(Options {
        defaults: true,
        ..Default::default()
    })?;
    let component = harness.create()?;
    for code in [
        INITIALIZE,
        INITIALIZE,
        START,
        START,
        PAUSE_PROCESSING,
        RESUME_PROCESSING,
        STOP,
        STOP,
        DISPOSE,
        DISPOSE,
    ] {
        component.run(code).await?;
    }
    assert_eq!(
        component.begin(APPLY_UPDATE, &[]).err().unwrap().code,
        abi::status::UNSUPPORTED
    );
    for initialized in [false, true] {
        let harness = Harness::new(Options::default())?;
        let component = harness.create()?;
        if initialized {
            component.run(INITIALIZE).await?;
        }
        component.run(DISPOSE).await?;
        component.run(DISPOSE).await?;
        assert_eq!(harness.state.count("dispose"), 1);
        assert_eq!(harness.state.count("start"), 0);
        assert_eq!(harness.state.count("stop"), 0);
    }
    let harness = Harness::new(Options::default())?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    drop(component);
    assert_eq!(
        harness.state.count("dispose"),
        0,
        "synchronous release never substitutes for disposal"
    );
    Ok(())
}

#[tokio::test]
async fn failed_cancelled_and_panicked_initialization_requires_disposal() -> anyhow::Result<()> {
    for fault in [Fault::Error, Fault::Pending, Fault::Panic] {
        let harness = Harness::new(Options::default())?;
        let component = harness.create()?;
        harness.state.fail("initialize", fault);
        component.interrupted(INITIALIZE, fault).await?;
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 1);
        assert!(component.begin(INITIALIZE, &[]).is_err());
        assert!(component.begin(START, &[]).is_err());
        component.run(DISPOSE).await?;
        component.run(DISPOSE).await?;
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
        assert_eq!(harness.state.count("initialize"), 1);
        assert_eq!(harness.state.count("dispose"), 1);
    }
    let harness = Harness::new(Options::default())?;
    let component = harness.create()?;
    drop(component.begin(INITIALIZE, &[])?);
    assert!(component.begin(INITIALIZE, &[]).is_err());
    component.run(DISPOSE).await?;
    assert_eq!(harness.state.count("initialize"), 0);
    Ok(())
}

#[tokio::test]
async fn partial_start_and_cleanup_failures_keep_the_worker_owned_until_retry() -> anyhow::Result<()>
{
    for consumer in [false, true] {
        for fault in [Fault::Error, Fault::Pending] {
            let harness = Harness::new(Options {
                consumer,
                role: if consumer {
                    ComponentRole::Sink
                } else {
                    ComponentRole::Source
                },
                ..Default::default()
            })?;
            let component = harness.create()?;
            component.run(INITIALIZE).await?;
            harness.state.fail("start", fault);
            component.interrupted(START, fault).await?;
            assert_eq!(harness.state.activations.load(Ordering::SeqCst), 1);
            assert!(component.begin(START, &[]).is_err());
            assert!(component.begin(DISPOSE, &[]).is_err());
            harness.state.fail("stop", fault);
            component.interrupted(STOP, fault).await?;
            assert_eq!(harness.state.activations.load(Ordering::SeqCst), 1);
            assert!(component.begin(START, &[]).is_err());
            component.run(STOP).await?;
            component.run(STOP).await?;
            assert_eq!(harness.state.activations.load(Ordering::SeqCst), 0);
            component.run(START).await?;
            component.run(PAUSE_PROCESSING).await?;
            component.run(STOP).await?;
            assert_eq!(
                harness.state.count("resume"),
                0,
                "stop does not resume a paused activation"
            );
            component.run(DISPOSE).await?;
            assert_eq!(harness.state.count("initialize"), 1);
            assert_eq!(harness.state.count("dispose"), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn incomplete_disposal_pause_and_resume_are_retryable_not_restarts() -> anyhow::Result<()> {
    for fault in [Fault::Error, Fault::Pending] {
        let harness = Harness::new(Options::default())?;
        let component = harness.create()?;
        component.run(INITIALIZE).await?;
        component.run(START).await?;
        harness.state.fail("pause", fault);
        component.interrupted(PAUSE_PROCESSING, fault).await?;
        assert!(component.begin(NEXT, &[]).is_err());
        assert!(component.begin(RESUME_PROCESSING, &[]).is_err());
        component.run(PAUSE_PROCESSING).await?;
        harness.state.fail("resume", fault);
        component.interrupted(RESUME_PROCESSING, fault).await?;
        assert!(component.begin(NEXT, &[]).is_err());
        component.run(RESUME_PROCESSING).await?;
        component.run(STOP).await?;
        harness.state.fail("dispose", fault);
        component.interrupted(DISPOSE, fault).await?;
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 1);
        assert!(component.begin(START, &[]).is_err());
        assert!(component.begin(INITIALIZE, &[]).is_err());
        component.run(DISPOSE).await?;
        component.run(DISPOSE).await?;
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
        assert_eq!(harness.state.count("dispose"), 2);
    }
    Ok(())
}

async fn retained_updates() -> anyhow::Result<()> {
    for mode in [ComponentUpdateMode::InPlace, ComponentUpdateMode::Restart] {
        let harness = Harness::new(Options::update(mode))?;
        let update = harness.approved()?;
        assert_eq!(
            harness.state.count("create"),
            0,
            "planning must not construct an object"
        );
        assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
        let component = harness.create()?;
        component.run(INITIALIZE).await?;
        component.run(START).await?;
        assert!(component
            .begin(APPLY_UPDATE, &wire::encode(&update)?)
            .is_err());
        component
            .run(if mode == ComponentUpdateMode::InPlace {
                PAUSE_PROCESSING
            } else {
                STOP
            })
            .await?;
        component.apply(&update).await?;
        assert_eq!(
            component.configuration()?,
            update.request.proposed.configuration
        );
        component
            .run(if mode == ComponentUpdateMode::InPlace {
                RESUME_PROCESSING
            } else {
                START
            })
            .await?;
        component.apply(&update).await?;
        assert_eq!(
            harness.state.count("apply"),
            1,
            "completed intent is acknowledged without a second mutation"
        );
        assert_eq!(harness.state.count("initialize"), 1);
        assert_eq!(harness.state.count("dispose"), 0);
        component.run(STOP).await?;
        component.run(DISPOSE).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_updates_cross_the_real_abi_current_thread() -> anyhow::Result<()> {
    retained_updates().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_updates_cross_the_real_abi_multi_thread() -> anyhow::Result<()> {
    retained_updates().await
}

#[tokio::test]
async fn interrupted_updates_keep_explicit_intent_without_panic_quarantine_or_rollback(
) -> anyhow::Result<()> {
    for fault in [Fault::Error, Fault::Pending] {
        let harness = Harness::new(Options::update(ComponentUpdateMode::Restart))?;
        let component = harness.create()?;
        component.run(INITIALIZE).await?;
        let update = harness.approved()?;
        harness.state.fail("apply", fault);
        let mut operation = component.begin(APPLY_UPDATE, &wire::encode(&update)?)?;
        if matches!(fault, Fault::Pending) {
            assert!(futures_util::poll!(&mut operation).is_pending());
            operation.cancel()?;
            assert_eq!(operation.await.unwrap_err().code, abi::status::CANCELLED);
        } else {
            assert_eq!(operation.await.unwrap_err().code, abi::status::FAILED);
        }
        assert_eq!(
            component.configuration()?,
            update.request.proposed.configuration,
            "partial mutation is not rolled back"
        );
        assert!(component.begin(START, &[]).is_err());
        component.run(STOP).await?;
        assert!(
            component.begin(START, &[]).is_err(),
            "stop does not declare an interrupted update successful"
        );
        let mut conflicting = update.clone();
        conflicting.operation_id = "other-update".into();
        assert!(component.apply(&conflicting).await.is_err());
        component.apply(&update).await?;
        component.apply(&update).await?;
        assert_eq!(harness.state.count("apply"), 2);
        component.run(START).await?;
        component.run(STOP).await?;
        component.run(DISPOSE).await?;
    }
    Ok(())
}

#[tokio::test]
async fn accepted_unpolled_start_stop_dispose_and_update_remain_fenced() -> anyhow::Result<()> {
    let harness = Harness::new(Options::update(ComponentUpdateMode::Restart))?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    drop(component.begin(START, &[])?);
    assert!(component.begin(START, &[]).is_err());
    drop(component.begin(STOP, &[])?);
    assert!(component.begin(START, &[]).is_err());
    component.run(STOP).await?;
    let update = harness.approved()?;
    drop(component.begin(APPLY_UPDATE, &wire::encode(&update)?)?);
    assert!(component.begin(START, &[]).is_err());
    component.apply(&update).await?;
    assert_eq!(harness.state.count("apply"), 1);
    drop(component.begin(DISPOSE, &[])?);
    assert!(component.begin(START, &[]).is_err());
    component.run(DISPOSE).await?;
    assert_eq!(harness.state.count("dispose"), 1);
    Ok(())
}

#[tokio::test]
async fn no_op_or_wrong_configuration_apply_is_not_reported_as_success() -> anyhow::Result<()> {
    let harness = Harness::new(Options {
        defaults: true,
        ..Options::update(ComponentUpdateMode::Restart)
    })?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    let update = harness.approved()?;
    assert_eq!(
        component
            .call(APPLY_UPDATE, &wire::encode(&update)?)
            .await
            .unwrap_err()
            .code,
        abi::status::UNSUPPORTED
    );
    assert!(component.begin(START, &[]).is_err());
    component.run(DISPOSE).await?;

    let harness = Harness::new(Options::update(ComponentUpdateMode::Restart))?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    harness
        .state
        .skip_configuration_update
        .store(true, Ordering::Release);
    let update = harness.approved()?;
    assert!(component
        .apply(&update)
        .await
        .unwrap_err()
        .to_string()
        .contains("proposed configuration"));
    assert_eq!(component.configuration()?, harness.request.configuration);
    harness
        .state
        .skip_configuration_update
        .store(false, Ordering::Release);
    component.apply(&update).await?;
    component.run(DISPOSE).await?;
    Ok(())
}

#[tokio::test]
async fn metadata_planners_and_retained_hooks_must_match_without_requiring_recreate_hooks(
) -> anyhow::Result<()> {
    assert!(Harness::new(Options {
        wakeups: true,
        ..Options::update(ComponentUpdateMode::Restart)
    })
    .is_err());
    assert!(Harness::new(Options {
        planner: false,
        ..Options::update(ComponentUpdateMode::Restart)
    })
    .is_err());
    assert!(Harness::new(Options {
        planner: true,
        ..Options::default()
    })
    .is_err());
    assert!(Harness::new(Options {
        modes: vec![ComponentUpdateMode::Restart; 2],
        ..Options::update(ComponentUpdateMode::Restart)
    })
    .is_err());
    for options in [
        Options {
            apply: false,
            ..Options::update(ComponentUpdateMode::Restart)
        },
        Options {
            apply: true,
            ..Options::default()
        },
    ] {
        assert!(Harness::new(options)?.create().is_err());
    }
    let harness = Harness::new(Options::update(ComponentUpdateMode::Recreate))?;
    let component = harness.create()?;
    let update = harness.approved()?;
    assert_eq!(update.plan.mode, ComponentUpdateMode::Recreate);
    assert_eq!(
        component
            .begin(APPLY_UPDATE, &wire::encode(&update)?)
            .err()
            .unwrap()
            .code,
        abi::status::UNSUPPORTED
    );
    component.run(DISPOSE).await?;
    let harness = Harness::new(Options::default())?;
    assert_eq!(
        harness
            .plan_bytes(BorrowedBytes::new(&serde_json::to_vec(
                &harness.request(2)
            )?))
            .unwrap_err()
            .code,
        abi::status::UNSUPPORTED
    );
    Ok(())
}

#[tokio::test]
async fn forged_identity_configuration_and_plans_are_rejected_without_mutation(
) -> anyhow::Result<()> {
    let harness = Harness::new(Options {
        modes: vec![
            ComponentUpdateMode::InPlace,
            ComponentUpdateMode::Restart,
            ComponentUpdateMode::Recreate,
        ],
        ..Options::update(ComponentUpdateMode::Restart)
    })?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    let update = harness.approved()?;
    let mut invalid = Vec::new();
    let mut changed = update.clone();
    changed.request.current.id = ComponentId::try_new(OTHER_ID)?;
    changed.request.proposed.id = changed.request.current.id.clone();
    invalid.push(changed);
    let mut changed = update.clone();
    changed.request.current.scope.as_mut().unwrap().generation += 1;
    changed.request.proposed.scope = changed.request.current.scope.clone();
    invalid.push(changed);
    let mut changed = update.clone();
    changed.request.current.configuration = serde_json::json!({"value": 42});
    invalid.push(changed);
    let mut changed = update.clone();
    changed.request.proposed.implementation =
        ImplementationIdentity::try_new("different/factory", "1")?;
    invalid.push(changed);
    let mut changed = update.clone();
    changed.request.proposed.configuration_version = 2;
    invalid.push(changed);
    let mut changed = update.clone();
    changed.request.proposed.configuration = serde_json::json!({"value": "wrong type"});
    invalid.push(changed);
    let mut changed = update.clone();
    changed.plan.data = ComponentUpdateData::Rebootstrap;
    invalid.push(changed);
    let mut changed = update.clone();
    changed.plan.mode = ComponentUpdateMode::InPlace;
    invalid.push(changed);
    let mut changed = update.clone();
    changed.plan.mode = ComponentUpdateMode::Recreate;
    invalid.push(changed);
    for changed in invalid {
        assert!(component.apply(&changed).await.is_err());
    }
    assert_eq!(harness.state.count("apply"), 0);
    component.apply(&update).await?;
    let mut reused = update.clone();
    reused.request.current = reused.request.proposed.clone();
    reused.request.proposed.configuration = serde_json::json!({"value": 3});
    assert!(component.apply(&reused).await.is_err());
    component.run(DISPOSE).await?;
    Ok(())
}

#[test]
fn factory_planning_is_bounded_json_pure_and_panic_contained() -> anyhow::Result<()> {
    let harness = Harness::new(Options::update(ComponentUpdateMode::Restart))?;
    let request = harness.request(2);
    assert_eq!(harness.plan(&request)?, Options::default().plan);
    assert!(harness
        .plan_bytes(BorrowedBytes::new(&wire::encode(&request)?))
        .is_err());
    assert!(harness.plan_bytes(BorrowedBytes::new(b"{}")).is_err());
    let mut unknown = serde_json::to_value(&request)?;
    unknown["force"] = serde_json::json!(true);
    assert!(harness
        .plan_bytes(BorrowedBytes::new(&serde_json::to_vec(&unknown)?))
        .is_err());
    assert!(harness
        .plan_bytes(BorrowedBytes {
            data: std::ptr::null(),
            len: abi::MAX_METADATA_BYTES + 1
        })
        .is_err());
    harness.state.fail("plan", Fault::Panic);
    assert_eq!(
        harness
            .plan_bytes(BorrowedBytes::new(&serde_json::to_vec(&request)?))
            .unwrap_err()
            .code,
        abi::status::PANICKED
    );
    assert!(harness.plan(&harness.request(-1)).is_err());
    assert_eq!(harness.state.count("create"), 0);
    assert_eq!(harness.state.count("initialize"), 0);
    Ok(())
}

fn control_message() -> anyhow::Result<Vec<u8>> {
    wire::encode(&crate::ControlMessage {
        from: ComponentId::try_new(OTHER_ID)?,
        generation: 1,
        direction: crate::ControlDirection::Downstream,
        notification: crate::ControlNotification::Ready,
    })
}

#[tokio::test]
async fn independent_control_schema_and_output_calls_are_settled_before_cleanup(
) -> anyhow::Result<()> {
    let harness = Harness::new(Options {
        control: true,
        reads: true,
        ..Default::default()
    })?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    component.run(START).await?;
    harness.state.fail("next", Fault::Pending);
    let mut next = component.begin(NEXT, &[])?;
    assert!(futures_util::poll!(&mut next).is_pending());
    let mut control = component.begin(CONTROL, &control_message()?)?;
    let mut schema = abi::OperationHandle::null();
    let mut read_handle = abi::output_reads::OutputReadHandle::null();
    let mut read = abi::OperationHandle::null();
    unsafe {
        transport::take_status((*harness.exported.source_schema()).begin.unwrap()(
            component.0.state,
            &mut schema,
        ))?;
        transport::take_status((*harness.exported.output_reads()).open.unwrap()(
            component.0.state,
            BorrowedBytes::new(b"out"),
            abi::output_reads::capability::SNAPSHOT,
            &mut read_handle,
        ))?;
        transport::take_status((*read_handle.vtable).request.unwrap()(
            read_handle.state,
            abi::output_reads::operation::SNAPSHOT,
            BorrowedBytes::empty(),
            &mut read,
        ))?;
    }
    let mut schema = unsafe { transport::OperationFuture::new(schema)? };
    let mut read = unsafe { transport::OperationFuture::new(read)? };
    assert!(futures_util::poll!(&mut control).is_pending());
    assert!(futures_util::poll!(&mut schema).is_pending());
    assert!(futures_util::poll!(&mut read).is_pending());
    assert_eq!(harness.state.readers.load(Ordering::SeqCst), 3);
    assert_eq!(
        component.begin(STOP, &[]).err().unwrap().code,
        abi::status::BUSY
    );
    drop(next);
    let mut stop = component.begin(STOP, &[])?;
    assert!(futures_util::poll!(&mut stop).is_pending());
    assert_eq!(harness.state.count("stop"), 0);
    assert_eq!(control.await.unwrap_err().code, abi::status::CLOSED);
    assert_eq!(schema.await.unwrap_err().code, abi::status::CLOSED);
    assert_eq!(read.await.unwrap_err().code, abi::status::CLOSED);
    tokio::time::timeout(Duration::from_secs(5), stop).await??;
    assert_eq!(harness.state.readers.load(Ordering::SeqCst), 0);
    component.run(DISPOSE).await?;
    unsafe {
        let mut rejected = abi::OperationHandle::null();
        assert_eq!(
            transport::take_status((*read_handle.vtable).request.unwrap()(
                read_handle.state,
                abi::output_reads::operation::SNAPSHOT,
                BorrowedBytes::empty(),
                &mut rejected,
            ))
            .unwrap_err()
            .code,
            abi::status::CLOSED
        );
        (*read_handle.vtable).release.unwrap()(read_handle.state);
    }
    Ok(())
}

#[tokio::test]
async fn disposal_fences_accepted_independent_calls_even_without_initialization(
) -> anyhow::Result<()> {
    let harness = Harness::new(Options {
        control: true,
        ..Default::default()
    })?;
    let component = harness.create()?;
    let mut control = component.begin(CONTROL, &control_message()?)?;
    assert!(futures_util::poll!(&mut control).is_pending());
    let mut dispose = component.begin(DISPOSE, &[])?;
    assert!(futures_util::poll!(&mut dispose).is_pending());
    assert_eq!(harness.state.count("dispose"), 0);
    drop(dispose);
    assert!(component.begin(CONTROL, &control_message()?).is_err());
    assert!(component.begin(INITIALIZE, &[]).is_err());
    assert_eq!(control.await.unwrap_err().code, abi::status::CLOSED);
    component.run(DISPOSE).await?;
    assert_eq!(harness.state.count("dispose"), 1);
    Ok(())
}

#[tokio::test]
async fn wakeups_remain_independent_and_service_run_is_not_a_lifecycle_alias() -> anyhow::Result<()>
{
    let harness = Harness::new(Options {
        role: ComponentRole::Transformer,
        wakeups: true,
        ..Default::default()
    })?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    component.run(START).await?;
    let mut wake = component.begin(WAKE_WAIT, &[])?;
    assert!(futures_util::poll!(&mut wake).is_pending());
    assert!(!wire::decode::<bool>(
        &component.call(WAKE_PENDING, &[]).await?
    )?);
    let mut stop = component.begin(STOP, &[])?;
    assert!(futures_util::poll!(&mut stop).is_pending());
    assert_eq!(wake.await.unwrap_err().code, abi::status::CLOSED);
    stop.await?;
    component.run(DISPOSE).await?;
    let harness = Harness::new(Options {
        role: ComponentRole::Service,
        ..Default::default()
    })?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    component.run(START).await?;
    component.run(RUN).await?;
    component.run(PAUSE_PROCESSING).await?;
    assert!(component.begin(RUN, &[]).is_err());
    component.run(RESUME_PROCESSING).await?;
    component.run(STOP).await?;
    component.run(DISPOSE).await?;
    assert_eq!(
        *harness.state.calls.lock().unwrap(),
        vec![
            "create",
            "initialize",
            "start",
            "run",
            "pause",
            "resume",
            "stop",
            "dispose"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn explicit_deletion_must_complete_before_disposal_and_is_never_an_update_side_effect(
) -> anyhow::Result<()> {
    for fault in [Fault::Error, Fault::Pending] {
        let harness = Harness::new(Options {
            deletion: true,
            ..Options::update(ComponentUpdateMode::Restart)
        })?;
        let component = harness.create()?;
        component.run(INITIALIZE).await?;
        component.apply(&harness.approved()?).await?;
        assert_eq!(harness.state.count("delete"), 0);
        component.run(START).await?;
        assert!(component.begin(DELETE_OWNED_RESOURCES, &[]).is_err());
        component.run(STOP).await?;
        harness.state.fail("delete", fault);
        component.interrupted(DELETE_OWNED_RESOURCES, fault).await?;
        assert!(component.begin(DISPOSE, &[]).is_err());
        assert!(component.begin(START, &[]).is_err());
        component.run(DELETE_OWNED_RESOURCES).await?;
        component.run(DELETE_OWNED_RESOURCES).await?;
        component.run(DISPOSE).await?;
        assert_eq!(harness.state.count("delete"), 2);
        assert_eq!(harness.state.count("dispose"), 1);
    }
    Ok(())
}

#[tokio::test]
async fn reconstruction_directives_reach_actual_factories_without_becoming_planning_permissions(
) -> anyhow::Result<()> {
    let mut harness = Harness::new(Options::update(ComponentUpdateMode::Recreate))?;
    let initial = harness.create()?;
    initial.run(DISPOSE).await?;
    assert_eq!(*harness.state.directives.lock().unwrap(), vec![None]);
    let directive = crate::UpdateDirective {
        operation_id: "accepted-reconstruction".into(),
        plan: ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        },
    };
    harness.request.update_directive = Some(directive.clone());
    let described = serde_json::to_value(&harness.request)?;
    assert!(described.get("update_directive").is_none());
    let component = harness.create()?;
    assert_eq!(
        harness.state.directives.lock().unwrap().last(),
        Some(&Some(directive.clone()))
    );
    assert_eq!(component.configuration()?, serde_json::json!({"value": 1}));
    assert_eq!(
        harness.plan(&harness.request(2))?.mode,
        ComponentUpdateMode::Recreate
    );
    component.run(INITIALIZE).await?;
    component.run(START).await?;
    component.run(STOP).await?;
    component.run(DISPOSE).await?;

    let unsupported = Harness::new(Options::default())?;
    let envelope = |directive: crate::UpdateDirective, version: u32| {
        serde_json::json!({
            "update_creation_version": version,
            "request": unsupported.request,
            "directive": directive,
        })
    };
    let bytes = wire::encode(&envelope(directive.clone(), 1))?;
    assert!(unsupported.create_bytes(&bytes).is_err());
    assert_eq!(unsupported.state.count("create"), 0);

    let make = |directive: crate::UpdateDirective, version: u32| {
        wire::encode(&serde_json::json!({
            "update_creation_version": version,
            "request": harness.request,
            "directive": directive,
        }))
    };
    let mut invalid = directive.clone();
    invalid.operation_id.clear();
    assert!(harness.create_bytes(&make(invalid, 1)?).is_err());
    let mut invalid = directive.clone();
    invalid.plan.mode = ComponentUpdateMode::Restart;
    assert!(harness.create_bytes(&make(invalid, 1)?).is_err());
    assert!(harness.create_bytes(&make(directive.clone(), 2)?).is_err());
    let mut inline = described;
    inline["update_directive"] = serde_json::to_value(&directive)?;
    assert!(
        harness.create_bytes(&wire::encode(&inline)?).is_err(),
        "execution metadata cannot enter an untagged description"
    );
    assert_eq!(
        harness.state.count("create"),
        2,
        "invalid directives must fail before construction"
    );
    Ok(())
}

#[tokio::test]
async fn panic_quarantine_still_allows_retrying_cleanup_but_never_reactivation(
) -> anyhow::Result<()> {
    let harness = Harness::new(Options::default())?;
    let component = harness.create()?;
    component.run(INITIALIZE).await?;
    component.run(START).await?;
    harness.state.fail("stop", Fault::Panic);
    component.interrupted(STOP, Fault::Panic).await?;
    assert_eq!(harness.state.activations.load(Ordering::SeqCst), 1);
    component.run(STOP).await?;
    assert_eq!(
        component.begin(START, &[]).err().unwrap().code,
        abi::status::PANICKED
    );
    harness.state.fail("dispose", Fault::Panic);
    component.interrupted(DISPOSE, Fault::Panic).await?;
    assert_eq!(harness.state.instances.load(Ordering::SeqCst), 1);
    component.run(DISPOSE).await?;
    component.run(DISPOSE).await?;
    assert_eq!(harness.state.instances.load(Ordering::SeqCst), 0);
    assert_eq!(harness.state.count("dispose"), 2);
    Ok(())
}

#[tokio::test]
async fn credential_bound_updates_recheck_the_same_description_only_factory_plan(
) -> anyhow::Result<()> {
    for reconstructed in [false, true] {
        let mut harness = Harness::new(Options {
            credentials: true,
            modes: vec![ComponentUpdateMode::Restart, ComponentUpdateMode::Recreate],
            ..Options::update(ComponentUpdateMode::Restart)
        })?;
        harness.request.activation_credentials_bound = true;
        harness.request.update_directive = reconstructed.then(|| crate::UpdateDirective {
            operation_id: "previous-reconstruction".into(),
            plan: ComponentUpdatePlan {
                mode: ComponentUpdateMode::Recreate,
                data: ComponentUpdateData::Preserve,
            },
        });
        let component = harness.create()?;
        let update = harness.approved()?;
        assert!(update.request.current.activation_credentials_bound);
        assert_eq!(
            update.request.current.update_directive.is_some(),
            reconstructed
        );
        let description: UpdateRequest =
            serde_json::from_slice(&serde_json::to_vec(&update.request)?)?;
        assert!(!description.current.activation_credentials_bound);
        assert!(description.current.update_directive.is_none());
        component.run(INITIALIZE).await?;
        harness.state.fail("apply", Fault::Error);
        assert!(component.apply(&update).await.is_err());
        component.apply(&update).await?;
        assert!(component.begin(START, &[]).is_err());
        component
            .call(
                START,
                &wire::encode(&crate::ActivationCredentials::Token {
                    username: "test-user".into(),
                    token: "test-credential".into(),
                })?,
            )
            .await?;
        assert_eq!(
            component.configuration()?,
            update.request.proposed.configuration
        );
        component.apply(&update).await?;
        assert_eq!(harness.state.count("apply"), 2);
        component.run(STOP).await?;
        component.run(DISPOSE).await?;
    }
    Ok(())
}
