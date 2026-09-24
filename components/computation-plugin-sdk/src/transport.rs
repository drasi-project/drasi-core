// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Ownership helpers shared as source code, never as Rust objects across a cdylib.
//! Unsafe entry points require valid pointers obeying the ABI ownership contract.

use std::{
    ffi::c_void,
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Wake as RustWake, Waker},
};

use crate::abi::{self, status, BorrowedBytes, Header, OperationHandle, OwnedBytes, Reply, Status};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: u32,
    pub message: String,
    pub retryable: bool,
}
impl Failure {
    pub fn new(code: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
        }
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(status::FAILED, message)
    }
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(status::PROTOCOL, message)
    }
    pub fn cancelled() -> Self {
        Self::new(status::CANCELLED, "native operation was cancelled")
    }
    pub fn closed() -> Self {
        Self::new(status::CLOSED, "native capability is no longer active")
    }
    pub fn panicked() -> Self {
        Self::new(status::PANICKED, "native operation panicked")
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native error {}: {}", self.code, self.message)
    }
}
impl std::error::Error for Failure {}
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<Self>() {
            Ok(failure) => failure,
            Err(error) => Self::failed(format!("{error:#}")),
        }
    }
}

pub fn drop_boundary(action: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(action));
}
pub fn status_boundary(action: impl FnOnce() -> Result<(), Failure>) -> Status {
    status_result(
        catch_unwind(AssertUnwindSafe(action)).unwrap_or_else(|_| Err(Failure::panicked())),
    )
}
pub fn reply_boundary(action: impl FnOnce() -> Result<Vec<u8>, Failure>) -> Reply {
    reply_result(
        catch_unwind(AssertUnwindSafe(action)).unwrap_or_else(|_| Err(Failure::panicked())),
    )
}
pub fn status_result(result: Result<(), Failure>) -> Status {
    match result {
        Ok(()) => Status::ok(),
        Err(mut error) => {
            if error.code == status::OK {
                error.code = status::FAILED;
            }
            Status {
                code: error.code,
                error: owned_bytes(serde_json::to_vec(&error).expect("Failure is serializable")),
            }
        }
    }
}
pub fn reply_result(result: Result<Vec<u8>, Failure>) -> Reply {
    match result {
        Ok(bytes) if bytes.len() <= abi::MAX_MESSAGE_BYTES => Reply {
            status: Status::ok(),
            payload: owned_bytes(bytes),
        },
        Ok(_) => Reply {
            status: status_result(Err(Failure::protocol("native output exceeds size limit"))),
            payload: OwnedBytes::empty(),
        },
        Err(error) => Reply {
            status: status_result(Err(error)),
            payload: OwnedBytes::empty(),
        },
    }
}

pub fn owned_bytes(bytes: Vec<u8>) -> OwnedBytes {
    if bytes.is_empty() {
        return OwnedBytes::empty();
    }
    let bytes = Box::new(bytes);
    OwnedBytes {
        data: bytes.as_ptr(),
        len: bytes.len(),
        context: Box::into_raw(bytes).cast(),
        release: Some(release_bytes),
    }
}
unsafe extern "C" fn release_bytes(context: *mut c_void) {
    drop_boundary(|| unsafe { drop(Box::from_raw(context.cast::<Vec<u8>>())) });
}

/// # Safety
/// Nonempty data must point to `len` readable bytes for the returned borrow.
pub unsafe fn borrowed_bytes<'a>(bytes: BorrowedBytes, limit: usize) -> Result<&'a [u8], Failure> {
    if bytes.len > limit || bytes.len > isize::MAX as usize {
        return Err(Failure::protocol("native byte buffer exceeds size limit"));
    }
    if bytes.len == 0 {
        return Ok(&[]);
    }
    if bytes.data.is_null() {
        return Err(Failure::protocol("null native byte buffer"));
    }
    Ok(unsafe { std::slice::from_raw_parts(bytes.data, bytes.len) })
}

struct BufferGuard(OwnedBytes);
impl Drop for BufferGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.context) };
        }
    }
}

/// # Safety
/// The buffer and its producer-side release function must obey the ABI contract.
pub unsafe fn take_bytes(bytes: OwnedBytes, limit: usize) -> Result<Vec<u8>, Failure> {
    let guard = BufferGuard(bytes);
    let bytes = &guard.0;
    let canonical_empty = bytes.data.is_null()
        && bytes.len == 0
        && bytes.context.is_null()
        && bytes.release.is_none();
    if !canonical_empty && (bytes.release.is_none() || bytes.context.is_null()) {
        return Err(Failure::protocol(
            "native owned buffer has no producer-side owner/release",
        ));
    }
    Ok(unsafe {
        borrowed_bytes(
            BorrowedBytes {
                data: bytes.data,
                len: bytes.len,
            },
            limit,
        )?
    }
    .to_vec())
}

