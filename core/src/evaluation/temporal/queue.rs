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

use crate::{
    interface::{FutureElementRef, IndexError},
    models::ElementReference,
};

use super::{codec, FutureTicket};

const TICKET_SOURCE: &str = "\0drasi:temporal:ticket";

// The queue's reference is opaque to providers. The real source attribution is
// retained in the ticket, without changing the public queue or plugin contracts.
pub(crate) fn encode(ticket: &FutureTicket) -> Result<ElementReference, IndexError> {
    let bytes = codec::encode_ticket(ticket).map_err(IndexError::other)?;
    let id = codec::key_field(&bytes);
    Ok(ElementReference::new(TICKET_SOURCE, &id))
}

pub(crate) fn decode(entry: FutureElementRef) -> Result<FutureTicket, IndexError> {
    if entry.element_ref.source_id.as_ref() != TICKET_SOURCE {
        return Err(IndexError::other(
            codec::TemporalCodecError::MigrationRequired,
        ));
    }
    let bytes = codec::decode_field(&entry.element_ref.element_id).map_err(IndexError::other)?;
    let ticket = codec::decode_ticket(&bytes).map_err(IndexError::other)?;
    if ticket.due_time != entry.due_time
        || ticket.original_time != entry.original_time
        || ticket.id.input.incarnation.0 != entry.group_signature
    {
        return Err(IndexError::CorruptedData);
    }
    Ok(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::temporal::fixtures;

    #[test]
    fn ticket_round_trip_preserves_identity_and_attribution() {
        let mut input = fixtures::input();
        let ticket = fixtures::ticket(&mut input);
        let entry = FutureElementRef {
            element_ref: encode(&ticket).unwrap(),
            original_time: ticket.original_time,
            due_time: ticket.due_time,
            group_signature: ticket.id.input.incarnation.0,
        };
        let restored = decode(entry).unwrap();
        assert_eq!(
            codec::encode_ticket(&restored).unwrap(),
            codec::encode_ticket(&ticket).unwrap()
        );
    }

    #[test]
    fn legacy_and_malformed_queue_payloads_are_errors() {
        let mut entry = FutureElementRef {
            element_ref: ElementReference::new("source", "node"),
            original_time: 10,
            due_time: 20,
            group_signature: 1,
        };
        assert!(decode(entry.clone()).is_err());
        for invalid in ["0", "gg", ""] {
            entry.element_ref = ElementReference::new(TICKET_SOURCE, invalid);
            assert!(decode(entry.clone()).is_err());
        }
    }
}
