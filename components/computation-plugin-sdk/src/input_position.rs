// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

use drasi_computation_contracts::{
    InputPositionPolicy, SourceProgressKey, MAX_SOURCE_POSITION_BYTES,
};
use serde::{Deserialize, Serialize};

use crate::{
    abi::{
        self,
        consumer_progress::{InputPositionPolicyV1, MAX_POSITION_BYTES, VERSION},
    },
    consumer_progress::{decode, encode},
    progress::Key,
    transport::{self, Failure},
};

const _: () = assert!(MAX_POSITION_BYTES == MAX_SOURCE_POSITION_BYTES);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    version: u32,
    key: Key,
}

pub fn encode_key(key: &SourceProgressKey) -> anyhow::Result<Vec<u8>> {
    let name = match key {
        SourceProgressKey::Source(source) => source.as_str(),
        SourceProgressKey::Stream(stream) => stream.as_str(),
    };
    anyhow::ensure!(
        name.len() <= abi::consumer_progress::MAX_PROGRESS_BYTES,
        "native input position key exceeds the progress byte limit"
    );
    Key::from(key).into_key()?;
    encode(&Identity {
        version: VERSION,
        key: Key::from(key),
    })
}

pub fn decode_key(bytes: &[u8]) -> anyhow::Result<SourceProgressKey> {
    let identity: Identity = decode(bytes)?;
    anyhow::ensure!(
        identity.version == VERSION,
        "unsupported native input position policy version"
    );
    identity.key.into_key()
}

struct State {
    policy: Arc<InputPositionPolicy>,
    description: Vec<u8>,
    damaged: Arc<transport::PanicState>,
}

pub(crate) struct ExportedInputPositionPolicy(Arc<State>);

impl ExportedInputPositionPolicy {
    pub(crate) fn new(
        policy: Arc<InputPositionPolicy>,
        damaged: Arc<transport::PanicState>,
    ) -> anyhow::Result<Self> {
        let description = encode_key(policy.key())?;
        Ok(Self(Arc::new(State {
            policy,
            description,
            damaged,
        })))
    }

    pub(crate) fn is_policy(&self, policy: &Arc<InputPositionPolicy>) -> bool {
        Arc::ptr_eq(&self.0.policy, policy)
    }

    pub(crate) fn table(&self) -> InputPositionPolicyV1 {
        InputPositionPolicyV1 {
            header: abi::Header::new::<InputPositionPolicyV1>(),
            context: Arc::as_ptr(&self.0) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            is_after: Some(is_after),
        }
    }
}

