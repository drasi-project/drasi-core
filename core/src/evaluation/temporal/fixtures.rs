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

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{evaluation::variable_value::VariableValue, models::ElementReference};

pub(crate) fn namespace() -> TemporalNamespace {
    TemporalNamespace {
        epoch: QueryEpoch([7; 16]),
        plan: PlanVersion(1),
    }
}

pub(crate) fn call() -> FunctionCall {
    FunctionCall {
        site: FunctionSite {
            part: PartId(1),
            position_in_query: 37,
        },
        occurrence: vec![2, 3],
    }
}

pub(crate) fn input() -> RetainedInput {
    let clock = ClockStamp {
        transaction_time: 100,
        realtime: 110,
    };
    RetainedInput {
        id: TemporalInputId {
            namespace: namespace(),
            part: PartId(1),
            incarnation: Incarnation(0),
        },
        origin: InputOrigin::Match(MatchIdentity::Fixed {
            slots: vec![Some(ElementReference::new("source", "anchor")), None],
        }),
        variables: BTreeMap::from([("value".into(), VariableValue::Awaiting)]),
        source_revision: SourceRevision(4),
        source_clock: clock,
        evaluated_at: clock,
        context: SavedContext {
            clock,
            input_grouping_hash: 13,
            solution_signature: Some(17),
            anchor: None,
        },
        row_signature: 19,
        applied: AppliedContribution::default(),
        dependencies: BTreeSet::new(),
        tickets: BTreeMap::new(),
        history: BTreeMap::new(),
        values: BTreeMap::new(),
        next_ticket_generation: Generation(0),
    }
}

pub(crate) fn ticket(input: &mut RetainedInput) -> FutureTicket {
    FutureTicket {
        id: input.allocate_ticket(call(), 0).unwrap(),
        cell: None,
        activation: Generation(0),
        original_time: 110,
        due_time: 120,
        attribution: Some(ElementReference::new("source", "anchor")),
    }
}
