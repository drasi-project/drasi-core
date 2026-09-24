// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_lib::computation::v1::{
    ComputationComponent, ComputationService, EnvelopeCodec, EnvelopeSink, EnvelopeSource,
    PluginIdentity, PortDirection, Record, RecordImage, RecordReference, Schema, Transformer,
    WakeupSource,
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};

use crate::{
    abi::{self, BorrowedBytes, Header, Reply, Status},
    metadata::{FactoryMetadata, PluginMetadata, TARGET},
    transaction::RetainedTransaction,
    transport::{self, Failure},
    wire::{self, CreateRequest, InstanceMetadata},
    ControlSender, NativeControlHandler, NativeTransactionContext, TransactionalComponent,
};

/// Configuration-only factory. `create` must not do I/O or start workers. Both
/// standalone and transactional construction use this same immutable contract.
pub trait Factory: Send + Sync + 'static {
    fn metadata(&self) -> FactoryMetadata;
    fn create(
        &self,
        request: &CreateRequest,
        control: ControlSender,
    ) -> anyhow::Result<CreatedComponent>;
}

pub enum Component {
    Source(Box<dyn EnvelopeSource>),
    Transformer(Box<dyn Transformer>),
    Transactional(Box<dyn TransactionalComponent>),
    Sink(Box<dyn EnvelopeSink>),
    Service(Box<dyn ComputationService>),
}
impl Component {
    fn base(&self) -> &dyn ComputationComponent {
        match self {
            Self::Source(value) => value.as_ref(),
            Self::Transformer(value) => value.as_ref(),
            Self::Transactional(value) => value.as_ref(),
            Self::Sink(value) => value.as_ref(),
            Self::Service(value) => value.as_ref(),
        }
    }
    fn base_mut(&mut self) -> &mut dyn ComputationComponent {
        match self {
            Self::Source(value) => value.as_mut(),
            Self::Transformer(value) => value.as_mut(),
            Self::Transactional(value) => value.as_mut(),
            Self::Sink(value) => value.as_mut(),
            Self::Service(value) => value.as_mut(),
        }
    }
    fn transformer(&self) -> Option<&dyn Transformer> {
        match self {
            Self::Transformer(value) => Some(value.as_ref()),
            Self::Transactional(value) => Some(value.as_ref()),
            _ => None,
        }
    }
    fn transformer_mut(&mut self) -> anyhow::Result<&mut dyn Transformer> {
        match self {
            Self::Transformer(value) => Ok(value.as_mut()),
            Self::Transactional(value) => Ok(value.as_mut()),
            _ => anyhow::bail!("component is not a native transformer"),
        }
    }
}

pub struct CreatedComponent {
    pub component: Component,
    pub control_handler: Option<Arc<dyn NativeControlHandler>>,
}
impl From<Component> for CreatedComponent {
    fn from(component: Component) -> Self {
        Self {
            component,
            control_handler: None,
        }
    }
}

pub struct PluginDefinition {
    metadata: PluginMetadata,
    factories: Vec<Arc<dyn Factory>>,
    schemas: Vec<Arc<Schema>>,
}
impl PluginDefinition {
    pub fn new(
        id: impl Into<Arc<str>>,
        version: impl Into<Arc<str>>,
        factories: Vec<Arc<dyn Factory>>,
        schemas: Vec<Arc<Schema>>,
    ) -> anyhow::Result<Self> {
        let plugin = PluginIdentity {
            id: id.into(),
            version: version.into(),
        };
        let metadata = PluginMetadata {
            abi_version: abi::ABI_VERSION.into(),
            wire_version: abi::WIRE_VERSION,
            factories: factories
                .iter()
                .map(|factory| -> anyhow::Result<_> {
                    let mut metadata = factory.metadata();
                    anyhow::ensure!(
                        metadata
                            .implementation
                            .plugin
                            .as_ref()
                            .is_none_or(|value| value == &plugin),
                        "native factory declared conflicting plugin provenance"
                    );
                    metadata.implementation.plugin = Some(plugin.clone());
                    Ok(metadata)
                })
                .collect::<anyhow::Result<_>>()?,
            plugin,
            schemas: schemas
                .iter()
                .map(|schema| schema.descriptor().clone())
                .collect(),
        };
        metadata.validate()?;
        Ok(Self {
            metadata,
            factories,
            schemas,
        })
    }
}

