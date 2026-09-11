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

use super::*;
use crate::evaluation::temporal::{codec, fixtures};

#[test]
fn part_membership_accepts_only_its_live_input_ids() {
    let input = fixtures::input().id;
    let part = PartState {
        namespace: input.namespace,
        part: input.part,
        inputs: BTreeSet::from([input]),
    };
    TemporalRecord::Part(part.clone()).validate().unwrap();
    let mut batch = TemporalBatch::new(part.namespace);
    batch.put(TemporalRecord::Part(part.clone())).unwrap();
    for wrong in [
        TemporalInputId {
            part: PartId(input.part.0 + 1),
            ..input
        },
        TemporalInputId {
            namespace: TemporalNamespace {
                epoch: QueryEpoch([99; 16]),
                ..input.namespace
            },
            ..input
        },
        TemporalInputId {
            namespace: TemporalNamespace {
                plan: PlanVersion(99),
                ..input.namespace
            },
            ..input
        },
    ] {
        assert!(batch
            .put(TemporalRecord::Part(PartState {
                inputs: BTreeSet::from([wrong]),
                ..part.clone()
            }))
            .is_err());
    }
    let mut empty = part;
    empty.inputs.clear();
    batch.put(TemporalRecord::Part(empty)).unwrap();
}

#[test]
fn net_true_condition_preserves_start_and_activation() {
    let mut state = TrueForState::default();
    state.settle(true, 10).unwrap();
    let activation = state.predicate.clone();
    state.settle(true, 20).unwrap();
    assert_eq!(state.predicate, activation);
    state.settle(false, 30).unwrap();
    state.settle(true, 40).unwrap();
    assert_eq!(
        state.predicate,
        PredicateState::True {
            since: 40,
            activation: Generation(1)
        }
    );
}

#[test]
fn replacement_and_recreation_do_not_reuse_ticket_identity() {
    let mut row = fixtures::input();
    let first = fixtures::ticket(&mut row);
    row.tickets.insert(first.id.clone(), first.clone());
    assert!(row.accepts(&first));

    row.source_revision = SourceRevision(5);
    row.evaluated_at.realtime = 115;
    assert!(
        row.accepts(&first),
        "unrelated source changes must not invalidate the ticket"
    );

    row.tickets.remove(&first.id);
    let replacement = fixtures::ticket(&mut row);
    row.tickets
        .insert(replacement.id.clone(), replacement.clone());
    assert!(!row.accepts(&first));
    assert!(row.accepts(&replacement));
    assert_ne!(first.id.generation, replacement.id.generation);

    let mut epoch = EpochState::new(row.id.namespace);
    assert_eq!(epoch.allocate().unwrap(), row.id.incarnation);
    let mut recreated = fixtures::input();
    recreated.id.incarnation = epoch.allocate().unwrap();
    let new_ticket = fixtures::ticket(&mut recreated);
    assert_ne!(first.id, new_ticket.id);
}

#[test]
fn independent_calls_slots_occurrences_and_inputs_have_distinct_tickets() {
    let mut row = fixtures::input();
    let first = row.allocate_ticket(fixtures::call(), 0).unwrap();
    let slot = row.allocate_ticket(fixtures::call(), 1).unwrap();
    let mut call = fixtures::call();
    call.occurrence.push(1);
    let occurrence = row.allocate_ticket(call.clone(), 0).unwrap();
    call.site.position_in_query += 1;
    let site = row.allocate_ticket(call, 0).unwrap();
    let ids = BTreeSet::from([first, slot, occurrence, site]);
    assert_eq!(ids.len(), 4);
}

#[test]
fn counters_fail_instead_of_wrapping() {
    let mut epoch = EpochState {
        namespace: fixtures::namespace(),
        next_incarnation: Incarnation(u64::MAX),
    };
    assert!(matches!(
        epoch.allocate(),
        Err(TemporalStateError::IncarnationExhausted)
    ));
    assert_eq!(epoch.next_incarnation, Incarnation(u64::MAX));
    let mut row = fixtures::input();
    row.next_ticket_generation = Generation(u64::MAX);
    assert!(matches!(
        row.allocate_ticket(fixtures::call(), 0),
        Err(TemporalStateError::GenerationExhausted)
    ));
    let mut state = TrueForState {
        predicate: PredicateState::False,
        next_activation: Generation(u64::MAX),
    };
    assert!(state.settle(true, 1).is_err());
    assert_eq!(state.predicate, PredicateState::False);
}

#[test]
fn deadline_clocks_never_reverse_realtime_or_replace_transaction_time() {
    let clock = ClockStamp {
        transaction_time: 10,
        realtime: 20,
    };
    assert_eq!(clock.at_deadline(15), clock);
    assert_eq!(
        clock.at_deadline(30),
        ClockStamp {
            transaction_time: 10,
            realtime: 30
        }
    );
}