/// # Safety
/// Error buffer ownership is transferred according to the ABI.
pub unsafe fn take_status(status: Status) -> Result<(), Failure> {
    let bytes = unsafe { take_bytes(status.error, abi::MAX_METADATA_BYTES)? };
    if status.code == status::OK {
        if !bytes.is_empty() {
            return Err(Failure::protocol("success carries a native error"));
        }
        return Ok(());
    }
    let failure = serde_json::from_slice::<Failure>(&bytes)
        .map_err(|_| Failure::protocol("native failure is missing structured error details"))?;
    if failure.code != status.code || failure.message.is_empty() {
        return Err(Failure::protocol("invalid native failure details"));
    }
    Err(failure)
}

/// # Safety
/// Both buffers transfer ownership and must obey their producer's contract.
pub unsafe fn take_reply(reply: Reply) -> Result<Vec<u8>, Failure> {
    let result = unsafe { take_status(reply.status) };
    let payload = unsafe { take_bytes(reply.payload, abi::MAX_MESSAGE_BYTES) };
    result?;
    payload
}

/// Read only the frozen header until its version/size is accepted.
///
/// # Safety
/// `table` must point to at least a readable Header. On a matching header it must
/// point to an initialized T whose first field is that Header, valid for 'a.
pub unsafe fn checked_table<'a, T>(table: *const T) -> Result<&'a T, Failure> {
    if table.is_null() || !table.is_aligned() {
        return Err(Failure::protocol("null or unaligned native vtable"));
    }
    let header = unsafe { ptr::read(table.cast::<Header>()) };
    header
        .validate::<T>()
        .map_err(|error| Failure::protocol(error.to_string()))?;
    Ok(unsafe { &*table })
}

struct ForeignWake(abi::Wake);
// Only callback functions touch the producing side's state. Their ABI requires
// thread safety, and this side owns exactly one retained reference.
unsafe impl Send for ForeignWake {}
unsafe impl Sync for ForeignWake {}
impl ForeignWake {
    unsafe fn from_borrowed(wake: *const abi::Wake) -> Result<Self, Failure> {
        let wake = *unsafe { checked_table(wake)? };
        if wake.context.is_null()
            || wake.retain.is_none()
            || wake.release.is_none()
            || wake.wake.is_none()
        {
            return Err(Failure::protocol("incomplete native wake callbacks"));
        }
        unsafe { wake.retain.expect("validated")(wake.context) };
        Ok(Self(wake))
    }
}
impl RustWake for ForeignWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        unsafe { self.0.wake.expect("validated")(self.0.context) };
    }
}
impl Drop for ForeignWake {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

type Work = Pin<Box<dyn Future<Output = Result<Vec<u8>, Failure>> + Send>>;
struct Operation {
    future: Option<Work>,
    terminal: u32,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
}

/// Create a producer-owned, caller-polled operation. The optional runtime is
/// entered only on this side while polling; no runtime or future crosses FFI.
pub fn export_operation(
    future: impl Future<Output = Result<Vec<u8>, Failure>> + Send + 'static,
    runtime: Option<Arc<tokio::runtime::Runtime>>,
) -> OperationHandle {
    OperationHandle {
        state: Box::into_raw(Box::new(Operation {
            future: Some(Box::pin(future)),
            terminal: 0,
            runtime,
        }))
        .cast(),
        vtable: &OPERATION_VTABLE,
    }
}

static OPERATION_VTABLE: abi::OperationVTable = abi::OperationVTable {
    header: Header::new::<abi::OperationVTable>(),
    poll: Some(poll_operation),
    cancel: Some(cancel_operation),
    release: Some(release_operation),
};

unsafe extern "C" fn poll_operation(state: *mut c_void, wake: *const abi::Wake) -> abi::PollResult {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if state.is_null() {
            return abi::PollResult {
                state: abi::POLL_READY,
                reply: reply_result(Err(Failure::protocol("null native operation"))),
            };
        }
        let operation = unsafe { &mut *state.cast::<Operation>() };
        let Some(future) = operation.future.as_mut() else {
            let error = if operation.terminal == status::CANCELLED {
                Failure::cancelled()
            } else {
                Failure::protocol("native operation already completed")
            };
            return abi::PollResult {
                state: abi::POLL_READY,
                reply: reply_result(Err(error)),
            };
        };
        let polled = catch_unwind(AssertUnwindSafe(|| {
            let waker = Waker::from(Arc::new(unsafe { ForeignWake::from_borrowed(wake)? }));
            let _entered = operation.runtime.as_ref().map(|runtime| runtime.enter());
            Ok::<_, Failure>(future.as_mut().poll(&mut Context::from_waker(&waker)))
        }));
        let result = match polled {
            Ok(Ok(Poll::Pending)) => {
                return abi::PollResult {
                    state: abi::POLL_PENDING,
                    reply: Reply::empty(),
                }
            }
            Ok(Ok(Poll::Ready(result))) => result,
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Failure::panicked()),
        };
        operation.terminal = result
            .as_ref()
            .err()
            .map_or(status::PROTOCOL, |error| error.code);
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(operation.future.take())));
        abi::PollResult {
            state: abi::POLL_READY,
            reply: reply_result(if dropped.is_err() {
                Err(Failure::panicked())
            } else {
                result
            }),
        }
    }));
    result.unwrap_or_else(|_| abi::PollResult {
        state: abi::POLL_READY,
        reply: reply_result(Err(Failure::panicked())),
    })
}

