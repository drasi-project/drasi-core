// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::output_reads::{
    WireReadMetadata, WireReadPage, WireReplayMetadata, WireSnapshotMetadata,
};
use drasi_computation_contracts::{ChangeEnvelope, Record};
use std::{
    future::Future,
    iter::Peekable,
    pin::Pin,
    sync::atomic::AtomicBool,
    task::{Context, Poll},
};

type Records = Peekable<Box<dyn Iterator<Item = Record> + Send>>;
type Opening = Pin<Box<dyn Future<Output = Result<CapturedRead, OutputReadError>> + Send>>;

pub(in crate::export) struct Pages {
    rows: Records,
    envelopes: std::vec::IntoIter<ChangeEnvelope>,
    pending_frame: Option<Vec<u8>>,
    remaining_rows: usize,
    remaining_envelopes: usize,
    index: u64,
}

impl Pages {
    pub(in crate::export) fn new(
        captured: CapturedRead,
    ) -> Result<(WireReadMetadata, Self), OutputReadError> {
        fn snapshot(value: &OutputSnapshot) -> Result<WireSnapshotMetadata, OutputReadError> {
            Ok(WireSnapshotMetadata {
                cursor: WireCursor::new(value.cursor()),
                dataset: value.dataset().to_owned(),
                schema: value.schema().clone(),
                records: u64::try_from(value.records().len())
                    .map_err(|_| OutputReadError::invalid("snapshot record count exceeds u64"))?,
            })
        }
        fn replay(value: &OutputReplay) -> Result<WireReplayMetadata, OutputReadError> {
            Ok(WireReplayMetadata {
                after: WireCursor::new(value.after()),
                head: WireCursor::new(value.head()),
                envelopes: u64::try_from(value.envelopes().len())
                    .map_err(|_| OutputReadError::invalid("replay envelope count exceeds u64"))?,
            })
        }
        let (metadata, rows, history) = match captured {
            CapturedRead::Snapshot(value) => (
                WireReadMetadata::Snapshot(snapshot(&value)?),
                Some(value),
                None,
            ),
            CapturedRead::Replay(value) => {
                (WireReadMetadata::Replay(replay(&value)?), None, Some(value))
            }
            CapturedRead::QuerySnapshot(value, generation) => (
                WireReadMetadata::QuerySnapshot {
                    snapshot: snapshot(&value)?,
                    generation,
                },
                Some(value),
                None,
            ),
            CapturedRead::QueryReplay(value, generation) => (
                WireReadMetadata::QueryReplay {
                    replay: replay(&value)?,
                    generation,
                },
                None,
                Some(value),
            ),
            CapturedRead::QueryView {
                snapshot: value,
                retained,
                generation,
                config_hash,
            } => (
                WireReadMetadata::QueryView {
                    snapshot: snapshot(&value)?,
                    retained: replay(&retained)?,
                    generation,
                    config_hash,
                },
                Some(value),
                Some(retained),
            ),
        };
        let remaining_rows = rows.as_ref().map_or(0, |value| value.records().len());
        let rows: Box<dyn Iterator<Item = Record> + Send> = match rows {
            Some(value) => Box::new(
                value
                    .records()
                    .clone()
                    .into_iter()
                    .map(|(_, record)| record),
            ),
            None => Box::new(std::iter::empty()),
        };
        let envelopes = history.map_or_else(Vec::new, OutputReplay::into_envelopes);
        Ok((
            metadata,
            Self {
                rows: rows.peekable(),
                remaining_rows,
                remaining_envelopes: envelopes.len(),
                envelopes: envelopes.into_iter(),
                pending_frame: None,
                index: 0,
            },
        ))
    }

