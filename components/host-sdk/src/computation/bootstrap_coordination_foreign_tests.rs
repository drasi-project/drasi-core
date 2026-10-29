// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::sync::OnceLock;

#[derive(Default)]
struct Observations {
    descriptions: AtomicUsize,
    snapshots: AtomicUsize,
    subscriptions: AtomicUsize,
}

struct Export {
    native: sdk::ExportedPlugin,
    state: Arc<State>,
    observations: Mutex<Option<Arc<Observations>>>,
}

fn export(shared: bool) -> &'static Export {
    static EXPORTS: OnceLock<[Export; 2]> = OnceLock::new();
    &EXPORTS.get_or_init(|| {
        [false, true].map(|shared| {
            let state = Arc::new(State::default());
            Export {
                native: sdk::ExportedPlugin::new(source_definition(shared, state.clone()).unwrap())
                    .unwrap(),
                state,
                observations: Mutex::new(None),
            }
        })
    })[usize::from(shared)]
}

unsafe extern "C" fn single_metadata() -> *const abi::Metadata {
    export(false).native.metadata()
}
unsafe extern "C" fn shared_metadata() -> *const abi::Metadata {
    export(true).native.metadata()
}
unsafe extern "C" fn single_entry(out: *mut abi::PluginHandle) -> abi::Status {
    transport::status_boundary(|| unsafe { export(false).native.entry(out) })
}
unsafe extern "C" fn shared_entry(out: *mut abi::PluginHandle) -> abi::Status {
    transport::status_boundary(|| unsafe { export(true).native.entry(out) })
}
unsafe extern "C" fn recovery() -> *const abi::recovery::PluginRecoveryV1 {
    export(false).native.recovery()
}
unsafe extern "C" fn consumers() -> *const abi::consumer_progress::PluginConsumerProgressV1 {
    export(false).native.consumer_progress()
}
unsafe extern "C" fn extension() -> *const PluginBootstrapCoordinationV1 {
    static TABLE: OnceLock<PluginBootstrapCoordinationV1> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = unsafe { *export(false).native.bootstrap_coordination() };
        table.create = Some(create);
        table
    })
}

enum RawReader {
    Single(abi::recovery::SourceProgressV1),
    Shared(abi::consumer_progress::ConsumerProgressV1),
}

struct Tap {
    raw: RawReader,
    observations: Arc<Observations>,
}
unsafe impl Send for Tap {}
unsafe impl Sync for Tap {}

impl Tap {
    fn new(raw: RawReader, observations: Arc<Observations>) -> Arc<Self> {
        match &raw {
            RawReader::Single(raw) => unsafe { raw.retain.unwrap()(raw.context) },
            RawReader::Shared(raw) => unsafe { raw.retain.unwrap()(raw.context) },
        }
        Arc::new(Self { raw, observations })
    }
    fn single(self: &Arc<Self>) -> abi::recovery::SourceProgressV1 {
        abi::recovery::SourceProgressV1 {
            header: abi::Header::new::<abi::recovery::SourceProgressV1>(),
            context: Arc::as_ptr(self).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            snapshot: Some(snapshot),
            subscribe: Some(subscribe_single),
        }
    }
    fn shared(self: &Arc<Self>) -> abi::consumer_progress::ConsumerProgressV1 {
        abi::consumer_progress::ConsumerProgressV1 {
            header: abi::Header::new::<abi::consumer_progress::ConsumerProgressV1>(),
            context: Arc::as_ptr(self).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            snapshot: Some(snapshot),
            subscribe: Some(subscribe_shared),
        }
    }
}
impl Drop for Tap {
    fn drop(&mut self) {
        match &self.raw {
            RawReader::Single(raw) => unsafe { raw.release.unwrap()(raw.context) },
            RawReader::Shared(raw) => unsafe { raw.release.unwrap()(raw.context) },
        }
    }
}
unsafe extern "C" fn retain(context: *mut c_void) {
    unsafe { Arc::increment_strong_count(context.cast::<Tap>()) };
}
unsafe extern "C" fn release(context: *mut c_void) {
    unsafe { Arc::decrement_strong_count(context.cast::<Tap>()) };
}
unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    let tap = unsafe { &*context.cast::<Tap>() };
    tap.observations.descriptions.fetch_add(1, Ordering::SeqCst);
    match &tap.raw {
        RawReader::Single(raw) => unsafe { raw.describe.unwrap()(raw.context) },
        RawReader::Shared(raw) => unsafe { raw.describe.unwrap()(raw.context) },
    }
}
unsafe extern "C" fn snapshot(context: *mut c_void) -> abi::Reply {
    let tap = unsafe { &*context.cast::<Tap>() };
    tap.observations.snapshots.fetch_add(1, Ordering::SeqCst);
    match &tap.raw {
        RawReader::Single(raw) => unsafe { raw.snapshot.unwrap()(raw.context) },
        RawReader::Shared(raw) => unsafe { raw.snapshot.unwrap()(raw.context) },
    }
}
unsafe extern "C" fn subscribe_single(
    context: *mut c_void,
    out: *mut abi::recovery::ProgressSubscriptionV1,
) -> abi::Status {
    let tap = unsafe { &*context.cast::<Tap>() };
    tap.observations
        .subscriptions
        .fetch_add(1, Ordering::SeqCst);
    let RawReader::Single(raw) = &tap.raw else {
        unreachable!()
    };
    unsafe { raw.subscribe.unwrap()(raw.context, out) }
}
unsafe extern "C" fn subscribe_shared(
    context: *mut c_void,
    out: *mut abi::consumer_progress::ConsumerProgressSubscriptionV1,
) -> abi::Status {
    let tap = unsafe { &*context.cast::<Tap>() };
    tap.observations
        .subscriptions
        .fetch_add(1, Ordering::SeqCst);
    let RawReader::Shared(raw) = &tap.raw else {
        unreachable!()
    };
    unsafe { raw.subscribe.unwrap()(raw.context, out) }
}