struct PluginState {
    definition: PluginDefinition,
    codec: Arc<EnvelopeCodec>,
}

/// Immutable export backing. Keep it alive while metadata pointers or plugin
/// handles are in use; `export_computation_plugin!` pins it for process lifetime.
pub struct ExportedPlugin {
    metadata: abi::Metadata,
    _manifest: Box<[u8]>,
    state: Arc<PluginState>,
}
// Metadata borrows immutable allocations owned by this object/static TARGET.
unsafe impl Send for ExportedPlugin {}
unsafe impl Sync for ExportedPlugin {}
impl ExportedPlugin {
    pub fn new(definition: PluginDefinition) -> anyhow::Result<Self> {
        definition.metadata.validate()?;
        let manifest = serde_json::to_vec(&definition.metadata)?.into_boxed_slice();
        anyhow::ensure!(
            manifest.len() <= abi::MAX_METADATA_BYTES,
            "native metadata exceeds size limit"
        );
        let codec = Arc::new(wire::codec(&definition.schemas)?);
        Ok(Self {
            metadata: abi::Metadata {
                header: Header::new::<abi::Metadata>(),
                target: BorrowedBytes::new(TARGET.as_bytes()),
                manifest: BorrowedBytes::new(&manifest),
            },
            _manifest: manifest,
            state: Arc::new(PluginState { definition, codec }),
        })
    }
    pub fn metadata(&self) -> *const abi::Metadata {
        &self.metadata
    }
    /// # Safety
    /// out must be a writable, unowned PluginHandle. The caller must release the
    /// returned handle only via its producer-side release callback.
    pub unsafe fn entry(&self, out: *mut abi::PluginHandle) -> Result<(), Failure> {
        require_output(out)?;
        unsafe {
            transport::write_out(
                out,
                abi::PluginHandle {
                    state: Box::into_raw(Box::new(self.state.clone())).cast(),
                    vtable: &PLUGIN_VTABLE,
                },
            )
        }
    }
}

fn require_output<T>(out: *mut T) -> Result<(), Failure> {
    if out.is_null() {
        Err(Failure::protocol("null native output pointer"))
    } else {
        Ok(())
    }
}
unsafe fn state_ref<'a, T>(state: *mut c_void) -> Result<&'a T, Failure> {
    if state.is_null() {
        return Err(Failure::protocol("null native handle"));
    }
    Ok(unsafe { &*state.cast::<T>() })
}
unsafe fn copy_input(input: BorrowedBytes) -> Result<Vec<u8>, Failure> {
    Ok(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? }.to_vec())
}

