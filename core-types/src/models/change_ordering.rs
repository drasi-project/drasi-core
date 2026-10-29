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

use serde::{Deserialize, Serialize};

/// How a receiving runtime orders one event for stale-change admission.
///
/// This is an immutable per-event policy. It is not a durability guarantee or a
/// capability flag. `ProducerSequence` may be selected only after the receiver
/// has validated the producer's identity and epoch and accepted its output
/// sequence monotonically; the event's business time is still carried unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeOrdering {
    /// Source event time is authoritative and stale timestamps are rejected.
    #[default]
    EventTime,
    /// The validated producer's accepted output sequence is authoritative.
    ProducerSequence,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_event_time_and_serde_names_are_stable() {
        assert_eq!(ChangeOrdering::default(), ChangeOrdering::EventTime);
        for (value, name) in [
            (ChangeOrdering::EventTime, "\"EventTime\""),
            (ChangeOrdering::ProducerSequence, "\"ProducerSequence\""),
        ] {
            assert_eq!(serde_json::to_string(&value).unwrap(), name);
            assert_eq!(serde_json::from_str::<ChangeOrdering>(name).unwrap(), value);
        }
        assert!(serde_json::from_str::<ChangeOrdering>("\"Sequence\"").is_err());
    }
}