static ORIGINAL: OnceLock<BootstrapCoordinatorVTable> = OnceLock::new();
static BAD_STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn malformed_stop(
    context: *mut c_void,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    if BAD_STOP.swap(false, Ordering::SeqCst) {
        transport::status_boundary(|| unsafe {
            transport::write_out(
                out,
                transport::export_operation(async { Ok(vec![1]) }, None),
            )
        })
    } else {
        unsafe { ORIGINAL.get().unwrap().stop.unwrap()(context, out) }
    }
}

unsafe extern "C" fn changed_description(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let bytes = unsafe { take_reply(ORIGINAL.get().unwrap().describe.unwrap()(context))? };
        let mut request: CreateRequest = bootstrap::decode(&bytes).map_err(Failure::from)?;
        match request.configuration["describeMismatch"].as_str().unwrap() {
            "source" => request.id = ComponentId::try_new("foreign").unwrap(),
            "instance" => request.scope.as_mut().unwrap().instance_id = "foreign".into(),
            "graph" => request.scope.as_mut().unwrap().graph_id = "foreign".into(),
            "factory" => {
                request.implementation = ImplementationIdentity::try_new("foreign", "1").unwrap()
            }
            "version" => request.configuration_version += 1,
            "configuration" => request.configuration["changed"] = json!(true),
            _ => unreachable!(),
        }
        bootstrap::encode(&request).map_err(Failure::from)
    })
}