static PLUGIN_VTABLE: abi::PluginVTable = abi::PluginVTable {
    header: Header::new::<abi::PluginVTable>(),
    release: Some(release_plugin),
    factory: Some(get_factory),
    validate_record: Some(validate_record),
};
unsafe extern "C" fn release_plugin(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<PluginState>>())) });
}
struct FactoryState {
    plugin: Arc<PluginState>,
    index: usize,
}
unsafe extern "C" fn get_factory(
    state: *mut c_void,
    index: u32,
    out: *mut abi::FactoryHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let plugin = unsafe { state_ref::<Arc<PluginState>>(state)? };
        if index as usize >= plugin.definition.factories.len() {
            return Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "unknown native factory index",
            ));
        }
        unsafe {
            transport::write_out(
                out,
                abi::FactoryHandle {
                    state: Box::into_raw(Box::new(FactoryState {
                        plugin: plugin.clone(),
                        index: index as usize,
                    }))
                    .cast(),
                    vtable: &FACTORY_VTABLE,
                },
            )
        }
    })
}
unsafe extern "C" fn validate_record(state: *mut c_void, input: BorrowedBytes) -> Status {
    transport::status_boundary(|| {
        let plugin = unsafe { state_ref::<Arc<PluginState>>(state)? };
        let request: wire::RecordValidation =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? })
                .map_err(Failure::from)?;
        let schema = plugin
            .definition
            .schemas
            .iter()
            .find(|schema| schema.descriptor() == &request.schema)
            .ok_or_else(|| Failure::protocol("undeclared native record schema"))?;
        let identity = request.identity().map_err(Failure::from)?;
        match request.image {
            None => {
                if !request.payload.is_empty() {
                    return Err(Failure::protocol("reference carries record payload"));
                }
                RecordReference::try_new(schema, identity)
                    .map_err(anyhow::Error::from)
                    .map_err(Failure::from)?;
            }
            Some(image) => {
                let image = match image {
                    0 => RecordImage::Full,
                    1 => RecordImage::Patch,
                    2 => RecordImage::Partial,
                    _ => return Err(Failure::protocol("unknown native record image")),
                };
                Record::try_new(schema, identity, image, request.payload.into())
                    .map_err(anyhow::Error::from)
                    .map_err(Failure::from)?;
            }
        }
        Ok(())
    })
}

static FACTORY_VTABLE: abi::FactoryVTable = abi::FactoryVTable {
    header: Header::new::<abi::FactoryVTable>(),
    release: Some(release_factory),
    create: Some(create_component),
};
unsafe extern "C" fn release_factory(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<FactoryState>())) });
}

struct Instance {
    component: tokio::sync::Mutex<Component>,
    metadata: InstanceMetadata,
    factory: FactoryMetadata,
    codec: Arc<EnvelopeCodec>,
    control: ControlSender,
    control_handler: Option<Arc<dyn NativeControlHandler>>,
    wakeup: Option<Arc<dyn WakeupSource>>,
    busy: AtomicBool,
    running: AtomicBool,
}