    pub(in crate::export) fn next(
        &mut self,
        codec: &BinaryEnvelopeCodec,
    ) -> Result<Vec<u8>, Failure> {
        use abi::output_reads::{MAX_PAGE_ITEMS, MAX_READ_BYTES, TARGET_PAGE_BYTES};
        let mut rows = Vec::new();
        let mut envelopes = Vec::new();
        let mut bytes = 256usize;
        while rows.len() + envelopes.len() < MAX_PAGE_ITEMS {
            if let Some(record) = self.rows.peek() {
                let size = record
                    .payload()
                    .len()
                    .saturating_add(record.identity().namespace().len())
                    .saturating_add(record.identity().value().len())
                    .saturating_add(256);
                if size > MAX_READ_BYTES - 256 {
                    return limit("one snapshot record exceeds the native page byte limit".into());
                }
                if !rows.is_empty() && bytes.saturating_add(size) > TARGET_PAGE_BYTES {
                    break;
                }
                bytes += size;
                rows.push(self.rows.next().expect("peeked snapshot record"));
                self.remaining_rows -= 1;
            } else if self.remaining_envelopes != 0 {
                if self.pending_frame.is_none() {
                    let envelope = self.envelopes.next().expect("counted retained envelope");
                    self.pending_frame = Some(codec.encode(&envelope).map_err(|error| {
                        Failure::failed(format!("retained output cannot be encoded: {error}"))
                    })?);
                }
                let size = self
                    .pending_frame
                    .as_ref()
                    .expect("encoded envelope")
                    .len()
                    .saturating_add(256);
                if size > MAX_READ_BYTES - 256 {
                    return limit("one replay envelope exceeds the native page byte limit".into());
                }
                if (!rows.is_empty() || !envelopes.is_empty())
                    && bytes.saturating_add(size) > TARGET_PAGE_BYTES
                {
                    break;
                }
                bytes += size;
                envelopes.push(WireFrame {
                    envelope: Cow::Owned(self.pending_frame.take().expect("encoded envelope")),
                });
                self.remaining_envelopes -= 1;
            } else {
                break;
            }
        }
        self.index = self
            .index
            .checked_add(1)
            .ok_or_else(|| Failure::failed("native transfer page index exhausted"))?;
        finish(Outcome::Ok(WireReadPage {
            index: self.index,
            done: self.done(),
            records: rows
                .iter()
                .map(|record| WireRecord {
                    namespace: Cow::Borrowed(record.identity().namespace()),
                    identity: Cow::Borrowed(record.identity().value()),
                    image: 0,
                    payload: Cow::Borrowed(record.payload()),
                })
                .collect(),
            envelopes,
        }))
    }

    pub(in crate::export) fn done(&self) -> bool {
        self.remaining_rows == 0 && self.remaining_envelopes == 0
    }
}

enum Slot {
    Opening(Opening),
    Ready(Pages),
    Finished,
    Closed,
}

pub(super) struct Transfer {
    binding: Arc<ReadBinding>,
    generation: u64,
    recovery: bool,
    slot: Mutex<Slot>,
    active: AtomicBool,
    closed: tokio::sync::watch::Sender<bool>,
}

impl Transfer {
    pub(super) fn close(&self) {
        let released =
            std::mem::replace(&mut *self.binding.lock_to_fence(&self.slot), Slot::Closed);
        drop(released);
        self.closed.send_replace(true);
    }

    fn poll(&self, cx: &mut Context<'_>, code: u32) -> Poll<Result<Vec<u8>, Failure>> {
        let mut slot = match self.slot.lock() {
            Ok(slot) => slot,
            Err(_) => return Poll::Ready(Err(Failure::protocol("native transfer state poisoned"))),
        };
        match (&mut *slot, code) {
            (Slot::Opening(opening), abi::output_reads::transfer_operation::START) => {
                let captured = match opening.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result.and_then(Pages::new),
                };
                match captured {
                    Ok((metadata, pages)) => {
                        *slot = Slot::Ready(pages);
                        Poll::Ready(finish(Outcome::Ok(metadata)))
                    }
                    Err(error) => {
                        *slot = Slot::Finished;
                        Poll::Ready(finish(Outcome::<WireReadMetadata>::from_result(Err(error))))
                    }
                }
            }
            (Slot::Ready(pages), abi::output_reads::transfer_operation::NEXT) => {
                let reply = pages.next(&self.binding.codec);
                if reply.is_err() || pages.done() {
                    *slot = Slot::Finished;
                }
                Poll::Ready(reply)
            }
            (Slot::Closed, _) => Poll::Ready(Err(retired())),
            _ => Poll::Ready(Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "native transfer must start once and finish each page in order",
            ))),
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        self.binding.transfer_count.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RequestGuard(Arc<Transfer>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}

