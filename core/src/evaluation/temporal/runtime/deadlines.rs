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
    evaluation::temporal::{
        ClockStamp, FunctionCell, FunctionState, FutureTicket, GroupState, PredicateState,
        RetainedInput, StateOwner, TemporalKey, TemporalNamespace, TemporalRecord,
        TemporalStateError,
    },
    interface::TemporalIndex,
};

use super::{due::ReadyHead, TemporalRuntimeError};

#[derive(Debug)]
pub enum DeadlineTarget {
    Obsolete,
    Ready(Box<ReadyInput>),
}

#[derive(Debug)]
pub struct ReadyInput {
    pub input: RetainedInput,
    pub evaluation_clock: ClockStamp,
    pub cell: Option<FunctionCell>,
    pub group: Option<GroupState>,
}

/// Resolve a ticket against retained state, never against an anchor rematch.
/// Hold the query's exclusive root across head inspection, pop, these reads, and settlement.
/// This does not consume the ticket, mutate history, expire samples, or apply contributions.
pub async fn load_due_target(
    index: &dyn TemporalIndex,
    namespace: TemporalNamespace,
    ready_head: ReadyHead,
    ticket: &FutureTicket,
) -> Result<DeadlineTarget, TemporalRuntimeError> {
    ready_head.confirm_pop(Some(ticket.due_time))?;
    ticket.validate()?;
    if ticket.id.input.namespace != namespace {
        return Err(TemporalStateError::NamespaceMismatch.into());
    }
    let input = match index.get(&TemporalKey::Input(ticket.id.input)).await? {
        None => return Ok(DeadlineTarget::Obsolete),
        Some(TemporalRecord::Input(input)) if input.id == ticket.id.input => input,
        Some(_) => return Err(TemporalRuntimeError::UnexpectedRecord("retained input")),
    };
    if !input.accepts(ticket) {
        return Ok(DeadlineTarget::Obsolete);
    }

    let mut cell = None;
    let mut group = None;
    if let Some(cell_id) = &ticket.cell {
        if !input.dependencies.contains(cell_id) {
            return Err(TemporalRuntimeError::MissingDependency(
                "input subscription",
            ));
        }
        let state = match index.get(&TemporalKey::Cell(cell_id.clone())).await? {
            Some(TemporalRecord::Cell(cell)) if cell.id == *cell_id => cell,
            None => return Err(TemporalRuntimeError::MissingDependency("function cell")),
            Some(_) => return Err(TemporalRuntimeError::UnexpectedRecord("function cell")),
        };
        if !state.subscribers.contains(&input.id) {
            return Err(TemporalRuntimeError::MissingDependency("cell subscription"));
        }
        if let StateOwner::Group(id) = cell_id.owner {
            let state = match index.get(&TemporalKey::Group(id)).await? {
                Some(TemporalRecord::Group(group)) if group.id == id => group,
                None => return Err(TemporalRuntimeError::MissingDependency("function group")),
                Some(_) => return Err(TemporalRuntimeError::UnexpectedRecord("function group")),
            };
            if !state.is_live() || !state.cells.contains(cell_id) {
                return Err(TemporalRuntimeError::MissingDependency(
                    "live function group",
                ));
            }
            group = Some(state);
        }
        match &state.state {
            FunctionState::TrueFor(state) => match &state.predicate {
                PredicateState::True { activation, .. } if *activation == ticket.activation => {}
                PredicateState::True { .. } | PredicateState::False => {
                    return Ok(DeadlineTarget::Obsolete);
                }
            },
            FunctionState::Aggregate
            | FunctionState::History(_)
            | FunctionState::UninitializedHistory => {
                return Err(TemporalRuntimeError::MissingDependency(
                    "deadline-capable function cell",
                ));
            }
            FunctionState::SlidingWindow(_) => {}
        }
        cell = Some(state);
    }
    Ok(DeadlineTarget::Ready(Box::new(ReadyInput {
        evaluation_clock: input.evaluated_at.at_deadline(ticket.due_time),
        input,
        cell,
        group,
    })))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use super::*;
    use crate::evaluation::temporal::runtime::due::{DueCutoff, DueHead};
    use crate::{
        evaluation::temporal::{
            codec, fixtures, FunctionCellId, Generation, GroupingKey, Incarnation, QueryEpoch,
            SourceRevision, TemporalBatch, TemporalGroupId, TrueForState,
        },
        interface::IndexError,
    };

    struct ReadOnlyIndex {
        records: BTreeMap<Vec<u8>, TemporalRecord>,
        reads: AtomicUsize,
    }

    impl ReadOnlyIndex {
        fn new(records: Vec<TemporalRecord>) -> Self {
            Self {
                records: records
                    .into_iter()
                    .map(|record| (codec::encode_key(&record.key()).unwrap(), record))
                    .collect(),
                reads: AtomicUsize::new(0),
            }
        }
    }

    fn due_head(ticket: &FutureTicket) -> ReadyHead {
        let DueHead::Ready(head) = DueCutoff::new(130).inspect(Some(ticket.due_time)) else {
            panic!("test ticket is not due");
        };
        head
    }

    #[async_trait]
    impl TemporalIndex for ReadOnlyIndex {
        async fn load_catalog(
            &self,
        ) -> Result<Option<crate::evaluation::temporal::TemporalCatalog>, IndexError> {
            panic!("deadline dispatch must use the opened query namespace");
        }

        async fn store_catalog(
            &self,
            _: crate::evaluation::temporal::TemporalCatalog,
        ) -> Result<(), IndexError> {
            panic!("deadline preparation must not mutate the query catalog");
        }

        async fn clear(&self) -> Result<(), IndexError> {
            panic!("deadline preparation must not clear query history");
        }

        async fn get(&self, key: &TemporalKey) -> Result<Option<TemporalRecord>, IndexError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let key = codec::encode_key(key).map_err(IndexError::other)?;
            Ok(self.records.get(&key).cloned())
        }

        async fn apply(&self, _: TemporalBatch) -> Result<(), IndexError> {
            panic!("deadline preparation must be read-only");
        }

        async fn clear_epoch(&self, _: QueryEpoch) -> Result<(), IndexError> {
            panic!("deadline preparation must not reset history");
        }
    }

    #[tokio::test]
    async fn an_advanced_source_clock_cannot_make_a_later_ticket_due() {
        let mut row = fixtures::input();
        let ticket = fixtures::ticket(&mut row);
        row.evaluated_at.realtime = 1_000;
        row.tickets.insert(ticket.id.clone(), ticket.clone());
        let index = ReadOnlyIndex::new(vec![TemporalRecord::Input(row.clone())]);
        let cutoff = DueCutoff::new(119);
        assert_eq!(
            cutoff.inspect(Some(ticket.due_time)),
            DueHead::NotDue { due_time: 120 }
        );
        let DueHead::Ready(older_head) = cutoff.inspect(Some(110)) else {
            panic!("older head should be due");
        };
        assert!(matches!(
            load_due_target(&index, row.id.namespace, older_head, &ticket).await,
            Err(TemporalRuntimeError::QueueHead(_))
        ));
        assert_eq!(index.reads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn internal_updates_leave_the_retained_ticket_live_and_preparation_read_only() {
        let mut row = fixtures::input();
        let ticket = fixtures::ticket(&mut row);
        row.tickets.insert(ticket.id.clone(), ticket.clone());
        row.source_revision = SourceRevision(5);
        row.evaluated_at.realtime = 125;
        let original = codec::encode_record(&TemporalRecord::Input(row.clone())).unwrap();
        let index = ReadOnlyIndex::new(vec![TemporalRecord::Input(row.clone())]);
        let DeadlineTarget::Ready(ready) =
            load_due_target(&index, row.id.namespace, due_head(&ticket), &ticket)
                .await
                .unwrap()
        else {
            panic!("lost pending input");
        };
        assert_eq!(ready.evaluation_clock.realtime, 125);
        assert_eq!(
            ready.evaluation_clock.transaction_time,
            row.evaluated_at.transaction_time
        );
        assert_eq!(
            codec::encode_record(&TemporalRecord::Input(ready.input)).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn retired_inputs_and_replaced_tickets_are_obsolete() {
        let mut row = fixtures::input();
        let ticket = fixtures::ticket(&mut row);
        let missing = ReadOnlyIndex::new(vec![]);
        let replaced = ReadOnlyIndex::new(vec![TemporalRecord::Input(row.clone())]);
        for index in [missing, replaced] {
            assert!(matches!(
                load_due_target(&index, row.id.namespace, due_head(&ticket), &ticket)
                    .await
                    .unwrap(),
                DeadlineTarget::Obsolete
            ));
        }
    }

    #[tokio::test]
    async fn shared_group_liveness_and_activation_do_not_depend_on_an_anchor() {
        let mut row = fixtures::input();
        let mut ticket = fixtures::ticket(&mut row);
        let group_id = TemporalGroupId {
            namespace: row.id.namespace,
            producer: row.id.part,
            incarnation: Incarnation(8),
        };
        let cell_id = FunctionCellId {
            owner: StateOwner::Group(group_id),
            call: ticket.id.call.clone(),
        };
        ticket.cell = Some(cell_id.clone());
        row.dependencies.insert(cell_id.clone());
        row.tickets.insert(ticket.id.clone(), ticket.clone());
        let group = GroupState {
            id: group_id,
            grouping: GroupingKey(vec![]),
            members: BTreeSet::from([row.id]),
            default_row: false,
            cells: BTreeSet::from([cell_id.clone()]),
            last_delivered: crate::evaluation::temporal::Presence::Absent,
        };
        let cell = FunctionCell {
            id: cell_id,
            state: FunctionState::TrueFor(TrueForState {
                predicate: PredicateState::True {
                    since: 110,
                    activation: Generation(0),
                },
                next_activation: Generation(1),
            }),
            subscribers: BTreeSet::from([row.id]),
        };
        let records = vec![
            TemporalRecord::Input(row.clone()),
            TemporalRecord::Group(group),
            TemporalRecord::Cell(cell.clone()),
        ];
        let index = ReadOnlyIndex::new(records.clone());
        assert!(matches!(
            load_due_target(&index, row.id.namespace, due_head(&ticket), &ticket)
                .await
                .unwrap(),
            DeadlineTarget::Ready(_)
        ));
        let mut records = records;
        let mut changed = cell;
        changed.state = FunctionState::TrueFor(TrueForState {
            predicate: PredicateState::True {
                since: 125,
                activation: Generation(1),
            },
            next_activation: Generation(2),
        });
        records[2] = TemporalRecord::Cell(changed);
        let index = ReadOnlyIndex::new(records);
        assert!(matches!(
            load_due_target(&index, row.id.namespace, due_head(&ticket), &ticket)
                .await
                .unwrap(),
            DeadlineTarget::Obsolete
        ));
    }

    #[tokio::test]
    async fn a_live_ticket_with_missing_dependency_is_corrupt_not_obsolete() {
        let mut row = fixtures::input();
        let mut ticket = fixtures::ticket(&mut row);
        let cell = FunctionCellId {
            owner: StateOwner::Input(row.id),
            call: ticket.id.call.clone(),
        };
        ticket.cell = Some(cell.clone());
        row.dependencies.insert(cell);
        row.tickets.insert(ticket.id.clone(), ticket.clone());
        let index = ReadOnlyIndex::new(vec![TemporalRecord::Input(row.clone())]);
        assert!(matches!(
            load_due_target(&index, row.id.namespace, due_head(&ticket), &ticket).await,
            Err(TemporalRuntimeError::MissingDependency("function cell"))
        ));
    }
}