unsafe extern "C" fn create_component(
    state: *mut c_void,
    input: BorrowedBytes,
    out: *mut abi::ComponentHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let state = unsafe { state_ref::<FactoryState>(state)? };
        let request: CreateRequest =
            wire::decode(unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? })
                .map_err(Failure::from)?;
        let metadata = &state.plugin.definition.metadata.factories[state.index];
        let create = || -> anyhow::Result<Arc<Instance>> {
            anyhow::ensure!(
                request.implementation == metadata.implementation
                    && request.configuration_version == metadata.configuration_version,
                "native implementation/configuration version mismatch"
            );
            metadata.configuration.validate(&request.configuration)?;
            let control = ControlSender::default();
            let created =
                state.plugin.definition.factories[state.index].create(&request, control.clone())?;
            let component = created.component;
            metadata.validate_instance(component.base().descriptor(), &request.id)?;
            let role = match &component {
                Component::Source(source) => {
                    anyhow::ensure!(
                        source.recovery_progress().is_none(),
                        "native source recovery capability is not implemented"
                    );
                    drasi_lib::computation::v1::ComponentRole::Source
                }
                Component::Transformer(_) | Component::Transactional(_) => {
                    drasi_lib::computation::v1::ComponentRole::Transformer
                }
                Component::Sink(sink) => {
                    anyhow::ensure!(
                        metadata.completion == Some(sink.completion())
                            && metadata.capabilities.snapshot == sink.supports_snapshot(),
                        "native sink completion/snapshot capability mismatch"
                    );
                    drasi_lib::computation::v1::ComponentRole::Sink
                }
                Component::Service(_) => drasi_lib::computation::v1::ComponentRole::Service,
            };
            anyhow::ensure!(
                metadata.role == role,
                "native factory returned the wrong component role"
            );
            anyhow::ensure!(
                metadata.capabilities.transactional
                    == matches!(&component, Component::Transactional(_)),
                "native transactional interface does not match metadata"
            );
            anyhow::ensure!(
                component.base().control_handler().is_none(),
                "Rust graph control handlers cannot cross FFI; supply NativeControlHandler"
            );
            anyhow::ensure!(
                created.control_handler.is_some() == metadata.capabilities.control,
                "native control handler does not match metadata"
            );
            anyhow::ensure!(
                component.base().requires_readiness_confirmation()
                    == metadata.capabilities.readiness,
                "native readiness capability mismatch"
            );
            let wakeup = component
                .transformer()
                .and_then(|value| value.wakeup_source());
            anyhow::ensure!(
                wakeup.is_some() == metadata.capabilities.wakeups,
                "native wakeup capability mismatch"
            );
            anyhow::ensure!(
                metadata.capabilities.continuations
                    || !component
                        .transformer()
                        .is_some_and(|value| value.has_pending_emissions()),
                "native component has undeclared continuations"
            );
            if let Component::Transactional(step) = &component {
                let input = metadata
                    .ports
                    .iter()
                    .find(|p| p.direction() == PortDirection::Input)
                    .expect("validated");
                let output = metadata
                    .ports
                    .iter()
                    .find(|p| p.direction() == PortDirection::Output)
                    .expect("validated");
                anyhow::ensure!(
                    step.transaction_input_schema().descriptor() == input.schema()
                        && step.transaction_output_schema().descriptor() == output.schema(),
                    "native transactional schemas differ from factory ports"
                );
            }
            metadata
                .configuration
                .validate(&component.base().configuration()?)?;
            Ok(Arc::new(Instance {
                metadata: InstanceMetadata {
                    descriptor: metadata.descriptor(request.id)?,
                    capabilities: metadata.capabilities,
                },
                component: tokio::sync::Mutex::new(component),
                factory: metadata.clone(),
                codec: state.plugin.codec.clone(),
                control,
                control_handler: created.control_handler,
                wakeup,
                busy: AtomicBool::new(false),
                running: AtomicBool::new(false),
            }))
        };
        let instance = create().map_err(Failure::from)?;
        unsafe {
            transport::write_out(
                out,
                abi::ComponentHandle {
                    state: Box::into_raw(Box::new(instance)).cast(),
                    vtable: &COMPONENT_VTABLE,
                },
            )
        }
    })
}

static COMPONENT_VTABLE: abi::ComponentVTable = abi::ComponentVTable {
    header: Header::new::<abi::ComponentVTable>(),
    release: Some(release_component),
    inspect: Some(inspect_component),
    configuration: Some(configuration),
    bind_control: Some(bind_control),
    begin: Some(begin),
};
unsafe extern "C" fn release_component(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<Instance>>())) });
}
unsafe extern "C" fn inspect_component(state: *mut c_void) -> Reply {
    transport::reply_boundary(|| {
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? };
        serde_json::to_vec(&instance.metadata)
            .map_err(anyhow::Error::from)
            .map_err(Failure::from)
    })
}
unsafe extern "C" fn configuration(state: *mut c_void) -> Reply {
    transport::reply_boundary(|| {
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? };
        let component = instance.component.try_lock().map_err(|_| {
            Failure::new(
                abi::status::BUSY,
                "configuration requires a data-operation boundary",
            )
        })?;
        let configuration = component.base().configuration().map_err(Failure::from)?;
        instance
            .factory
            .configuration
            .validate(&configuration)
            .map_err(Failure::from)?;
        serde_json::to_vec(&configuration)
            .map_err(anyhow::Error::from)
            .map_err(Failure::from)
    })
}
unsafe extern "C" fn bind_control(state: *mut c_void, control: *const abi::Control) -> Status {
    transport::status_boundary(|| {
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? };
        if !instance.factory.capabilities.control {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "native component has no control interface",
            ));
        }
        unsafe { instance.control.bind(control) }
    })
}

