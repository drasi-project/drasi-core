// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use handled_branch_ownership::{
    protocol::{self, CloneInput, Create, Destroy, Emit, Reply, Token},
    BranchKey, Error, Limits, Registry,
};
use libloading::Library;
use std::{
    ffi::c_void,
    mem::MaybeUninit,
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

struct Native {
    create: Create,
    clone_input: CloneInput,
    emit: Emit,
    destroy: Destroy,
    _library: Library,
}

impl Native {
    fn load() -> Self {
        let executable = std::env::current_exe().unwrap();
        let profile = executable
            .parent()
            .and_then(|deps| deps.parent())
            .expect("test executable in profile/deps");
        let path = profile.join(format!(
            "{}handled_branch_ownership_fixture{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        assert!(
            path.is_file(),
            "Build the independent fixture first: cargo build --manifest-path lib/tests/handled-branch-ownership/Cargo.toml -p handled-branch-ownership-fixture"
        );
        unsafe {
            let library = Library::new(path).unwrap();
            Self {
                create: *library.get(b"timing_create\0").unwrap(),
                clone_input: *library.get(b"timing_clone\0").unwrap(),
                emit: *library.get(b"timing_emit\0").unwrap(),
                destroy: *library.get(b"timing_destroy\0").unwrap(),
                _library: library,
            }
        }
    }

    fn retain<'a>(
        &'a self,
        registry: &Arc<Registry>,
        token: Token,
        value: u64,
    ) -> Result<Input<'a>, u32> {
        self.retain_host(registry.host(), token, value)
    }

    fn retain_host(
        &self,
        host: protocol::Host,
        token: Token,
        value: u64,
    ) -> Result<Input<'_>, u32> {
        let mut input = ptr::null_mut();
        let status = unsafe { (self.create)(host, token, value, &mut input) };
        if status != protocol::OK {
            assert!(input.is_null());
            return Err(status);
        }
        Ok(Input {
            native: self,
            input,
        })
    }
}

struct Input<'a> {
    native: &'a Native,
    input: *mut c_void,
}

impl<'a> Input<'a> {
    fn try_clone(&self) -> Result<Self, u32> {
        let mut input = ptr::null_mut();
        let status = unsafe { (self.native.clone_input)(self.input, &mut input) };
        if status != protocol::OK {
            assert!(input.is_null());
            return Err(status);
        }
        Ok(Self {
            native: self.native,
            input,
        })
    }

    fn emit(&self) -> Result<Output<'a>, u32> {
        let mut reply = MaybeUninit::uninit();
        let status = unsafe { (self.native.emit)(self.input, reply.as_mut_ptr()) };
        if status != protocol::OK {
            return Err(status);
        }
        Ok(Output {
            reply: unsafe { reply.assume_init() },
            _native: self.native,
        })
    }
}

impl Drop for Input<'_> {
    fn drop(&mut self) {
        unsafe { (self.native.destroy)(self.input) };
    }
}

struct Output<'a> {
    reply: Reply,
    _native: &'a Native,
}

impl Output<'_> {
    fn value(&self) -> u64 {
        assert_eq!(self.reply.len, 8);
        let bytes = unsafe { std::slice::from_raw_parts(self.reply.data, self.reply.len) };
        u64::from_le_bytes(bytes.try_into().unwrap())
    }
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        unsafe { (self.reply.release)(self.reply.owner) };
    }
}

type Rig = (Arc<Registry>, Arc<AtomicU64>);

fn rig(records: usize) -> Rig {
    let clock = Arc::new(AtomicU64::new(0));
    let registry = Registry::controlled(
        Limits {
            records,
            bytes: 64 * 1024,
        },
        clock.clone(),
    )
    .unwrap();
    (registry, clock)
}

