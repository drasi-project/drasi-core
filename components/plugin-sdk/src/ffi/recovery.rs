// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Versioned recovery metadata transported between independently built plugins.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::{ensure, Context, Result};
use drasi_lib::bootstrap::BootstrapContext;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::sources::SourceError;
use serde::{Deserialize, Serialize};

use super::payload::take_ffi_payload;
use super::vtables::{FfiSubscribeResult, FfiSubscriptionResponse};

const MAX_BOOTSTRAP_INVOCATION_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUBSCRIBE_FAILURE_BYTES: usize = 1024 * 1024;
const RECOVERY_PAYLOAD_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct BootstrapInvocation {
    version: u8,
    properties: HashMap<String, serde_json::Value>,
    next_sequence: u64,
    pub settings: Option<SourceSubscriptionSettings>,
}

impl BootstrapInvocation {
    pub fn context(&self, server_id: String, source_id: String) -> BootstrapContext {
        let context =
            BootstrapContext::with_properties(server_id, source_id, self.properties.clone());
        context
            .sequence_counter
            .store(self.next_sequence, Ordering::SeqCst);
        context
    }
}

/// Encode borrowed invocation metadata. The buffer remains owned by the caller.
pub fn encode_bootstrap_invocation(
    context: &BootstrapContext,
    settings: Option<&SourceSubscriptionSettings>,
) -> Result<Vec<u8>> {
    let payload = BootstrapInvocation {
        version: RECOVERY_PAYLOAD_VERSION,
        properties: context.properties().clone(),
        next_sequence: context.sequence_counter.load(Ordering::SeqCst),
        settings: settings.cloned(),
    };
    let bytes = rmp_serde::to_vec_named(&payload).context("Encoding bootstrap invocation")?;
    ensure!(
        bytes.len() <= MAX_BOOTSTRAP_INVOCATION_BYTES,
        "Bootstrap invocation exceeds the 8 MiB metadata limit"
    );
    Ok(bytes)
}

/// # Safety
/// For a non-null pointer and a length within the accepted limit, `ptr` must
/// be readable for `len` bytes for the duration of this call.
pub(crate) unsafe fn decode_bootstrap_invocation(
    ptr: *const u8,
    len: usize,
) -> Result<BootstrapInvocation> {
    ensure!(
        !ptr.is_null() && len > 0,
        "Missing bootstrap invocation metadata"
    );
    ensure!(
        len <= MAX_BOOTSTRAP_INVOCATION_BYTES,
        "Bootstrap invocation exceeds the 8 MiB metadata limit"
    );
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let invocation: BootstrapInvocation =
        rmp_serde::from_slice(bytes).context("Decoding bootstrap invocation")?;
    ensure!(
        invocation.version == RECOVERY_PAYLOAD_VERSION,
        "Unsupported bootstrap invocation version {}",
        invocation.version
    );
    Ok(invocation)
}