fn io_runtime() -> Result<Arc<tokio::runtime::Runtime>, Failure> {
    // Each binary owns its own I/O drivers. Only the host polls component work.
    // The static retains the runtime alongside the process-pinned plugin code.
    static RUNTIME: OnceLock<Result<Arc<tokio::runtime::Runtime>, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("drasi-native-plugin-io")
                .enable_all()
                .build()
                .map(Arc::new)
                .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(Failure::failed)
}
struct BusyGuard(Arc<Instance>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn begin(
    state: *mut c_void,
    code: u32,
    input: BorrowedBytes,
    transaction: *const abi::Transaction,
    out: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let instance = unsafe { state_ref::<Arc<Instance>>(state)? }.clone();
        let input = unsafe { copy_input(input)? };
        if (code == abi::operation::TRANSACT) == transaction.is_null() {
            return Err(Failure::protocol(
                "transaction capability is required only for TRANSACT",
            ));
        }
        let transaction = if transaction.is_null() {
            None
        } else {
            Some(unsafe { RetainedTransaction::new(transaction)? })
        };
        let c = instance.factory.capabilities;
        use abi::operation::*;
        let allowed = match code {
            START | STOP => true,
            NEXT => instance.factory.role == drasi_lib::computation::v1::ComponentRole::Source,
            TRANSFORM | DELIVERY_COMPLETED => {
                instance.factory.role == drasi_lib::computation::v1::ComponentRole::Transformer
            }
            HANDLE => instance.factory.role == drasi_lib::computation::v1::ComponentRole::Sink,
            RUN | QUIESCE => {
                instance.factory.role == drasi_lib::computation::v1::ComponentRole::Service
            }
            CONTINUE => c.continuations,
            WAKE_WAIT | WAKE_PENDING | ON_WAKEUP => c.wakeups,
            SNAPSHOT => c.snapshot,
            CONTROL => c.control,
            TRANSACT => c.transactional,
            _ => false,
        };
        if !allowed {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "unsupported native component operation",
            ));
        }
        let guard = if matches!(code, CONTROL | WAKE_WAIT | WAKE_PENDING) {
            None
        } else {
            instance
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| {
                    Failure::new(
                        abi::status::BUSY,
                        "native data/lifecycle operation is already active",
                    )
                })?;
            Some(BusyGuard(instance.clone()))
        };
        let runtime = io_runtime()?;
        let operation = transport::export_operation(
            async move {
                let _guard = guard;
                execute(instance, code, input, transaction)
                    .await
                    .map_err(Failure::from)
            },
            Some(runtime),
        );
        unsafe { transport::write_out(out, operation) }
    })
}