#[test]
fn retained_native_input_and_reply_preserve_original_host_start() {
    let native = Native::load();
    let (registry, clock) = rig(64);
    let source = registry.start().unwrap();
    let input = native.retain(&registry, source.token(), 41).unwrap();
    drop(source);
    clock.store(100_000_000, Ordering::Release);
    let clone = input.try_clone().unwrap();
    drop(input);
    let output = clone.emit().unwrap();
    drop(clone);
    assert_eq!(output.value(), 42);
    // Only the plugin-owned reply keeps the origin alive at this point.
    assert_eq!(registry.diagnostics().unwrap().records, 2);
    let received = registry.acquire(output.reply.token).unwrap();
    drop(output);
    let branch = BranchKey { output: 1, sink: 1 };
    registry.prepare(received.token(), branch, 1).unwrap();
    clock.store(250_000_000, Ordering::Release);
    assert_eq!(
        registry.handled(received.token(), branch, &[0]).unwrap(),
        Some(Duration::from_millis(250))
    );
    assert_eq!(
        registry.handled(received.token(), branch, &[0]).unwrap(),
        None
    );
    drop(received);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn failed_decode_or_cancelled_native_work_releases_owned_buffers() {
    let native = Native::load();
    let (registry, _) = rig(64);
    for emit in [false, true] {
        let source = registry.start().unwrap();
        let input = native.retain(&registry, source.token(), 1).unwrap();
        drop(source);
        if emit {
            let rejected_reply = input.emit().unwrap();
            drop(input);
            drop(rejected_reply);
        } else {
            drop(input);
        }
        assert_eq!(registry.diagnostics().unwrap().records, 0);
        assert_eq!(registry.diagnostics().unwrap().bytes, 0);
    }
}

#[test]
fn native_capacity_failure_does_not_acquire_an_unaccounted_lease() {
    let native = Native::load();
    let (registry, _) = rig(4);
    let source = registry.start().unwrap();
    let input = native.retain(&registry, source.token(), 10).unwrap();
    let clone = input.try_clone().unwrap();
    assert_eq!(registry.diagnostics().unwrap().records, 4);
    let untracked_output = clone.emit().unwrap();
    assert_eq!(untracked_output.value(), 11);
    assert_eq!(untracked_output.reply.token, Token::default());
    let untracked_clone = input.try_clone().unwrap();
    let clone_output = untracked_clone.emit().unwrap();
    assert_eq!(clone_output.value(), 11);
    assert_eq!(clone_output.reply.token, Token::default());
    assert_eq!(registry.diagnostics().unwrap().records, 4);
    assert_eq!(registry.diagnostics().unwrap().capacity, 2);
    drop((source, clone, input));
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    drop((untracked_output, untracked_clone, clone_output));
}

#[test]
fn host_context_outlives_graph_owner_until_last_native_release() {
    let native = Native::load();
    let (registry, _) = rig(64);
    let weak = Arc::downgrade(&registry);
    let source = registry.start().unwrap();
    let input = native.retain(&registry, source.token(), 0).unwrap();
    drop(source);
    registry.retire().unwrap();
    drop(registry);
    assert!(weak.upgrade().is_some());
    let output = input.emit().unwrap();
    assert_eq!(output.value(), 1);
    assert_eq!(output.reply.token, Token::default());
    drop(input);
    assert!(weak.upgrade().is_none());
    drop(output);
}

#[test]
fn stale_and_foreign_tokens_fail_across_the_actual_library_boundary() {
    let native = Native::load();
    let (one, _) = rig(64);
    let (two, _) = rig(64);
    let source = one.start().unwrap();
    let stale = source.token();
    assert!(matches!(
        native.retain(&two, stale, 1),
        Err(protocol::INVALID)
    ));
    drop(source);
    assert!(matches!(
        native.retain(&one, stale, 1),
        Err(protocol::INVALID)
    ));
    assert!(matches!(one.acquire(stale), Err(Error::Invalid)));
    assert_eq!(one.diagnostics().unwrap().records, 0);
    assert_eq!(two.diagnostics().unwrap().records, 0);
}

#[test]
fn rejected_native_output_keeps_only_the_existing_input_owner() {
    let native = Native::load();
    let (registry, _) = rig(64);
    let source = registry.start().unwrap();
    let input = native.retain(&registry, source.token(), u64::MAX).unwrap();
    drop(source);
    assert!(matches!(input.emit(), Err(protocol::INVALID)));
    assert_eq!(registry.diagnostics().unwrap().records, 2);
    drop(input);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
}

#[test]
fn disabled_timing_transforms_without_registry_activity_or_callback_retention() {
    let native = Native::load();
    let (registry, _) = rig(64);
    let weak = Arc::downgrade(&registry);
    let input = native.retain(&registry, Token::default(), 10).unwrap();
    assert_eq!(registry.diagnostics().unwrap(), Default::default());
    drop(registry);
    assert!(weak.upgrade().is_none());
    let clone = input.try_clone().unwrap();
    drop(input);
    let output = clone.emit().unwrap();
    drop(clone);
    assert_eq!(output.value(), 11);
    assert_eq!(output.reply.token, Token::default());
}

#[test]
fn host_receive_capacity_failure_releases_the_foreign_reply() {
    let native = Native::load();
    let (registry, _) = rig(3);
    let source = registry.start().unwrap();
    let input = native.retain(&registry, source.token(), 1).unwrap();
    drop(source);
    let output = input.emit().unwrap();
    assert_eq!(output.value(), 2);
    assert!(matches!(
        registry.acquire(output.reply.token),
        Err(Error::Capacity)
    ));
    drop((output, input));
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

#[test]
fn concurrent_native_transfers_and_completion_share_one_receipt() {
    let (registry, clock) = rig(128);
    let source = registry.start().unwrap();
    clock.store(100_000_000, Ordering::Release);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let registry = registry.clone();
            let source = source.clone();
            std::thread::spawn(move || {
                let native = Native::load();
                let input = native.retain(&registry, source.token(), 1).unwrap();
                let output = input.emit().unwrap();
                let received = registry.acquire(output.reply.token).unwrap();
                drop((input, output));
                let branch = BranchKey { output: 1, sink: 1 };
                registry.prepare(received.token(), branch, 1).unwrap();
                registry.handled(received.token(), branch, &[0]).unwrap()
            })
        })
        .collect();
    let samples: Vec<_> = threads
        .into_iter()
        .filter_map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(samples, [Duration::from_millis(100)]);
    drop(source);
    assert_eq!(registry.diagnostics().unwrap().records, 0);
    assert_eq!(registry.diagnostics().unwrap().bytes, 0);
}