unsafe fn state<'a>(context: *mut c_void) -> Result<&'a State, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null native input position policy"));
    }
    Ok(unsafe { &*context.cast::<State>() })
}
unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        if state.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        Ok(state.description.clone())
    })
}
unsafe extern "C" fn is_after(
    context: *mut c_void,
    event: abi::BorrowedBytes,
    handled: abi::BorrowedBytes,
) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        if state.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let event = unsafe { transport::borrowed_bytes(event, MAX_POSITION_BYTES)? };
        let handled = unsafe { transport::borrowed_bytes(handled, MAX_POSITION_BYTES)? };
        let result = match catch_unwind(AssertUnwindSafe(|| state.policy.is_after(event, handled)))
        {
            Ok(result) => result.map_err(Failure::from)?,
            Err(_) => {
                state.damaged.quarantine();
                return Err(Failure::panicked());
            }
        };
        encode(&result).map_err(Failure::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{InputPositionComparator, StreamId};
    use transport::take_reply;

    struct Partitioned;
    impl InputPositionComparator for Partitioned {
        fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
            assert_ne!(event.first(), Some(&255), "fixture comparator panic");
            let [partition, offset] = event else {
                anyhow::bail!("invalid event position")
            };
            let prior = handled
                .get(usize::from(*partition))
                .ok_or_else(|| anyhow::anyhow!("unknown partition"))?;
            Ok(offset > prior)
        }
    }
    fn compare(raw: &InputPositionPolicyV1, event: &[u8], handled: &[u8]) -> Result<bool, Failure> {
        let bytes = unsafe {
            take_reply(raw.is_after.unwrap()(
                raw.context,
                abi::BorrowedBytes::new(event),
                abi::BorrowedBytes::new(handled),
            ))?
        };
        decode(&bytes).map_err(Failure::from)
    }

    #[test]
    fn consumer_progress_input_policy_preserves_partition_semantics_bounds_and_ownership(
    ) -> anyhow::Result<()> {
        let policy = Arc::new(InputPositionPolicy::new(
            SourceProgressKey::Source("source".into()),
            Arc::new(Partitioned),
        )?);
        let weak = Arc::downgrade(&policy);
        let exported =
            ExportedInputPositionPolicy::new(policy, Arc::new(transport::PanicState::default()))?;
        let raw = exported.table();
        let description = unsafe { take_reply(raw.describe.unwrap()(raw.context))? };
        assert_eq!(
            decode_key(&description)?,
            SourceProgressKey::Source("source".into())
        );
        assert!(compare(&raw, &[0, 7], &[6, 90])?);
        assert!(!compare(&raw, &[1, 89], &[6, 90])?);
        assert!(!compare(&raw, &[1, 90], &[6, 90])?);
        assert!(compare(&raw, &[2, 1], &[6, 90])
            .unwrap_err()
            .message
            .contains("unknown partition"));
        assert!(compare(&raw, &[0], &[6, 90]).is_err());
        assert_eq!(
            compare(&raw, &vec![0; MAX_POSITION_BYTES + 1], &[0])
                .unwrap_err()
                .code,
            abi::status::PROTOCOL
        );
        assert_eq!(
            compare(&raw, &[0, 1], &vec![0; MAX_POSITION_BYTES + 1])
                .unwrap_err()
                .code,
            abi::status::PROTOCOL
        );
        assert!(compare(&raw, &[0, 1], &vec![0; MAX_POSITION_BYTES])?);
        unsafe { raw.retain.unwrap()(raw.context) };
        drop(exported);
        assert!(weak.upgrade().is_some());
        assert!(compare(&raw, &[0, 7], &[6, 90])?);
        unsafe { raw.release.unwrap()(raw.context) };
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn consumer_progress_input_policy_preserves_errors_and_quarantines_panics() -> anyhow::Result<()>
    {
        let damaged = Arc::new(transport::PanicState::default());
        let exported = ExportedInputPositionPolicy::new(
            Arc::new(InputPositionPolicy::new(
                SourceProgressKey::Stream(StreamId::try_new("source/out")?),
                Arc::new(Partitioned),
            )?),
            damaged.clone(),
        )?;
        assert_eq!(
            compare(&exported.table(), &[2, 1], &[0]).unwrap_err().code,
            abi::status::FAILED
        );
        assert!(!damaged.is_damaged());
        assert_eq!(
            compare(&exported.table(), &[255], &[0]).unwrap_err().code,
            abi::status::PANICKED
        );
        assert!(damaged.is_damaged());
        assert_eq!(
            compare(&exported.table(), &[0, 1], &[0]).unwrap_err().code,
            abi::status::PANICKED
        );
        Ok(())
    }

    #[test]
    fn consumer_progress_input_policy_rejects_invalid_key_descriptions() -> anyhow::Result<()> {
        assert!(encode_key(&SourceProgressKey::Source(String::new())).is_err());
        assert!(decode_key(&encode(&Identity {
            version: VERSION + 1,
            key: Key::Source("source".into())
        })?)
        .is_err());
        assert!(decode_key(&encode(&Identity {
            version: VERSION,
            key: Key::Source(String::new())
        })?)
        .is_err());
        for key in [
            SourceProgressKey::Source("source".into()),
            SourceProgressKey::Stream(StreamId::try_new("source/out")?),
        ] {
            assert_eq!(decode_key(&encode_key(&key)?)?, key);
        }
        Ok(())
    }
}