async fn execute(
    instance: Arc<Instance>,
    code: u32,
    input: Vec<u8>,
    transaction: Option<RetainedTransaction>,
) -> anyhow::Result<Vec<u8>> {
    use abi::operation::*;
    if code == CONTROL {
        instance
            .control_handler
            .as_ref()
            .expect("validated")
            .on_message(wire::decode(&input)?, instance.control.clone())
            .await?;
        return Ok(Vec::new());
    }
    if code == WAKE_WAIT {
        instance.wakeup.as_ref().expect("validated").wait().await?;
        return Ok(Vec::new());
    }
    if code == WAKE_PENDING {
        return wire::encode(
            &instance
                .wakeup
                .as_ref()
                .expect("validated")
                .has_pending()
                .await?,
        );
    }
    let mut component = instance.component.lock().await;
    if !matches!(code, START | STOP | TRANSACT) {
        anyhow::ensure!(
            instance.running.load(Ordering::Acquire),
            "native component has not started"
        );
    }
    match code {
        START => {
            anyhow::ensure!(
                !instance.running.load(Ordering::Acquire),
                "native component already started"
            );
            component.base_mut().start().await?;
            instance.running.store(true, Ordering::Release);
        }
        STOP => {
            instance.running.store(false, Ordering::Release);
            component.base_mut().stop().await?;
        }
        NEXT => {
            let Component::Source(source) = &mut *component else {
                unreachable!("validated role")
            };
            let output = source.next().await?;
            if let Some(output) = &output {
                wire::check_port(
                    &instance.metadata.descriptor,
                    &output.port,
                    &output.envelope,
                    PortDirection::Output,
                )?;
            }
            return wire::encode(
                &output
                    .as_ref()
                    .map(|output| wire::Envelope::output(output, &instance.codec))
                    .transpose()?,
            );
        }
        TRANSFORM | CONTINUE | ON_WAKEUP => {
            let transformer = component.transformer_mut()?;
            let outputs = match code {
                TRANSFORM => {
                    let input =
                        wire::decode::<wire::Envelope>(&input)?.into_input(&instance.codec)?;
                    wire::check_port(
                        &instance.metadata.descriptor,
                        &input.port,
                        &input.envelope,
                        PortDirection::Input,
                    )?;
                    transformer.transform(input).await?
                }
                CONTINUE => transformer.continue_transform().await?,
                _ => transformer.on_wakeup().await?,
            };
            let pending = transformer.has_pending_emissions();
            anyhow::ensure!(
                !pending || instance.factory.capabilities.continuations,
                "native transformer produced undeclared continuation work"
            );
            for output in &outputs {
                wire::check_port(
                    &instance.metadata.descriptor,
                    &output.port,
                    &output.envelope,
                    PortDirection::Output,
                )?;
            }
            return wire::encode(&wire::OutputBatch {
                outputs: outputs
                    .iter()
                    .map(|output| wire::Envelope::output(output, &instance.codec))
                    .collect::<anyhow::Result<_>>()?,
                pending,
            });
        }
        DELIVERY_COMPLETED => {
            let outputs: Vec<wire::Envelope> = wire::decode(&input)?;
            let outputs = outputs
                .into_iter()
                .map(|output| output.into_output(&instance.codec))
                .collect::<anyhow::Result<Vec<_>>>()?;
            for output in &outputs {
                wire::check_port(
                    &instance.metadata.descriptor,
                    &output.port,
                    &output.envelope,
                    PortDirection::Output,
                )?;
            }
            component
                .transformer_mut()?
                .delivery_completed(&outputs)
                .await?;
        }
        HANDLE | SNAPSHOT => {
            let Component::Sink(sink) = &mut *component else {
                unreachable!("validated role")
            };
            let input = wire::decode::<wire::Envelope>(&input)?.into_input(&instance.codec)?;
            wire::check_port(
                &instance.metadata.descriptor,
                &input.port,
                &input.envelope,
                PortDirection::Input,
            )?;
            if code == SNAPSHOT {
                sink.replace_snapshot(input).await?;
            } else {
                sink.handle(input).await?;
            }
        }
        RUN | QUIESCE => {
            let Component::Service(service) = &mut *component else {
                unreachable!("validated role")
            };
            if code == RUN {
                service.run().await?;
            } else {
                service.quiesce().await?;
            }
        }
        TRANSACT => {
            let Component::Transactional(step) = &*component else {
                unreachable!("validated role")
            };
            let input = instance.codec.decode(&input)?;
            anyhow::ensure!(
                input.changes().schema() == step.transaction_input_schema().descriptor(),
                "native transaction input schema mismatch"
            );
            let context = NativeTransactionContext::new(
                instance.metadata.descriptor.id(),
                transaction.as_ref().expect("validated"),
                &instance.codec,
            );
            let output = step.transform_in_transaction(input, &context).await?;
            anyhow::ensure!(
                output.changes().schema() == step.transaction_output_schema().descriptor(),
                "native transaction output schema mismatch"
            );
            return Ok(instance.codec.encode(&output)?.to_vec());
        }
        _ => anyhow::bail!("unsupported native operation"),
    }
    Ok(Vec::new())
}