unsafe extern "C" fn create(
    factory: *mut c_void,
    input: abi::BorrowedBytes,
    progress: *const BootstrapProgressV1,
    out: *mut BootstrapCoordinatorHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        let input_value: CreateRequest = bootstrap::decode(unsafe {
            transport::borrowed_bytes(input, abi::bootstrap::MAX_CONTROL_BYTES)?
        })
        .map_err(Failure::from)?;
        let raw = *unsafe { checked_table(progress)? };
        let shared = raw.single.is_null();
        let observations = Arc::new(Observations::default());
        *export(shared).observations.lock().unwrap() = Some(observations.clone());
        let single_tap = if shared {
            None
        } else {
            Some(Tap::new(
                RawReader::Single(unsafe { *raw.single }),
                observations.clone(),
            ))
        };
        let consumer_tap = if shared {
            Some(Tap::new(
                RawReader::Shared(unsafe { *raw.consumers }),
                observations.clone(),
            ))
        } else {
            None
        };
        let members = if shared {
            unsafe { std::slice::from_raw_parts(raw.members, raw.member_count as usize) }
        } else {
            &[]
        };
        let taps: Vec<_> = members
            .iter()
            .map(|member| Tap::new(RawReader::Single(member.progress), observations.clone()))
            .collect();
        let members: Vec<_> = members
            .iter()
            .zip(&taps)
            .map(|(member, tap)| BootstrapProgressMemberV1 {
                id: member.id,
                progress: tap.single(),
            })
            .collect();
        let single = single_tap.as_ref().map(Tap::single);
        let consumers = consumer_tap.as_ref().map(Tap::shared);
        let readers = BootstrapProgressV1 {
            single: single.as_ref().map_or(std::ptr::null(), |table| table),
            consumers: consumers.as_ref().map_or(std::ptr::null(), |table| table),
            members: if members.is_empty() {
                std::ptr::null()
            } else {
                members.as_ptr()
            },
            ..raw
        };
        unsafe {
            take_status((*export(shared).native.bootstrap_coordination())
                .create
                .unwrap()(factory, input, &readers, out))?;
            ORIGINAL.get_or_init(|| *(*out).vtable);
        }
        if input_value.configuration.get("describeMismatch").is_some() {
            static CHANGED: OnceLock<BootstrapCoordinatorVTable> = OnceLock::new();
            unsafe {
                (*out).vtable = CHANGED.get_or_init(|| BootstrapCoordinatorVTable {
                    describe: Some(changed_description),
                    ..*ORIGINAL.get().unwrap()
                });
            }
        }
        if input_value.configuration["badStop"] == true {
            static CHANGED: OnceLock<BootstrapCoordinatorVTable> = OnceLock::new();
            BAD_STOP.store(true, Ordering::SeqCst);
            unsafe {
                (*out).vtable = CHANGED.get_or_init(|| BootstrapCoordinatorVTable {
                    stop: Some(malformed_stop),
                    ..*ORIGINAL.get().unwrap()
                });
            }
        }
        Ok(())
    })
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_constructor_only_describes_and_validates_immutable_metadata(
) -> anyhow::Result<()> {
    for shared in [false, true] {
        let native = unsafe {
            NativePlugin::from_entry_points_with_bootstrap_coordination(
                if shared {
                    shared_metadata
                } else {
                    single_metadata
                },
                if shared { shared_entry } else { single_entry },
                Some(recovery),
                Some(consumers),
                None,
                Some(extension),
            )?
        };
        let factory = &native.factories()[0];
        let query = owner("query");
        let group = Arc::new(ConsumerProgressSet::new(
            "graph",
            ComponentId::try_new("source")?,
            [query.clone(), owner("other")],
        )?);
        let make = |configuration| {
            factory.create_source_bootstrap(
                ComponentId::try_new("source")?,
                configuration,
                scope(0),
                progress(shared, &query, &group),
            )
        };
        let coordinator = make(json!({}))?;
        let counts = export(shared).observations.lock().unwrap().clone().unwrap();
        assert!(counts.descriptions.load(Ordering::SeqCst) > 0);
        assert_eq!(
            counts.snapshots.load(Ordering::SeqCst),
            0,
            "coordinator construction cannot snapshot real readers"
        );
        assert_eq!(
            counts.subscriptions.load(Ordering::SeqCst),
            0,
            "coordinator construction cannot subscribe to real readers"
        );
        readers_active(&export(shared).state, true);
        let _subscriptions = Subscriptions::new(&export(shared).state)?;
        let expected = if shared { 3 } else { 1 };
        assert_eq!(counts.snapshots.load(Ordering::SeqCst), expected);
        assert_eq!(counts.subscriptions.load(Ordering::SeqCst), expected);
        coordinator.shutdown().await?;
        let malformed = make(json!({"badStop":true}))?;
        let before = export(shared).state.stop_started.load(Ordering::SeqCst);
        assert!(malformed
            .shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("unexpected payload"));
        assert_eq!(
            export(shared).state.stop_started.load(Ordering::SeqCst),
            before
        );
        assert!(malformed
            .bind_query(query.component_id(), &scope(0), &query)
            .is_err());
        readers_active(&export(shared).state, true);
        let mut pending = Subscriptions::new(&export(shared).state)?;
        pending.verify(true).await?;
        malformed.shutdown().await?;
        assert_eq!(
            export(shared).state.stop_started.load(Ordering::SeqCst),
            before + 1
        );
        pending.verify(false).await?;
        for mismatch in [
            "source",
            "instance",
            "graph",
            "factory",
            "version",
            "configuration",
        ] {
            assert!(
                make(json!({"describeMismatch":mismatch})).is_err(),
                "foreign description mismatch {mismatch} must be rejected"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_validates_every_returned_handle_field() -> anyhow::Result<()> {
    let state = Arc::new(State::default());
    let plugin = NativePlugin::from_definition(source_definition(false, state)?)?;
    let resource = plugin.factories()[0].create_source_bootstrap(
        ComponentId::try_new("source")?,
        json!({}),
        scope(0),
        NativeSourceBootstrapProgress::Single(owner("query")),
    )?;
    let raw = resource.raw;
    assert!(unsafe { coordinator_table(BootstrapCoordinatorHandle::null()) }.is_err());
    assert!(unsafe {
        coordinator_table(BootstrapCoordinatorHandle {
            vtable: raw.vtable.cast::<u8>().wrapping_add(1).cast(),
            ..raw
        })
    }
    .is_err());
    let prefix = abi::Header::new::<abi::Header>();
    assert!(unsafe {
        coordinator_table(BootstrapCoordinatorHandle {
            vtable: (&prefix as *const abi::Header).cast(),
            ..raw
        })
    }
    .is_err());
    assert!(unsafe {
        coordinator_table(BootstrapCoordinatorHandle {
            state: std::ptr::null_mut(),
            ..raw
        })
    }
    .is_err());
    for field in 0..9 {
        let mut table = unsafe { *raw.vtable };
        match field {
            0 => table.header.magic ^= 1,
            1 => table.header.major += 1,
            2 => table.header.minor += 1,
            3 => table.header.size += 1,
            4 => table.retain = None,
            5 => table.release = None,
            6 => table.describe = None,
            7 => table.open = None,
            _ => table.stop = None,
        }
        assert!(unsafe {
            coordinator_table(BootstrapCoordinatorHandle {
                vtable: &table,
                ..raw
            })
        }
        .is_err());
    }
    resource.shutdown().await?;
    Ok(())
}

struct OrdinaryFactory(bool);
impl sdk::Factory for OrdinaryFactory {
    fn metadata(&self) -> sdk::FactoryMetadata {
        metadata()
    }
    fn supports_bootstrap_coordination(&self) -> bool {
        self.0
    }
    fn create(
        &self,
        request: &CreateRequest,
        _: sdk::ControlSender,
    ) -> anyhow::Result<sdk::CreatedComponent> {
        Ok(
            sdk::Component::Source(Box::new(OrdinarySource(ComponentDescriptor::try_new(
                request.id.clone(),
                metadata().ports,
            )?)))
            .into(),
        )
    }
}

struct OrdinarySource(ComponentDescriptor);
#[async_trait]
impl sdk::NativeComponent for OrdinarySource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(json!({}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl sdk::NativeSource for OrdinarySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}
fn ordinary_definition(supported: bool) -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "fixture/ordinary-source",
        "1",
        vec![Arc::new(OrdinaryFactory(supported))],
        vec![GraphChangeCodec::schema()],
    )
}
fn ordinary_export() -> &'static sdk::ExportedPlugin {
    static EXPORT: OnceLock<sdk::ExportedPlugin> = OnceLock::new();
    EXPORT.get_or_init(|| sdk::ExportedPlugin::new(ordinary_definition(false).unwrap()).unwrap())
}
unsafe extern "C" fn ordinary_metadata() -> *const abi::Metadata {
    ordinary_export().metadata()
}
unsafe extern "C" fn ordinary_entry(out: *mut abi::PluginHandle) -> abi::Status {
    transport::status_boundary(|| unsafe { ordinary_export().entry(out) })
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_absent_and_opted_out_factories_preserve_ordinary_sources(
) -> anyhow::Result<()> {
    for present in [false, true] {
        let plugin = if present {
            NativePlugin::from_definition(ordinary_definition(false)?)?
        } else {
            unsafe { NativePlugin::from_entry_points(ordinary_metadata, ordinary_entry)? }
        };
        let mut registry = crate::PluginRegistry::new();
        registry.register_computation_plugin(plugin.clone())?;
        assert!(registry
            .computation_source_bootstrap_factories()?
            .is_empty());
        let factory = &plugin.factories()[0];
        assert!(!factory.supports_source_bootstrap());
        assert!(!factory
            .descriptor()
            .dependencies
            .contains_key("source_bootstrap"));
        assert!(factory
            .create_source_bootstrap(
                ComponentId::try_new("source")?,
                json!({}),
                scope(0),
                NativeSourceBootstrapProgress::Single(owner("query")),
            )
            .is_err());
        let mut spec = factory.specification(ComponentId::try_new("source")?, json!({}))?;
        spec.dependencies.insert(
            "source_bootstrap".into(),
            vec![ResourceId::try_new("coordinator")?],
        );
        assert!(factory.validate(&spec).is_err());
        let mut live = factory.create_component(ComponentId::try_new("source")?, json!({}))?;
        live.start().await?;
        assert!(live.next().await?.is_none());
        live.stop().await?;
    }
    // Merely advertising the extension cannot grant coordination without its
    // required source progress service.
    assert!(ordinary_definition(true)
        .and_then(NativePlugin::from_definition)
        .is_err());
    Ok(())
}