static VTABLE: abi::output_reads::OutputReadVTable = abi::output_reads::OutputReadVTable {
    header: Header::new::<abi::output_reads::OutputReadVTable>(),
    release: Some(release),
    request: Some(request),
    transfer: None,
};

pub(super) unsafe extern "C" fn open(
    raw: *mut c_void,
    code: u32,
    input: BorrowedBytes,
    out: *mut abi::output_reads::OutputReadHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let handle = unsafe { state_ref::<Arc<HandleState>>(raw)? }.clone();
        if handle.binding.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let input =
            unsafe { transport::borrowed_bytes(input, abi::output_reads::MAX_REQUEST_BYTES)? };
        let cursor = handle.request_cursor(code, input)?;
        if matches!(
            code,
            abi::output_reads::operation::COMMITTED_HEAD
                | abi::output_reads::operation::QUERY_HEAD
                | abi::output_reads::operation::QUERY_OBSERVATION
                | abi::output_reads::operation::QUERY_RECOVERY_OBSERVATION
        ) {
            return Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "heads are single-message reads",
            ));
        }
        let (guard, _) = handle.binding.claim_read(code)?;
        let binding = handle.binding.clone();
        if binding.transfer_count.fetch_add(1, Ordering::AcqRel) >= abi::output_reads::MAX_TRANSFERS
        {
            binding.transfer_count.fetch_sub(1, Ordering::AcqRel);
            return Err(Failure::new(
                abi::status::BUSY,
                "too many native output transfers",
            ));
        }
        let transfer = Arc::new(Transfer {
            binding: binding.clone(),
            generation: guard.generation(),
            recovery: recovery_request(code),
            slot: Mutex::new(Slot::Opening(Box::pin(async move {
                handle.capture(code, cursor.as_ref()).await
            }))),
            active: AtomicBool::new(false),
            closed: tokio::sync::watch::channel(false).0,
        });
        {
            let mut transfers = binding
                .transfers
                .lock()
                .map_err(|_| Failure::protocol("native transfer registry poisoned"))?;
            transfers.retain(|entry| entry.strong_count() != 0);
            transfers.push(Arc::downgrade(&transfer));
        }
        guard.validate()?;
        unsafe {
            transport::write_out(
                out,
                abi::output_reads::OutputReadHandle {
                    state: Box::into_raw(Box::new(transfer)).cast(),
                    vtable: &VTABLE,
                },
            )
        }
    })
}

unsafe extern "C" fn release(raw: *mut c_void) {
    transport::drop_boundary(|| {
        let transfer = unsafe { Box::from_raw(raw.cast::<Arc<Transfer>>()) };
        transfer.close();
    });
}

unsafe extern "C" fn request(
    raw: *mut c_void,
    code: u32,
    input: BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let transfer = unsafe { state_ref::<Arc<Transfer>>(raw)? }.clone();
        if transfer.binding.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        if !unsafe { transport::borrowed_bytes(input, 0)? }.is_empty()
            || !matches!(
                code,
                abi::output_reads::transfer_operation::START
                    | abi::output_reads::transfer_operation::NEXT
            )
        {
            return Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "invalid native transfer request",
            ));
        }
        let (guard, mut changed) = transfer
            .binding
            .claim_mode(Some(transfer.generation), transfer.recovery)?;
        if transfer.active.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                abi::status::BUSY,
                "native transfer request already active",
            ));
        }
        let request_guard = RequestGuard(transfer.clone());
        let mut closed = transfer.closed.subscribe();
        let runtime = io_runtime()?;
        let damaged = transfer.binding.damaged.clone();
        let operation = transport::export_component_operation(
            async move {
                let _request_guard = request_guard;
                guard.validate()?;
                if *closed.borrow() {
                    return Err(retired());
                }
                let reply = tokio::select! {
                    biased;
                    _ = changed.changed() => return Err(retired()),
                    _ = closed.changed() => return Err(retired()),
                    result = tokio::time::timeout(READ_TIMEOUT, futures_util::future::poll_fn(
                        |cx| transfer.poll(cx, code),
                    )) => match result {
                        Ok(result) => result?,
                        Err(_) => return Err(Failure::failed("native transfer page timed out")),
                    },
                };
                guard.validate()?;
                Ok(reply)
            },
            Some(runtime),
            damaged,
            false,
        );
        unsafe { transport::write_out(out, operation) }
    })
}