unsafe extern "C" fn cancel_operation(state: *mut c_void) -> Status {
    status_boundary(|| {
        if state.is_null() {
            return Err(Failure::protocol("null native operation"));
        }
        let operation = unsafe { &mut *state.cast::<Operation>() };
        if operation.future.is_some() {
            operation.terminal = status::CANCELLED;
            drop(operation.future.take());
        }
        Ok(())
    })
}
unsafe extern "C" fn release_operation(state: *mut c_void) {
    drop_boundary(|| {
        if !state.is_null() {
            unsafe { drop(Box::from_raw(state.cast::<Operation>())) };
        }
    });
}

struct WakeContext {
    waker: Mutex<Option<Waker>>,
    closed: AtomicBool,
}
impl WakeContext {
    fn new() -> Self {
        Self {
            waker: Mutex::new(None),
            closed: AtomicBool::new(false),
        }
    }
    fn table(this: &Arc<Self>) -> abi::Wake {
        abi::Wake {
            header: Header::new::<abi::Wake>(),
            context: Arc::as_ptr(this).cast_mut().cast(),
            retain: Some(retain_wake),
            release: Some(release_wake),
            wake: Some(wake_host),
        }
    }
}
unsafe extern "C" fn retain_wake(context: *mut c_void) {
    drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<WakeContext>()) });
}
unsafe extern "C" fn release_wake(context: *mut c_void) {
    drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<WakeContext>()) });
}
unsafe extern "C" fn wake_host(context: *mut c_void) {
    drop_boundary(|| {
        let context = unsafe { &*context.cast::<WakeContext>() };
        if context.closed.load(Ordering::Acquire) {
            return;
        }
        let waker = context
            .waker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    });
}

/// A local Future around a foreign C operation. Drop cancels/releases work but
/// never frees a wake context still retained by the producer or an I/O driver.
pub struct OperationFuture {
    handle: OperationHandle,
    wake: Arc<WakeContext>,
    done: bool,
}
// The native operation contract permits moving the owner between threads. It
// does not permit simultaneous poll/release of one handle.
unsafe impl Send for OperationFuture {}
impl OperationFuture {
    /// # Safety
    /// Transfers a unique, valid operation handle produced by a pinned library.
    pub unsafe fn new(handle: OperationHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(handle.vtable)? };
        if handle.state.is_null()
            || table.poll.is_none()
            || table.cancel.is_none()
            || table.release.is_none()
        {
            return Err(Failure::protocol("incomplete native operation table"));
        }
        Ok(Self {
            handle,
            wake: Arc::new(WakeContext::new()),
            done: false,
        })
    }
    pub fn cancel(&mut self) -> Result<(), Failure> {
        let table = unsafe { &*self.handle.vtable };
        unsafe { take_status(table.cancel.expect("validated")(self.handle.state)) }
    }
}
impl Future for OperationFuture {
    type Output = Result<Vec<u8>, Failure>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.done {
            return Poll::Ready(Err(Failure::protocol(
                "native future polled after completion",
            )));
        }
        *self.wake.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(cx.waker().clone());
        let wake = WakeContext::table(&self.wake);
        let table = unsafe { &*self.handle.vtable };
        let polled = unsafe { table.poll.expect("validated")(self.handle.state, &wake) };
        let reply = unsafe { take_reply(polled.reply) };
        if polled.state == abi::POLL_PENDING && reply.as_ref().is_ok_and(Vec::is_empty) {
            return Poll::Pending;
        }
        self.done = true;
        self.wake.closed.store(true, Ordering::Release);
        Poll::Ready(match polled.state {
            abi::POLL_READY => reply,
            abi::POLL_PENDING => Err(reply
                .err()
                .unwrap_or_else(|| Failure::protocol("pending operation carries output"))),
            _ => Err(Failure::protocol("unknown native poll state")),
        })
    }
}
impl Drop for OperationFuture {
    fn drop(&mut self) {
        self.wake.closed.store(true, Ordering::Release);
        self.wake
            .waker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let table = unsafe { &*self.handle.vtable };
        if !self.done {
            let _ = unsafe { take_status(table.cancel.expect("validated")(self.handle.state)) };
        }
        unsafe { table.release.expect("validated")(self.handle.state) };
    }
}

