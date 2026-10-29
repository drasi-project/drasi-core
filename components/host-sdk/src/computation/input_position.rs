// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use drasi_computation_contracts::{
    decode_bounded_messagepack, InputPositionComparator, InputPositionPolicy,
    MAX_SOURCE_POSITION_BYTES,
};
use drasi_computation_plugin_sdk::{
    abi::{self, consumer_progress::InputPositionPolicyV1},
    input_position::decode_key,
    transport::{take_reply, Failure},
};

use super::proxy::native_operation_cause;

struct Comparator(InputPositionPolicyV1);
// The producer alone dereferences and releases its immutable policy context.
unsafe impl Send for Comparator {}
unsafe impl Sync for Comparator {}
impl Drop for Comparator {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

impl InputPositionComparator for Comparator {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
        anyhow::ensure!(
            event.len() <= MAX_SOURCE_POSITION_BYTES && handled.len() <= MAX_SOURCE_POSITION_BYTES,
            "native input position exceeds the source cursor limit"
        );
        let bytes = unsafe {
            take_reply(self.0.is_after.expect("validated")(
                self.0.context,
                abi::BorrowedBytes::new(event),
                abi::BorrowedBytes::new(handled),
            ))
        }
        .map_err(native_operation_cause)?;
        decode_bounded_messagepack(&bytes, 1)
            .map_err(|error| native_operation_cause(Failure::protocol(error.to_string())))
    }
}

pub(super) unsafe fn from_borrowed(
    raw: InputPositionPolicyV1,
) -> anyhow::Result<Option<Arc<InputPositionPolicy>>> {
    raw.header.validate::<InputPositionPolicyV1>()?;
    if raw.context.is_null() {
        anyhow::ensure!(
            raw.retain.is_none()
                && raw.release.is_none()
                && raw.describe.is_none()
                && raw.is_after.is_none(),
            "empty native input position policy contains callbacks"
        );
        return Ok(None);
    }
    anyhow::ensure!(
        raw.retain.is_some()
            && raw.release.is_some()
            && raw.describe.is_some()
            && raw.is_after.is_some(),
        "incomplete native input position policy"
    );
    let description = unsafe { take_reply(raw.describe.expect("validated")(raw.context)) }
        .map_err(native_operation_cause)?;
    let key = decode_key(&description)?;
    unsafe { raw.retain.expect("validated")(raw.context) };
    let comparator = Arc::new(Comparator(raw));
    Ok(Some(Arc::new(InputPositionPolicy::new(key, comparator)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::SourceProgressKey;
    use drasi_computation_plugin_sdk::{
        consumer_progress::encode, input_position::encode_key, transport,
    };
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Foreign {
        references: AtomicUsize,
        description: Vec<u8>,
        response: Vec<u8>,
    }
    unsafe extern "C" fn retain(context: *mut c_void) {
        unsafe { &*context.cast::<Foreign>() }
            .references
            .fetch_add(1, Ordering::AcqRel);
    }
    unsafe extern "C" fn release(context: *mut c_void) {
        unsafe { &*context.cast::<Foreign>() }
            .references
            .fetch_sub(1, Ordering::AcqRel);
    }
    unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
        transport::reply_boundary(|| Ok(unsafe { &*context.cast::<Foreign>() }.description.clone()))
    }
    unsafe extern "C" fn compare(
        context: *mut c_void,
        _: abi::BorrowedBytes,
        _: abi::BorrowedBytes,
    ) -> abi::Reply {
        transport::reply_boundary(|| Ok(unsafe { &*context.cast::<Foreign>() }.response.clone()))
    }
    fn table(state: &Foreign) -> InputPositionPolicyV1 {
        InputPositionPolicyV1 {
            header: abi::Header::new::<InputPositionPolicyV1>(),
            context: state as *const Foreign as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            is_after: Some(compare),
        }
    }

    #[test]
    fn consumer_progress_input_policy_checks_tables_keys_boolean_replies_and_release(
    ) -> anyhow::Result<()> {
        let mut foreign = Foreign {
            references: AtomicUsize::new(1),
            description: encode_key(&SourceProgressKey::Source("source".into()))?,
            response: encode(&true)?,
        };
        assert!(unsafe { from_borrowed(InputPositionPolicyV1::null())? }.is_none());
        for field in 0..6 {
            let mut raw = table(&foreign);
            match field {
                0 => raw.header.size += 1,
                1 => raw.context = std::ptr::null_mut(),
                2 => raw.retain = None,
                3 => raw.release = None,
                4 => raw.describe = None,
                _ => raw.is_after = None,
            }
            assert!(unsafe { from_borrowed(raw) }.is_err());
            assert_eq!(foreign.references.load(Ordering::Acquire), 1);
        }
        let policy = unsafe { from_borrowed(table(&foreign))? }.unwrap();
        assert_eq!(policy.key(), &SourceProgressKey::Source("source".into()));
        assert!(policy.is_after(&[0], &[1])?);
        assert_eq!(foreign.references.load(Ordering::Acquire), 2);
        let clone = policy.clone();
        drop(policy);
        assert_eq!(foreign.references.load(Ordering::Acquire), 2);
        drop(clone);
        assert_eq!(foreign.references.load(Ordering::Acquire), 1);
        foreign.response = encode(&"not a boolean")?;
        let policy = unsafe { from_borrowed(table(&foreign))? }.unwrap();
        let error = policy.is_after(&[0], &[1]).unwrap_err();
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().code,
            abi::status::PROTOCOL
        );
        drop(policy);
        foreign.description = encode(&serde_json::json!({"version": 1, "key": {"Source": ""}}))?;
        assert!(unsafe { from_borrowed(table(&foreign)) }.is_err());
        assert_eq!(foreign.references.load(Ordering::Acquire), 1);
        Ok(())
    }
}