mod production {
    use super::*;
    use drasi_computation_contracts::telemetry::{
        HandledBranchKey, HandledBranchLimits, HandledBranchToken, HandledBranchTracker,
        TelemetryError,
    };

    fn token(value: HandledBranchToken) -> Token {
        Token {
            owner: value.owner,
            lease: value.lease,
        }
    }

    fn local(value: Token) -> HandledBranchToken {
        HandledBranchToken {
            owner: value.owner,
            lease: value.lease,
        }
    }

    fn status(error: TelemetryError) -> u32 {
        match error {
            TelemetryError::Capacity(_) => protocol::FULL,
            TelemetryError::Retired => protocol::CLOSED,
            _ => protocol::INVALID,
        }
    }

    unsafe extern "C" fn retain_context(context: *const c_void) {
        unsafe { Arc::increment_strong_count(context.cast::<HandledBranchTracker>()) };
    }

    unsafe extern "C" fn release_context(context: *const c_void) {
        unsafe { Arc::decrement_strong_count(context.cast::<HandledBranchTracker>()) };
    }

    unsafe extern "C" fn retain(context: *const c_void, input: Token, output: *mut Token) -> u32 {
        if context.is_null() || output.is_null() {
            return protocol::INVALID;
        }
        let tracker = unsafe { &*context.cast::<HandledBranchTracker>() };
        match tracker.retain_token(local(input)) {
            Ok(value) => {
                unsafe { output.write(token(value)) };
                protocol::OK
            }
            Err(error) => status(error),
        }
    }

    unsafe extern "C" fn release(context: *const c_void, input: Token) -> u32 {
        if context.is_null() {
            return protocol::INVALID;
        }
        let tracker = unsafe { &*context.cast::<HandledBranchTracker>() };
        tracker
            .release_token(local(input))
            .map_or_else(status, |()| protocol::OK)
    }

    fn host(tracker: &Arc<HandledBranchTracker>) -> protocol::Host {
        protocol::Host {
            context: Arc::as_ptr(tracker).cast(),
            retain_context,
            release_context,
            retain,
            release,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn actual_shared_tracker_keeps_timing_through_native_input_and_reply() {
        let native = Native::load();
        let tracker = HandledBranchTracker::new(Default::default()).unwrap();
        let origin = tracker.start().unwrap();
        let input = native
            .retain_host(host(&tracker), token(origin.token().unwrap()), 41)
            .unwrap();
        drop(origin);
        tokio::time::advance(Duration::from_millis(100)).await;
        let output = input.emit().unwrap();
        drop(input);
        assert_eq!(output.value(), 42);
        let received = tracker.acquire(local(output.reply.token)).unwrap();
        drop(output);
        let branch = HandledBranchKey {
            producer_epoch: [1; 16],
            output_sequence: 7,
            sink: "sink".into(),
        };
        let token = received.token().unwrap();
        tracker.prepare(token, &branch, 2).unwrap();
        assert_eq!(tracker.handled_rows(token, &branch, &[0]).unwrap(), None);
        tokio::time::advance(Duration::from_millis(150)).await;
        assert_eq!(
            tracker.handled_rows(token, &branch, &[1]).unwrap(),
            Some(Duration::from_millis(250))
        );
        assert_eq!(tracker.handled_all(token, &branch).unwrap(), None);
        drop(received);
        assert_eq!(tracker.diagnostics().unwrap().records, 0);
        assert_eq!(tracker.diagnostics().unwrap().record_bytes, 0);
    }

    #[test]
    fn actual_shared_tracker_saturation_and_retirement_preserve_business_results() {
        let native = Native::load();
        let tracker = HandledBranchTracker::new(HandledBranchLimits {
            max_records: 3,
            ..Default::default()
        })
        .unwrap();
        let weak = Arc::downgrade(&tracker);
        let origin = tracker.start().unwrap();
        let input = native
            .retain_host(host(&tracker), token(origin.token().unwrap()), 10)
            .unwrap();
        let output = input.emit().unwrap();
        assert_eq!(output.value(), 11);
        assert_eq!(output.reply.token, Token::default());
        assert_eq!(tracker.diagnostics().unwrap().capacity_rejections, 1);
        drop((origin, output));
        tracker.retire().unwrap();
        drop(tracker);
        assert!(weak.upgrade().is_some());
        let output = input.emit().unwrap();
        assert_eq!(output.value(), 11);
        assert_eq!(output.reply.token, Token::default());
        drop(input);
        assert!(weak.upgrade().is_none());
    }
}