/// Write a newly owned handle only to a nonnull output pointer.
///
/// # Safety
/// out must be writable for T. The caller must not already own a value in it.
pub unsafe fn write_out<T>(out: *mut T, value: T) -> Result<(), Failure> {
    if out.is_null() {
        return Err(Failure::protocol("null native output pointer"));
    }
    unsafe { ptr::write(out, value) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test(flavor = "current_thread")]
    async fn poll_wake_and_owned_output_cross_the_real_c_table() {
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        let handle = export_operation(
            async move {
                receive.await.map_err(|_| Failure::closed())?;
                Ok(vec![1, 2, 3])
            },
            None,
        );
        let mut future = Box::pin(unsafe { OperationFuture::new(handle) }.unwrap());
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx).is_pending())).await
        );
        send.send(()).unwrap();
        assert_eq!(future.await.unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn cancellation_never_becomes_success() {
        let handle = export_operation(std::future::pending(), None);
        let mut operation = unsafe { OperationFuture::new(handle) }.unwrap();
        operation.cancel().unwrap();
        assert_eq!(operation.await.unwrap_err().code, status::CANCELLED);
        let handle = export_operation(async { Err(Failure::failed("failure")) }, None);
        assert_eq!(
            unsafe { OperationFuture::new(handle) }
                .unwrap()
                .await
                .unwrap_err()
                .code,
            status::FAILED
        );
        let handle = export_operation(
            async {
                panic!("contained panic");
            },
            None,
        );
        assert_eq!(
            unsafe { OperationFuture::new(handle) }
                .unwrap()
                .await
                .unwrap_err()
                .code,
            status::PANICKED
        );
    }

    #[tokio::test]
    async fn wake_during_poll_is_not_lost() {
        let mut first = true;
        let work = std::future::poll_fn(move |cx| {
            if first {
                first = false;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(Ok(vec![7]))
            }
        });
        let operation = unsafe { OperationFuture::new(export_operation(work, None)) }.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), operation)
                .await
                .unwrap()
                .unwrap(),
            vec![7]
        );
    }

    struct RememberWake(Arc<Mutex<Option<Waker>>>);
    impl Future for RememberWake {
        type Output = Result<Vec<u8>, Failure>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            *self.0.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
    #[tokio::test]
    async fn callback_owner_survives_operation_cancellation_and_release() {
        let remembered = Arc::new(Mutex::new(None));
        let handle = export_operation(RememberWake(remembered.clone()), None);
        let mut future = Box::pin(unsafe { OperationFuture::new(handle) }.unwrap());
        let weak = Arc::downgrade(&future.wake);
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(future);
        assert!(weak.upgrade().is_some());
        let late_wake = remembered.lock().unwrap().take().unwrap();
        late_wake.wake_by_ref();
        drop(late_wake);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn malformed_error_releases_both_producer_buffers() {
        static RELEASES: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn release(context: *mut c_void) {
            RELEASES.fetch_add(1, Ordering::SeqCst);
            unsafe { release_bytes(context) };
        }
        let mut error = owned_bytes(b"not an error object".to_vec());
        error.release = Some(release);
        let mut payload = owned_bytes(b"must also release".to_vec());
        payload.release = Some(release);
        assert!(unsafe {
            take_reply(Reply {
                status: Status {
                    code: status::FAILED,
                    error,
                },
                payload,
            })
        }
        .is_err());
        assert_eq!(RELEASES.load(Ordering::SeqCst), 2);
    }
}
