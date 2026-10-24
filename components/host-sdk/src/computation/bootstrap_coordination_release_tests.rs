// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::sync::OnceLock;

#[derive(Default)]
struct Counts {
    releases: AtomicUsize,
    drops: AtomicUsize,
}

struct Fixture {
    native: sdk::ExportedPlugin,
    counts: Mutex<Option<Arc<Counts>>>,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| Fixture {
        native: sdk::ExportedPlugin::new(
            source_definition(false, Arc::new(State::default())).unwrap(),
        )
        .unwrap(),
        counts: Mutex::new(None),
    })
}

struct Counted {
    raw: BootstrapCoordinatorHandle,
    counts: Arc<Counts>,
}
unsafe impl Send for Counted {}
unsafe impl Sync for Counted {}
impl Drop for Counted {
    fn drop(&mut self) {
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
        unsafe { (*self.raw.vtable).release.unwrap()(self.raw.state) };
    }
}

unsafe extern "C" fn retain(context: *mut c_void) {
    unsafe { Arc::increment_strong_count(context.cast::<Counted>()) };
}
unsafe extern "C" fn release(context: *mut c_void) {
    let state = unsafe { &*context.cast::<Counted>() };
    state.counts.releases.fetch_add(1, Ordering::SeqCst);
    unsafe { Arc::decrement_strong_count(context.cast::<Counted>()) };
}
unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    let raw = unsafe { &*context.cast::<Counted>() }.raw;
    unsafe { (*raw.vtable).describe.unwrap()(raw.state) }
}
unsafe extern "C" fn invalid_description(_: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| bootstrap::encode(&"not a create request").map_err(Failure::from))
}
unsafe extern "C" fn open(
    context: *mut c_void,
    query: abi::BorrowedBytes,
    out: *mut abi::bootstrap::BootstrapHandle,
) -> abi::Status {
    let raw = unsafe { &*context.cast::<Counted>() }.raw;
    unsafe { (*raw.vtable).open.unwrap()(raw.state, query, out) }
}
unsafe extern "C" fn stop(context: *mut c_void, out: *mut abi::OperationHandle) -> abi::Status {
    let raw = unsafe { &*context.cast::<Counted>() }.raw;
    unsafe { (*raw.vtable).stop.unwrap()(raw.state, out) }
}

fn tables() -> &'static [BootstrapCoordinatorVTable; 6] {
    static TABLES: OnceLock<[BootstrapCoordinatorVTable; 6]> = OnceLock::new();
    TABLES.get_or_init(|| {
        let complete = BootstrapCoordinatorVTable {
            header: abi::Header::new::<BootstrapCoordinatorVTable>(),
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            open: Some(open),
            stop: Some(stop),
        };
        [
            complete,
            BootstrapCoordinatorVTable {
                retain: None,
                ..complete
            },
            BootstrapCoordinatorVTable {
                describe: None,
                ..complete
            },
            BootstrapCoordinatorVTable {
                open: None,
                ..complete
            },
            BootstrapCoordinatorVTable {
                stop: None,
                ..complete
            },
            BootstrapCoordinatorVTable {
                describe: Some(invalid_description),
                ..complete
            },
        ]
    })
}

unsafe extern "C" fn create(
    factory: *mut c_void,
    input: abi::BorrowedBytes,
    progress: *const BootstrapProgressV1,
    out: *mut BootstrapCoordinatorHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        let request: CreateRequest = bootstrap::decode(unsafe {
            transport::borrowed_bytes(input, abi::bootstrap::MAX_CONTROL_BYTES)?
        })
        .map_err(Failure::from)?;
        let index = request.configuration["table"].as_u64().unwrap() as usize;
        let mut raw = BootstrapCoordinatorHandle::null();
        unsafe {
            take_status(
                (*fixture().native.bootstrap_coordination()).create.unwrap()(
                    factory, input, progress, &mut raw,
                ),
            )?;
        }
        let counts = Arc::new(Counts::default());
        *fixture().counts.lock().unwrap() = Some(counts.clone());
        let state = Arc::new(Counted { raw, counts });
        unsafe {
            transport::write_out(
                out,
                BootstrapCoordinatorHandle {
                    state: Arc::into_raw(state).cast_mut().cast(),
                    vtable: &tables()[index],
                },
            )
        }
    })
}

unsafe extern "C" fn metadata() -> *const abi::Metadata {
    fixture().native.metadata()
}
unsafe extern "C" fn entry(out: *mut abi::PluginHandle) -> abi::Status {
    transport::status_boundary(|| unsafe { fixture().native.entry(out) })
}
unsafe extern "C" fn recovery() -> *const abi::recovery::PluginRecoveryV1 {
    fixture().native.recovery()
}
unsafe extern "C" fn extension() -> *const PluginBootstrapCoordinationV1 {
    static TABLE: OnceLock<PluginBootstrapCoordinationV1> = OnceLock::new();
    TABLE.get_or_init(|| PluginBootstrapCoordinationV1 {
        create: Some(create),
        ..unsafe { *fixture().native.bootstrap_coordination() }
    })
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordination_releases_transferred_owner_exactly_once_on_rejection(
) -> anyhow::Result<()> {
    let plugin = unsafe {
        NativePlugin::from_entry_points_with_bootstrap_coordination(
            metadata,
            entry,
            Some(recovery),
            None,
            None,
            Some(extension),
        )?
    };
    let make = |table| {
        plugin.factories()[0].create_source_bootstrap(
            ComponentId::try_new("source")?,
            json!({"table":table}),
            scope(0),
            NativeSourceBootstrapProgress::Single(owner("query")),
        )
    };
    // Every rejected table has a valid header, non-null state, and valid release.
    // The last case fails after ownership has transferred into the resource.
    for table in 1..6 {
        assert!(make(table).is_err(), "table {table} should be rejected");
        let counts = fixture().counts.lock().unwrap().take().unwrap();
        assert_eq!(
            counts.releases.load(Ordering::SeqCst),
            1,
            "table {table} leaked or double-released"
        );
        assert_eq!(
            counts.drops.load(Ordering::SeqCst),
            1,
            "producer owner must be reclaimed"
        );
    }
    let resource = make(0)?;
    let counts = fixture().counts.lock().unwrap().take().unwrap();
    assert_eq!(
        counts.releases.load(Ordering::SeqCst),
        0,
        "successful transfer disarms the guard"
    );
    let retained = resource.clone();
    drop(resource);
    retained.shutdown().await?;
    assert_eq!(counts.releases.load(Ordering::SeqCst), 0);
    drop(retained);
    assert_eq!(counts.releases.load(Ordering::SeqCst), 1);
    assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    Ok(())
}
