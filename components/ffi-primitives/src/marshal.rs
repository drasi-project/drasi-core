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

//! Helpers for marshalling Rust `Option`s across the C ABI, which cannot carry
//! `Option<T>` directly.
//!
//! An `Option<u64>` is encoded as a `(value, present)` pair: when `present` is
//! `false` the `value` is meaningless and decodes back to `None`. Host and
//! plugin sides share these two functions so the encoding can never drift.

/// Encode an `Option<u64>` as a `(value, present)` pair for the C ABI.
///
/// `None` becomes `(0, false)`; `Some(v)` becomes `(v, true)`.
#[inline]
pub fn encode_optional_u64(opt: Option<u64>) -> (u64, bool) {
    match opt {
        Some(v) => (v, true),
        None => (0, false),
    }
}

/// Decode a `(value, present)` pair received across the C ABI back into an
/// `Option<u64>`. `value` is ignored when `present` is `false`.
#[inline]
pub fn decode_optional_u64(value: u64, present: bool) -> Option<u64> {
    present.then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_round_trips() {
        let (value, present) = encode_optional_u64(None);
        assert!(!present);
        assert_eq!(decode_optional_u64(value, present), None);
    }

    #[test]
    fn some_round_trips() {
        for v in [0u64, 1, 5, 42, u64::MAX] {
            let (value, present) = encode_optional_u64(Some(v));
            assert!(present);
            assert_eq!(value, v);
            assert_eq!(decode_optional_u64(value, present), Some(v));
        }
    }

    #[test]
    fn decode_ignores_value_when_absent() {
        // A non-zero value with present=false must still decode to None.
        assert_eq!(decode_optional_u64(999, false), None);
    }
}