#[test]
fn retirement_deletes_live_records_without_tombstones() {
    let mut row = fixtures::input();
    let ticket = fixtures::ticket(&mut row);
    row.tickets.insert(ticket.id.clone(), ticket.clone());
    let cell = FunctionCellId {
        owner: StateOwner::Input(row.id),
        call: fixtures::call(),
    };
    row.dependencies.insert(cell.clone());
    let mut batch = TemporalBatch::new(row.id.namespace);
    batch.put(TemporalRecord::Input(row.clone())).unwrap();
    let cancelled = batch.retire_input(&row).unwrap();
    let writes = codec::prepare_batch(&batch).unwrap();
    assert_eq!(cancelled.len(), 1);
    assert!(writes[&codec::encode_key(&TemporalKey::Input(row.id)).unwrap()].is_none());
    assert!(writes[&codec::encode_key(&TemporalKey::Origin(row.origin_key())).unwrap()].is_none());
    assert!(writes[&codec::encode_key(&TemporalKey::Cell(cell)).unwrap()].is_none());
    assert_eq!(writes.len(), 3);
}

#[test]
fn group_retirement_requires_no_members_or_default_row() {
    let row = fixtures::input();
    let id = TemporalGroupId {
        namespace: row.id.namespace,
        producer: PartId(0),
        incarnation: Incarnation(6),
    };
    let cell = FunctionCellId {
        owner: StateOwner::Group(id),
        call: fixtures::call(),
    };
    let mut group = GroupState {
        id,
        grouping: GroupingKey(vec![VariableValue::Null]),
        members: BTreeSet::from([row.id]),
        default_row: false,
        cells: BTreeSet::from([cell]),
        last_delivered: Presence::Absent,
    };
    let mut batch = TemporalBatch::new(id.namespace);
    assert!(batch.retire_group(&group).is_err());
    assert!(batch.mutations().is_empty());
    group.members.clear();
    group.default_row = true;
    assert!(batch.retire_group(&group).is_err());
    group.default_row = false;
    batch.retire_group(&group).unwrap();
    assert_eq!(batch.mutations().len(), 3);
}

#[test]
fn namespace_mismatch_and_invalid_links_are_rejected_before_batch_mutation() {
    let mut row = fixtures::input();
    let mut batch = TemporalBatch::new(row.id.namespace);
    row.id.namespace.plan = PlanVersion(999);
    assert!(batch.put(TemporalRecord::Input(row)).is_err());
    assert!(batch.mutations().is_empty());

    let mut row = fixtures::input();
    let mut ticket = fixtures::ticket(&mut row);
    ticket.id.input.incarnation = Incarnation(999);
    row.tickets.insert(ticket.id.clone(), ticket);
    assert!(batch.put(TemporalRecord::Input(row)).is_err());
    assert!(batch.mutations().is_empty());
}

#[test]
fn typed_origins_distinguish_parts_match_shapes_and_group_lifetimes() {
    let row = fixtures::input();
    let first = codec::encode_key(&TemporalKey::Origin(row.origin_key())).unwrap();
    let mut changed = row.origin_key();
    changed.part = PartId(2);
    assert_ne!(
        first,
        codec::encode_key(&TemporalKey::Origin(changed)).unwrap()
    );
    let a = ElementReference::new("s", "a");
    let b = ElementReference::new("s", "b");
    let fixed = InputOrigin::Match(MatchIdentity::Fixed {
        slots: vec![Some(a.clone()), Some(b.clone())],
    });
    let bounded = InputOrigin::Match(MatchIdentity::Bounded {
        nodes: vec![a, b],
        paths: vec![],
    });
    let key = |origin| {
        TemporalKey::Origin(OriginKey {
            namespace: row.id.namespace,
            part: row.id.part,
            origin,
        })
    };
    assert_ne!(
        codec::encode_key(&key(fixed)).unwrap(),
        codec::encode_key(&key(bounded)).unwrap()
    );
    let group = TemporalGroupId {
        namespace: row.id.namespace,
        producer: PartId(0),
        incarnation: Incarnation(5),
    };
    assert_ne!(
        codec::encode_key(&key(InputOrigin::Group(group))).unwrap(),
        codec::encode_key(&key(InputOrigin::Group(TemporalGroupId {
            incarnation: Incarnation(6),
            ..group
        })))
        .unwrap()
    );
}

#[test]
fn window_expiration_releases_samples_only_once() {
    let mut window = SlidingWindowState {
        samples: BTreeMap::from([
            (
                Generation(0),
                WindowSample {
                    revision: SourceRevision(1),
                    expires_at: 10,
                    value: VariableValue::Null,
                },
            ),
            (
                Generation(1),
                WindowSample {
                    revision: SourceRevision(2),
                    expires_at: 20,
                    value: VariableValue::Awaiting,
                },
            ),
        ]),
        next_sample: Generation(2),
    };
    assert_eq!(window.expire(10).len(), 1);
    assert!(window.expire(10).is_empty());
    assert_eq!(window.samples.len(), 1);
    assert_eq!(window.expire(30).len(), 1);
    assert!(window.samples.is_empty());
    assert_eq!(window.next_sample, Generation(2));
}