#[derive(Serialize, Deserialize)]
enum SubscribeFailure {
    PositionUnavailable {
        source_id: String,
        requested: Vec<u8>,
        earliest_available: Option<Vec<u8>>,
    },
    Other {
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
struct SubscribeFailureEnvelope {
    version: u8,
    failure: SubscribeFailure,
}

impl FfiSubscribeResult {
    pub(crate) fn catch_unwind(
        subscribe: impl FnOnce() -> Result<*mut FfiSubscriptionResponse>,
    ) -> Self {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(subscribe)) {
            Ok(Ok(response)) => Self::Success(response),
            Ok(Err(error)) => Self::from_error(error),
            Err(_) => Self::from_error(anyhow::anyhow!("Source subscribe panicked")),
        }
    }

    /// Serialize a failure without exposing Rust error objects to the peer.
    pub fn from_error(error: anyhow::Error) -> Self {
        let failure = match error.downcast_ref::<SourceError>() {
            Some(SourceError::PositionUnavailable {
                source_id,
                requested,
                earliest_available,
            }) => SubscribeFailure::PositionUnavailable {
                source_id: source_id.clone(),
                requested: requested.to_vec(),
                earliest_available: earliest_available.as_ref().map(|value| value.to_vec()),
            },
            None => SubscribeFailure::Other {
                message: format!("{error:#}"),
            },
        };
        let payload = SubscribeFailureEnvelope {
            version: RECOVERY_PAYLOAD_VERSION,
            failure,
        };
        let bytes = rmp_serde::to_vec_named(&payload)
            .expect("BUG: subscription error payload must be serializable")
            .into_boxed_slice();
        let payload_len = bytes.len();
        Self::Failure {
            payload_ptr: Box::into_raw(bytes).cast::<u8>(),
            payload_len,
            payload_drop_fn: Some(drop_failure_payload),
        }
    }

    /// Consume the result and reconstruct host-owned structured errors.
    ///
    /// # Safety
    /// Success must contain an owned FFI response; failure must contain a
    /// producer-owned buffer and its matching deallocator. Consume exactly once.
    pub unsafe fn into_result(self) -> Result<*mut FfiSubscriptionResponse> {
        match self {
            Self::Success(response) => {
                ensure!(
                    !response.is_null(),
                    "Subscribe returned a null success response"
                );
                Ok(response)
            }
            Self::Failure {
                payload_ptr,
                payload_len,
                payload_drop_fn,
            } => {
                let failure = unsafe {
                    take_ffi_payload(payload_ptr, payload_len, payload_drop_fn, |bytes| {
                        Some(decode_subscribe_failure(bytes))
                    })
                }
                .context("Missing or oversized subscription failure payload")??;
                Err(match failure {
                    SubscribeFailure::PositionUnavailable {
                        source_id,
                        requested,
                        earliest_available,
                    } => SourceError::PositionUnavailable {
                        source_id,
                        requested: requested.into(),
                        earliest_available: earliest_available.map(Into::into),
                    }
                    .into(),
                    SubscribeFailure::Other { message } => anyhow::anyhow!(message),
                })
            }
        }
    }
}

fn decode_subscribe_failure(bytes: &[u8]) -> Result<SubscribeFailure> {
    ensure!(
        bytes.len() <= MAX_SUBSCRIBE_FAILURE_BYTES,
        "Subscription failure exceeds the 1 MiB metadata limit"
    );
    let payload: SubscribeFailureEnvelope =
        rmp_serde::from_slice(bytes).context("Decoding subscription failure")?;
    ensure!(
        payload.version == RECOVERY_PAYLOAD_VERSION,
        "Unsupported subscription failure version {}",
        payload.version
    );
    if let SubscribeFailure::PositionUnavailable {
        requested,
        earliest_available,
        ..
    } = &payload.failure
    {
        let limit = drasi_lib::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES;
        ensure!(
            requested.len() <= limit
                && earliest_available
                    .as_ref()
                    .is_none_or(|value| value.len() <= limit),
            "Subscription failure contains an oversized source position"
        );
    }
    Ok(payload.failure)
}

extern "C" fn drop_failure_payload(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_metadata_rejects_missing_oversized_and_corrupt_payloads() {
        assert!(unsafe { decode_bootstrap_invocation(std::ptr::null(), 0) }.is_err());
        let byte = [0];
        assert!(unsafe {
            decode_bootstrap_invocation(byte.as_ptr(), MAX_BOOTSTRAP_INVOCATION_BYTES + 1)
        }
        .is_err());
        assert!(unsafe { decode_bootstrap_invocation(byte.as_ptr(), 1) }.is_err());
    }

    #[test]
    fn bootstrap_metadata_preserves_absent_settings_and_checks_version() {
        let context = BootstrapContext::new_minimal("server".into(), "source".into());
        let bytes = encode_bootstrap_invocation(&context, None).unwrap();
        let mut decoded =
            unsafe { decode_bootstrap_invocation(bytes.as_ptr(), bytes.len()) }.unwrap();
        assert!(decoded.settings.is_none());
        decoded.version += 1;
        let bytes = rmp_serde::to_vec_named(&decoded).unwrap();
        assert!(unsafe { decode_bootstrap_invocation(bytes.as_ptr(), bytes.len()) }.is_err());
    }

    #[test]
    fn malformed_failure_is_not_an_expired_position() {
        let bytes = vec![0xc1].into_boxed_slice();
        let result = FfiSubscribeResult::Failure {
            payload_len: bytes.len(),
            payload_ptr: Box::into_raw(bytes).cast::<u8>(),
            payload_drop_fn: Some(drop_failure_payload),
        };
        let error = unsafe { result.into_result() }.unwrap_err();
        assert!(error.downcast_ref::<SourceError>().is_none());
        assert!(format!("{error:#}").contains("Decoding subscription failure"));
    }

    #[test]
    fn unknown_failure_version_and_null_success_are_errors() {
        let bytes = rmp_serde::to_vec_named(&SubscribeFailureEnvelope {
            version: RECOVERY_PAYLOAD_VERSION + 1,
            failure: SubscribeFailure::Other {
                message: "fixture".into(),
            },
        })
        .unwrap();
        assert!(decode_subscribe_failure(&bytes).is_err());
        assert!(
            unsafe { FfiSubscribeResult::Success(std::ptr::null_mut()).into_result() }.is_err()
        );
    }

    #[test]
    fn malformed_failure_releases_the_producer_buffer_once() {
        static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        extern "C" fn counted_drop(ptr: *mut u8, len: usize) {
            DROPS.fetch_add(1, Ordering::SeqCst);
            drop_failure_payload(ptr, len);
        }
        let bytes = vec![0xc1].into_boxed_slice();
        let result = FfiSubscribeResult::Failure {
            payload_len: bytes.len(),
            payload_ptr: Box::into_raw(bytes).cast::<u8>(),
            payload_drop_fn: Some(counted_drop),
        };
        assert!(unsafe { result.into_result() }.is_err());
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failure_position_limits_are_inclusive() {
        let limit = drasi_lib::sources::base::SourceBase::MAX_SOURCE_POSITION_BYTES;
        for (requested, earliest, accepted) in [
            (limit, limit, true),
            (limit + 1, 0, false),
            (0, limit + 1, false),
        ] {
            let bytes = rmp_serde::to_vec_named(&SubscribeFailureEnvelope {
                version: RECOVERY_PAYLOAD_VERSION,
                failure: SubscribeFailure::PositionUnavailable {
                    source_id: "source".into(),
                    requested: vec![0; requested],
                    earliest_available: Some(vec![0; earliest]),
                },
            })
            .unwrap();
            assert_eq!(decode_subscribe_failure(&bytes).is_ok(), accepted);
        }
    }

    #[test]
    fn oversized_failure_is_released_and_does_not_trigger_a_reset() {
        static DROPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        extern "C" fn counted_drop(ptr: *mut u8, len: usize) {
            DROPS.fetch_add(1, Ordering::SeqCst);
            drop_failure_payload(ptr, len);
        }
        let bytes = vec![0; MAX_SUBSCRIBE_FAILURE_BYTES + 1].into_boxed_slice();
        let result = FfiSubscribeResult::Failure {
            payload_len: bytes.len(),
            payload_ptr: Box::into_raw(bytes).cast::<u8>(),
            payload_drop_fn: Some(counted_drop),
        };
        let error = unsafe { result.into_result() }.unwrap_err();
        assert!(error.downcast_ref::<SourceError>().is_none());
        assert!(error.to_string().contains("1 MiB"));
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }
}
