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

//! Retained input settlement under a caller-owned, already-dirty query root.

use crate::evaluation::QueryExecutionError;

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use drasi_query_ast::ast::{
    Expression, FunctionExpression, Literal, ParentExpression, UnaryExpression,
};
use hashers::jenkins::spooky_hash::SpookyHasher;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables, SideEffects},
        functions::{FunctionEffect, FunctionRegistry},
        temporal::*,
        variable_value::VariableValue,
        EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator,
    },
    interface::{FutureQueue, IndexError, PushType, TemporalIndex},
};

use super::{
    deadlines::{load_due_target, DeadlineTarget},
    due::{DueCutoff, DueHead},
    frame::{EvaluationFrame, TemporalEvaluation},
    functions::{settle_function, RequestedTicket},
    program::{ExpressionProgram, PartProgram, TemporalProgram},
};

#[derive(Debug, Clone)]
pub struct TemporalInputChange {
    pub origin: InputOrigin,
    pub before: Option<QueryVariables>,
    pub after: Option<QueryVariables>,
    pub context: SavedContext,
    pub row_signature: u64,
}

pub struct TemporalRuntime {
    program: TemporalProgram,
    namespace: TemporalNamespace,
    index: Arc<dyn TemporalIndex>,
    evaluator: Arc<ExpressionEvaluator>,
    future_queue: Arc<dyn FutureQueue>,
    registry: Arc<FunctionRegistry>,
}

struct WorkRow {
    old: Option<RetainedInput>,
    input: RetainedInput,
    removed: bool,
    dirty: bool,
    eligible: bool,
    old_group: Option<TemporalGroupId>,
    group: Option<TemporalGroupId>,
    frame: Arc<Mutex<EvaluationFrame>>,
    predicate: Vec<AggregateContribution>,
    projection: Vec<AggregateContribution>,
    dependencies: BTreeSet<FunctionCellId>,
    visited: BTreeSet<FunctionCall>,
    snapshot_calls: BTreeSet<FunctionCall>,
    requests: BTreeMap<(FunctionCall, u32), RequestedTicket>,
}

impl WorkRow {
    fn new(input: RetainedInput, old: Option<RetainedInput>, dirty: bool) -> Self {
        let values = input
            .values
            .iter()
            .map(|(call, value)| (call.clone(), value.value.clone()))
            .collect();
        Self {
            old,
            input,
            removed: false,
            dirty,
            eligible: true,
            old_group: None,
            group: None,
            frame: Arc::new(Mutex::new(EvaluationFrame {
                values,
                ..EvaluationFrame::default()
            })),
            predicate: Vec::new(),
            projection: Vec::new(),
            dependencies: BTreeSet::new(),
            visited: BTreeSet::new(),
            snapshot_calls: BTreeSet::new(),
            requests: BTreeMap::new(),
        }
    }
}

struct PartWork {
    rows: BTreeMap<TemporalInputId, WorkRow>,
    groups: BTreeMap<TemporalGroupId, GroupState>,
    cells: BTreeMap<FunctionCellId, FunctionCell>,
    initial_defaults: BTreeMap<TemporalGroupId, DeliveredRow>,
}

#[derive(Clone, Copy)]
struct EvaluationEvent {
    revision: Option<SourceRevision>,
    retained_revision: SourceRevision,
    transaction_time: Option<u64>,
    realtime: u64,
}

struct PartResult {
    changes: Vec<TemporalInputChange>,
    output: Vec<QueryPartEvaluationContext>,
    changed: bool,
}

impl TemporalRuntime {
    pub fn new(
        program: TemporalProgram,
        namespace: TemporalNamespace,
        index: Arc<dyn TemporalIndex>,
        evaluator: Arc<ExpressionEvaluator>,
        future_queue: Arc<dyn FutureQueue>,
        registry: Arc<FunctionRegistry>,
    ) -> Self {
        Self {
            program,
            namespace,
            index,
            evaluator,
            future_queue,
            registry,
        }
    }

    pub async fn process_changes(
        &self,
        changes: Vec<TemporalInputChange>,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let first = self
            .program
            .parts
            .first()
            .ok_or(EvaluationError::InvalidContext)?;
        let changes = coalesce_changes(changes, self.namespace, first.id)?;
        let mut effective = Vec::new();
        for change in changes {
            if !optional_variables_equal(change.before.as_ref(), change.after.as_ref())? {
                effective.push(change);
            }
        }
        if effective.is_empty() {
            return Ok(Vec::new());
        }
        let (mut catalog, mut epoch) = self.load_epoch().await?;
        let revision = catalog.next_revision;
        catalog.next_revision = SourceRevision(
            revision
                .0
                .checked_add(1)
                .ok_or(EvaluationError::OverflowError)?,
        );
        let event = EvaluationEvent {
            revision: Some(revision),
            retained_revision: revision,
            transaction_time: effective
                .iter()
                .map(|change| change.context.clock.transaction_time)
                .max(),
            realtime: effective
                .iter()
                .map(|change| change.context.clock.realtime)
                .max()
                .ok_or(EvaluationError::InvalidContext)?,
        };
        let (output, changed) = self
            .run_parts(0, effective, BTreeSet::new(), event, &mut epoch)
            .await?;
        if changed {
            self.persist_epoch(epoch).await?;
            self.index.store_catalog(catalog).await?;
        }
        Ok(output)
    }

    pub async fn process_ticket(
        &self,
        ticket: FutureTicket,
        cutoff: u64,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        let DueHead::Ready(head) = DueCutoff::new(cutoff).inspect(Some(ticket.due_time)) else {
            return Ok(Vec::new());
        };
        let target = load_due_target(self.index.as_ref(), self.namespace, head, &ticket)
            .await
            .map_err(index_error)?;
        let DeadlineTarget::Ready(target) = target else {
            return Ok(Vec::new());
        };
        let (_, mut epoch) = self.load_epoch().await?;
        let part = self
            .program
            .parts
            .iter()
            .position(|part| part.id == target.input.id.part)
            .ok_or(EvaluationError::CorruptData)?;
        let mut refresh = BTreeSet::from([target.input.id]);
        if let Some(cell) = target.cell {
            refresh.extend(cell.subscribers);
        }
        let (output, changed) = self
            .run_parts(
                part,
                Vec::new(),
                refresh,
                EvaluationEvent {
                    revision: None,
                    retained_revision: target.input.source_revision,
                    transaction_time: None,
                    realtime: ticket.due_time,
                },
                &mut epoch,
            )
            .await?;
        if changed {
            self.persist_epoch(epoch).await?;
        }
        Ok(output)
    }

    async fn load_epoch(&self) -> Result<(TemporalCatalog, EpochState), EvaluationError> {
        let catalog = self
            .index
            .load_catalog()
            .await?
            .ok_or_else(|| index_error(codec::TemporalCodecError::MigrationRequired))?;
        if catalog.namespace != self.namespace {
            return Err(index_error(TemporalStateError::NamespaceMismatch));
        }
        let epoch = match self.index.get(&TemporalKey::Epoch(self.namespace)).await? {
            Some(TemporalRecord::Epoch(epoch)) if epoch.namespace == self.namespace => epoch,
            _ => return Err(EvaluationError::CorruptData),
        };
        Ok((catalog, epoch))
    }

    async fn persist_epoch(&self, epoch: EpochState) -> Result<(), EvaluationError> {
        let mut batch = TemporalBatch::new(self.namespace);
        batch
            .put(TemporalRecord::Epoch(epoch))
            .map_err(index_error)?;
        self.index.apply(batch).await?;
        Ok(())
    }

    async fn run_parts(
        &self,
        first: usize,
        mut changes: Vec<TemporalInputChange>,
        mut refresh: BTreeSet<TemporalInputId>,
        event: EvaluationEvent,
        epoch: &mut EpochState,
    ) -> Result<(Vec<QueryPartEvaluationContext>, bool), EvaluationError> {
        let mut changed = false;
        let mut output = Vec::new();
        for (ordinal, part) in self.program.parts.iter().enumerate().skip(first) {
            if changes.is_empty() && refresh.is_empty() {
                break;
            }
            let result = self
                .process_part(
                    part,
                    changes,
                    std::mem::take(&mut refresh),
                    event,
                    epoch,
                    ordinal > first || event.revision.is_none(),
                )
                .await?;
            changes = result.changes;
            changed |= result.changed;
            if ordinal + 1 == self.program.parts.len() {
                output = result.output;
            }
        }
        Ok((output, changed))
    }

    async fn process_part(
        &self,
        part: &PartProgram,
        changes: Vec<TemporalInputChange>,
        refresh: BTreeSet<TemporalInputId>,
        event: EvaluationEvent,
        epoch: &mut EpochState,
        force: bool,
    ) -> Result<PartResult, EvaluationError> {
        let mut work = self.load_part(part).await?;
        if work
            .rows
            .keys()
            .any(|id| id.incarnation >= epoch.next_incarnation)
        {
            return Err(EvaluationError::CorruptData);
        }
        self.merge_changes(part, &mut work, changes, event, epoch, force)
            .await?;
        for id in refresh {
            let row = work.rows.get_mut(&id).ok_or(EvaluationError::CorruptData)?;
            row.dirty = true;
            touch(row, event);
        }
        if !work.rows.values().any(|row| row.dirty) {
            return Ok(PartResult {
                changes: Vec::new(),
                output: Vec::new(),
                changed: false,
            });
        }
        self.prepare_groups(part, &mut work, event, epoch).await?;
        self.load_cells(&mut work).await?;
        self.capture_initial_defaults(part, &mut work).await?;
        for expression in &part.predicates {
            self.settle_expression(part, expression, true, &mut work, event)
                .await?;
            for row in work
                .rows
                .values_mut()
                .filter(|row| row.dirty && row.eligible)
            {
                let context = evaluation_context(part, row, false);
                row.eligible = self
                    .evaluator
                    .evaluate_predicate(&context, &expression.expression)
                    .await?;
            }
        }
        for expression in &part.projection {
            self.settle_expression(part, expression, false, &mut work, event)
                .await?;
        }
        for row in work.rows.values_mut().filter(|row| row.dirty) {
            row.input.applied.last_delivered = if row.eligible && !row.removed {
                Presence::Real(self.project(part, row, false).await?)
            } else {
                Presence::Absent
            };
        }
        let result = self.collect_output(part, &mut work, event).await?;
        self.persist_part(part, work).await?;
        Ok(result)
    }

    async fn load_part(&self, part: &PartProgram) -> Result<PartWork, EvaluationError> {
        let key = TemporalKey::Part {
            namespace: self.namespace,
            part: part.id,
        };
        let inputs = match self.index.get(&key).await? {
            None => BTreeSet::new(),
            Some(TemporalRecord::Part(state))
                if state.namespace == self.namespace && state.part == part.id =>
            {
                state.inputs
            }
            Some(_) => return Err(EvaluationError::CorruptData),
        };
        let mut rows = BTreeMap::new();
        for id in inputs {
            let input = match self.index.get(&TemporalKey::Input(id)).await? {
                Some(TemporalRecord::Input(input)) if input.id == id => input,
                _ => return Err(EvaluationError::CorruptData),
            };
            match self
                .index
                .get(&TemporalKey::Origin(input.origin_key()))
                .await?
            {
                Some(TemporalRecord::Origin { input: found, .. }) if found == id => {}
                _ => return Err(EvaluationError::CorruptData),
            }
            rows.insert(id, WorkRow::new(input.clone(), Some(input), false));
        }
        Ok(PartWork {
            rows,
            groups: BTreeMap::new(),
            cells: BTreeMap::new(),
            initial_defaults: BTreeMap::new(),
        })
    }

    async fn merge_changes(
        &self,
        part: &PartProgram,
        work: &mut PartWork,
        changes: Vec<TemporalInputChange>,
        event: EvaluationEvent,
        epoch: &mut EpochState,
        force: bool,
    ) -> Result<(), EvaluationError> {
        let mut origins = BTreeMap::new();
        for row in work.rows.values() {
            origins.insert(
                codec::encode_key(&TemporalKey::Origin(row.input.origin_key()))
                    .map_err(index_error)?,
                row.input.id,
            );
        }
        for change in changes {
            let key = OriginKey {
                namespace: self.namespace,
                part: part.id,
                origin: change.origin.clone(),
            };
            let encoded =
                codec::encode_key(&TemporalKey::Origin(key.clone())).map_err(index_error)?;
            if let Some(id) = origins.get(&encoded) {
                let row = work.rows.get_mut(id).ok_or(EvaluationError::CorruptData)?;
                let differs = match &change.after {
                    Some(after) => row.removed || !variables_equal(&row.input.variables, after)?,
                    None => !row.removed,
                };
                if !differs && !force {
                    continue;
                }
                row.dirty = true;
                row.removed = change.after.is_none();
                row.eligible = !row.removed;
                if let Some(after) = change.after {
                    row.input.variables = after;
                    row.input.context = change.context.clone();
                    if event.revision.is_some() {
                        row.input.source_clock = change.context.clock;
                    }
                    row.input.row_signature = change.row_signature;
                }
                touch(row, event);
            } else {
                if change.before.is_some() || change.after.is_none() {
                    return Err(EvaluationError::CorruptData);
                }
                if self.index.get(&TemporalKey::Origin(key)).await?.is_some() {
                    return Err(EvaluationError::CorruptData);
                }
                let id = TemporalInputId {
                    namespace: self.namespace,
                    part: part.id,
                    incarnation: epoch.allocate().map_err(index_error)?,
                };
                let input = RetainedInput {
                    id,
                    origin: change.origin,
                    variables: change.after.ok_or(EvaluationError::CorruptData)?,
                    source_revision: event.retained_revision,
                    source_clock: change.context.clock,
                    evaluated_at: change.context.clock,
                    context: change.context,
                    row_signature: change.row_signature,
                    applied: AppliedContribution::default(),
                    dependencies: BTreeSet::new(),
                    tickets: BTreeMap::new(),
                    history: BTreeMap::new(),
                    values: BTreeMap::new(),
                    next_ticket_generation: Generation(0),
                };
                let mut row = WorkRow::new(input, None, true);
                touch(&mut row, event);
                work.rows.insert(id, row);
                origins.insert(encoded, id);
            }
        }
        Ok(())
    }

    async fn grouping(
        &self,
        part: &PartProgram,
        input: &RetainedInput,
    ) -> Result<GroupingKey, EvaluationError> {
        let mut context = ExpressionEvaluationContext::from_saved(&input.variables, &input.context);
        context.set_side_effects(SideEffects::Snapshot);
        let mut values = Vec::new();
        for expression in &part.grouping {
            values.push(
                self.evaluator
                    .evaluate_expression(&context, expression)
                    .await?,
            );
        }
        Ok(GroupingKey(values))
    }

    async fn prepare_groups(
        &self,
        part: &PartProgram,
        work: &mut PartWork,
        event: EvaluationEvent,
        epoch: &mut EpochState,
    ) -> Result<(), EvaluationError> {
        let aggregates = part
            .predicates
            .iter()
            .chain(&part.projection)
            .any(|program| {
                program
                    .effects
                    .iter()
                    .any(|effect| self.effect(effect) == FunctionEffect::Aggregate)
            });
        if !part.grouped && !aggregates {
            return Ok(());
        }
        let mut by_key = BTreeMap::new();
        for row in work.rows.values_mut() {
            if let Some(old) = &row.old {
                let grouping = self.grouping(part, old).await?;
                row.old_group = Some(
                    self.ensure_group(part, grouping, &mut work.groups, &mut by_key, epoch, false)
                        .await?,
                );
            }
            if !row.removed {
                let grouping = self.grouping(part, &row.input).await?;
                row.group = Some(
                    self.ensure_group(part, grouping, &mut work.groups, &mut by_key, epoch, true)
                        .await?,
                );
            }
        }
        let affected: BTreeSet<_> = work
            .rows
            .values()
            .filter(|row| row.dirty)
            .flat_map(|row| row.old_group.into_iter().chain(row.group))
            .collect();
        let reads_aggregate_in_predicate = part.predicates.iter().any(|program| {
            program
                .effects
                .iter()
                .any(|effect| self.effect(effect) == FunctionEffect::Aggregate)
        });
        let temporal_reads_aggregate = part.projection.iter().any(|program| {
            program.effects.iter().any(|effect| {
                self.effect(effect).is_temporal()
                    && expression_dependencies(
                        &Expression::FunctionExpression(effect.clone()),
                        &self.registry,
                    )
                    .0
            })
        });
        for row in work.rows.values_mut() {
            if aggregates
                && row.input.dependencies.iter().any(|dependency| {
                    matches!(dependency.owner, StateOwner::Group(group) if affected.contains(&group))
                })
            {
                row.dirty = true;
                if reads_aggregate_in_predicate || temporal_reads_aggregate {
                    touch(row, event);
                }
            }
        }
        for group in work.groups.values_mut() {
            let expected: BTreeSet<_> = work
                .rows
                .values()
                .filter(|row| row.old_group == Some(group.id))
                .map(|row| row.input.id)
                .collect();
            if group.members != expected {
                return Err(EvaluationError::CorruptData);
            }
            group.members.clear();
        }
        for row in work.rows.values().filter(|row| !row.removed) {
            if let Some(id) = row.group {
                work.groups
                    .get_mut(&id)
                    .ok_or(EvaluationError::CorruptData)?
                    .members
                    .insert(row.input.id);
            }
        }
        Ok(())
    }

    async fn ensure_group(
        &self,
        part: &PartProgram,
        grouping: GroupingKey,
        groups: &mut BTreeMap<TemporalGroupId, GroupState>,
        by_key: &mut BTreeMap<Vec<u8>, TemporalGroupId>,
        epoch: &mut EpochState,
        create: bool,
    ) -> Result<TemporalGroupId, EvaluationError> {
        let key = GroupOrigin {
            namespace: self.namespace,
            producer: part.id,
            grouping,
        };
        let encoded =
            codec::encode_key(&TemporalKey::GroupOrigin(key.clone())).map_err(index_error)?;
        if let Some(id) = by_key.get(&encoded) {
            return Ok(*id);
        }
        let group = match self
            .index
            .get(&TemporalKey::GroupOrigin(key.clone()))
            .await?
        {
            Some(TemporalRecord::GroupOrigin { group: id, .. }) => {
                match self.index.get(&TemporalKey::Group(id)).await? {
                    Some(TemporalRecord::Group(group)) if group.id == id && group.is_live() => {
                        group
                    }
                    _ => return Err(EvaluationError::CorruptData),
                }
            }
            None if create => GroupState {
                id: TemporalGroupId {
                    namespace: self.namespace,
                    producer: part.id,
                    incarnation: epoch.allocate().map_err(index_error)?,
                },
                grouping: key.grouping,
                members: BTreeSet::new(),
                default_row: false,
                cells: BTreeSet::new(),
                last_delivered: Presence::Absent,
            },
            _ => return Err(EvaluationError::CorruptData),
        };
        let id = group.id;
        if codec::encode_key(&TemporalKey::GroupOrigin(group.origin())).map_err(index_error)?
            != encoded
        {
            return Err(EvaluationError::CorruptData);
        }
        by_key.insert(encoded, id);
        groups.insert(id, group);
        Ok(id)
    }

    async fn load_cells(&self, work: &mut PartWork) -> Result<(), EvaluationError> {
        let ids: BTreeSet<_> = work
            .rows
            .values()
            .flat_map(|row| row.input.dependencies.iter().cloned())
            .chain(
                work.groups
                    .values()
                    .flat_map(|group| group.cells.iter().cloned()),
            )
            .collect();
        for id in ids {
            let cell = match self.index.get(&TemporalKey::Cell(id.clone())).await? {
                Some(TemporalRecord::Cell(cell)) if cell.id == id => cell,
                _ => return Err(EvaluationError::CorruptData),
            };
            work.cells.insert(id, cell);
        }
        for row in work.rows.values() {
            for id in &row.input.dependencies {
                if id.call.site.part != row.input.id.part
                    || matches!(id.owner, StateOwner::Group(group) if row.old_group != Some(group))
                {
                    return Err(EvaluationError::CorruptData);
                }
                if !work
                    .cells
                    .get(id)
                    .is_some_and(|cell| cell.subscribers.contains(&row.input.id))
                {
                    return Err(EvaluationError::CorruptData);
                }
            }
            for cell in work.cells.values() {
                for id in &cell.subscribers {
                    if !work
                        .rows
                        .get(id)
                        .is_some_and(|row| row.input.dependencies.contains(&cell.id))
                    {
                        return Err(EvaluationError::CorruptData);
                    }
                }
            }
        }
        Ok(())
    }

    async fn capture_initial_defaults(
        &self,
        part: &PartProgram,
        work: &mut PartWork,
    ) -> Result<(), EvaluationError> {
        if !part.grouped
            || part.projection.iter().any(|expression| {
                expression.effects.iter().any(|effect| {
                    let effect = self.effect(effect);
                    effect.is_temporal() && effect != FunctionEffect::SlidingWindow
                })
            })
        {
            return Ok(());
        }
        for row in work
            .rows
            .values()
            .filter(|row| row.dirty && !row.removed && row.old.is_none())
        {
            let id = row.group.ok_or(EvaluationError::CorruptData)?;
            if matches!(work.groups[&id].last_delivered, Presence::Absent)
                && !work.initial_defaults.contains_key(&id)
            {
                work.initial_defaults
                    .insert(id, self.project(part, row, true).await?);
            }
        }
        Ok(())
    }

    fn effect(&self, expression: &FunctionExpression) -> FunctionEffect {
        self.registry
            .get_function(&expression.name)
            .map_or(FunctionEffect::Pure, |function| function.effect())
    }

    async fn settle_expression(
        &self,
        part: &PartProgram,
        program: &ExpressionProgram,
        predicate: bool,
        work: &mut PartWork,
        event: EvaluationEvent,
    ) -> Result<(), EvaluationError> {
        for expression in &program.effects {
            let site = FunctionSite {
                part: part.id,
                position_in_query: expression.position_in_query,
            };
            for row in work.rows.values_mut().filter(|row| row.dirty) {
                row.snapshot_calls.clear();
                {
                    let mut frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
                    frame.target = Some(site.clone());
                    frame.captured.clear();
                }
                if row.eligible && !row.removed {
                    let context = evaluation_context(part, row, false);
                    match self
                        .evaluator
                        .evaluate_expression(&context, &program.expression)
                        .await
                    {
                        Ok(_) => {}
                        Err(error)
                            if error.execution_error()
                                == Some(&QueryExecutionError::TemporalDeferred) => {}
                        Err(error) => return Err(error),
                    }
                    if self.effect(expression) == FunctionEffect::Aggregate {
                        self.capture_snapshot_subscriptions(part, program, row)
                            .await?;
                    }
                }
            }
            if self.effect(expression) == FunctionEffect::Aggregate {
                self.settle_aggregate(expression, &site, predicate, work)
                    .await?;
            } else {
                self.settle_temporal(work, event)?;
            }
            for row in work.rows.values_mut().filter(|row| row.dirty) {
                let mut frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
                frame.settled.insert(site.clone());
                frame.target = None;
            }
        }
        Ok(())
    }

    async fn capture_snapshot_subscriptions(
        &self,
        part: &PartProgram,
        program: &ExpressionProgram,
        row: &mut WorkRow,
    ) -> Result<(), EvaluationError> {
        let windows: BTreeSet<_> = program
            .effects
            .iter()
            .filter(|expression| self.effect(expression) == FunctionEffect::SlidingWindow)
            .map(|expression| FunctionSite {
                part: part.id,
                position_in_query: expression.position_in_query,
            })
            .collect();
        let (expired, captured) = {
            let mut frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
            let expired: Vec<_> = frame
                .values
                .iter()
                .filter(|(call, value)| {
                    windows.contains(&call.site) && **value == VariableValue::Bool(true)
                })
                .map(|(call, _)| call.clone())
                .collect();
            if expired.is_empty() {
                return Ok(());
            }
            for call in &expired {
                frame
                    .values
                    .insert(call.clone(), VariableValue::Bool(false));
            }
            (expired, std::mem::take(&mut frame.captured))
        };
        // An expired window still reads its aggregate. Capture that read separately from the
        // desired receipt, so expiry retracts once without losing the subscription.
        let context = evaluation_context(part, row, false);
        let result = self
            .evaluator
            .evaluate_expression(&context, &program.expression)
            .await;
        {
            let mut frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
            row.snapshot_calls = frame
                .captured
                .keys()
                .filter(|call| !captured.contains_key(*call))
                .cloned()
                .collect();
            frame.captured = captured;
            for call in expired {
                frame.values.insert(call, VariableValue::Bool(true));
            }
        }
        match result {
            Ok(_) => Ok(()),
            Err(error)
                if error.execution_error() == Some(&QueryExecutionError::TemporalDeferred) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn settle_aggregate(
        &self,
        expression: &FunctionExpression,
        site: &FunctionSite,
        predicate: bool,
        work: &mut PartWork,
    ) -> Result<(), EvaluationError> {
        let mut retract = Vec::new();
        let mut apply = Vec::new();
        for row in work.rows.values_mut().filter(|row| row.dirty) {
            let captures = row
                .frame
                .lock()
                .map_err(|_| EvaluationError::CorruptData)?
                .captured
                .clone();
            let mut desired: BTreeMap<_, _> = captures
                .values()
                .map(|capture| {
                    (
                        capture.call.clone(),
                        AggregateContribution {
                            call: capture.call.clone(),
                            owner: ContributionOwner::Function(expression.position_in_query),
                            key: capture.key.clone(),
                            arguments: capture.arguments.clone(),
                            context: capture.context.clone(),
                        },
                    )
                })
                .collect();
            let old: BTreeMap<_, _> = row.old.as_ref().map_or_else(BTreeMap::new, |old| {
                let receipts = if predicate {
                    &old.applied.predicate
                } else {
                    &old.applied.projection
                };
                receipts
                    .iter()
                    .filter(|receipt| &receipt.call.site == site)
                    .map(|receipt| (receipt.call.clone(), receipt.clone()))
                    .collect()
            });
            for (call, receipt) in &old {
                let unchanged = match desired.get(call) {
                    Some(desired) => same_receipt(receipt, desired)?,
                    None => false,
                };
                if !unchanged {
                    retract.push((
                        receipt.clone(),
                        if row.removed {
                            SideEffects::RevertForDelete
                        } else {
                            SideEffects::RevertForUpdate
                        },
                    ));
                }
            }
            for (call, receipt) in &mut desired {
                let unchanged = match old.get(call) {
                    Some(old) => same_receipt(old, receipt)?,
                    None => false,
                };
                if !unchanged {
                    apply.push(receipt.clone());
                } else if let Some(old) = old.get(call) {
                    *receipt = old.clone();
                }
                let id = FunctionCellId {
                    owner: StateOwner::Group(row.group.ok_or(EvaluationError::CorruptData)?),
                    call: call.clone(),
                };
                subscribe(
                    row,
                    id,
                    FunctionState::Aggregate,
                    &mut work.cells,
                    &mut work.groups,
                )?;
                row.visited.insert(call.clone());
            }
            for call in std::mem::take(&mut row.snapshot_calls) {
                let id = FunctionCellId {
                    owner: StateOwner::Group(row.group.ok_or(EvaluationError::CorruptData)?),
                    call: call.clone(),
                };
                subscribe(
                    row,
                    id,
                    FunctionState::Aggregate,
                    &mut work.cells,
                    &mut work.groups,
                )?;
                row.visited.insert(call);
            }
            if predicate {
                row.predicate.extend(desired.into_values());
            } else {
                row.projection.extend(desired.into_values());
            }
        }
        for (receipt, directive) in retract {
            self.evaluator
                .change_contribution(expression, &receipt, directive)
                .await?;
        }
        for receipt in apply {
            self.evaluator
                .change_contribution(expression, &receipt, SideEffects::Apply)
                .await?;
        }
        Ok(())
    }

    fn settle_temporal(
        &self,
        work: &mut PartWork,
        event: EvaluationEvent,
    ) -> Result<(), EvaluationError> {
        for row in work.rows.values_mut().filter(|row| row.dirty) {
            let captures = row
                .frame
                .lock()
                .map_err(|_| EvaluationError::CorruptData)?
                .captured
                .clone();
            for capture in captures.into_values() {
                if !capture.effect.is_temporal() {
                    return Err(EvaluationError::CorruptData);
                }
                let state = match capture.effect {
                    FunctionEffect::TrueFor => {
                        Some(FunctionState::TrueFor(TrueForState::default()))
                    }
                    FunctionEffect::PreviousValue | FunctionEffect::PreviousDistinctValue => {
                        Some(FunctionState::UninitializedHistory)
                    }
                    _ => None,
                };
                let id = if let Some(state) = state {
                    let (aggregate, free) = expression_dependencies(
                        &Expression::FunctionExpression(capture.expression.clone()),
                        &self.registry,
                    );
                    let owner = if aggregate && !free {
                        StateOwner::Group(row.group.ok_or(EvaluationError::CorruptData)?)
                    } else {
                        StateOwner::Input(row.input.id)
                    };
                    let id = FunctionCellId {
                        owner,
                        call: capture.call.clone(),
                    };
                    subscribe(row, id.clone(), state, &mut work.cells, &mut work.groups)?;
                    Some(id)
                } else {
                    None
                };
                let cell = id.as_ref().and_then(|id| work.cells.get_mut(id));
                let settled =
                    settle_function(&capture, &mut row.input, cell, event.revision.is_some())?;
                row.frame
                    .lock()
                    .map_err(|_| EvaluationError::CorruptData)?
                    .values
                    .insert(capture.call.clone(), settled.value);
                row.visited.insert(capture.call);
                for request in settled.tickets {
                    if row
                        .requests
                        .insert((request.call.clone(), request.slot), request)
                        .is_some()
                    {
                        return Err(EvaluationError::CorruptData);
                    }
                }
            }
        }
        Ok(())
    }

    async fn project(
        &self,
        part: &PartProgram,
        row: &WorkRow,
        snapshot: bool,
    ) -> Result<DeliveredRow, EvaluationError> {
        let context = evaluation_context(part, row, snapshot);
        let mut variables = QueryVariables::new();
        for expression in &part.projection {
            let (name, value) = self
                .evaluator
                .evaluate_projection_field(&context, &expression.expression)
                .await?;
            variables.insert(name.into(), value);
        }
        Ok(DeliveredRow {
            variables,
            row_signature: row.input.row_signature,
        })
    }

    async fn collect_output(
        &self,
        part: &PartProgram,
        work: &mut PartWork,
        event: EvaluationEvent,
    ) -> Result<PartResult, EvaluationError> {
        let mut result = PartResult {
            changes: Vec::new(),
            output: Vec::new(),
            changed: true,
        };
        if !part.grouped {
            for row in work.rows.values().filter(|row| row.dirty) {
                let before = row
                    .old
                    .as_ref()
                    .and_then(|old| delivered(&old.applied.last_delivered));
                let after = delivered(&row.input.applied.last_delivered);
                let origin = InputOrigin::Derived {
                    input: row.input.id,
                    ordinal: 0,
                };
                let context = output_context(&row.input.context, event);
                push_change(&mut result.changes, origin, before, after, &context);
                let equal = optional_variables_equal(
                    before.map(|row| &row.variables),
                    after.map(|row| &row.variables),
                )?;
                if equal {
                    continue;
                }
                result.output.push(match (before, after) {
                    (None, Some(after)) => QueryPartEvaluationContext::Adding {
                        after: after.variables.clone(),
                        row_signature: after.row_signature,
                    },
                    (Some(before), None) => QueryPartEvaluationContext::Removing {
                        before: before.variables.clone(),
                        row_signature: before.row_signature,
                    },
                    (Some(before), Some(after)) => QueryPartEvaluationContext::Updating {
                        before: before.variables.clone(),
                        after: after.variables.clone(),
                        row_signature: after.row_signature,
                    },
                    (None, None) => continue,
                });
            }
            return Ok(result);
        }
        let affected: BTreeSet<_> = work
            .rows
            .values()
            .filter(|row| row.dirty)
            .flat_map(|row| row.old_group.into_iter().chain(row.group))
            .collect();
        let mut grouping_keys = Vec::new();
        for expression in &part.grouping {
            let representative = work
                .rows
                .values()
                .next()
                .ok_or(EvaluationError::CorruptData)?;
            let context = evaluation_context(part, representative, true);
            grouping_keys.push(
                self.evaluator
                    .evaluate_projection_field(&context, expression)
                    .await?
                    .0,
            );
        }
        // A default snapshot is visible output, not an input to another aggregation.
        let downstream_aggregates = self
            .program
            .parts
            .windows(2)
            .any(|parts| parts[0].id == part.id && parts[1].grouped);
        for id in affected {
            let group = work.groups.get(&id).ok_or(EvaluationError::CorruptData)?;
            let before = delivered(&group.last_delivered).cloned();
            let real = work.rows.values().find(|row| {
                row.group == Some(id)
                    && !row.removed
                    && delivered(&row.input.applied.last_delivered).is_some()
            });
            let representative = real
                .or_else(|| {
                    work.rows.values().find(|row| {
                        row.old_group == Some(id)
                            && row
                                .old
                                .as_ref()
                                .and_then(|old| delivered(&old.applied.last_delivered))
                                .is_some()
                    })
                })
                .or_else(|| {
                    work.rows
                        .values()
                        .find(|row| row.old_group == Some(id) || row.group == Some(id))
                })
                .ok_or(EvaluationError::CorruptData)?;
            let context = output_context(&representative.input.context, event);
            let mut after = if let Some(row) = real {
                delivered(&row.input.applied.last_delivered).cloned()
            } else if matches!(group.last_delivered, Presence::Default(_)) {
                before.clone()
            } else if before.is_some() {
                let saved = if representative.old_group == Some(id) {
                    representative
                        .old
                        .as_ref()
                        .ok_or(EvaluationError::CorruptData)?
                } else {
                    &representative.input
                };
                let snapshot = snapshot_row(saved, part)?;
                Some(self.project(part, &snapshot, true).await?)
            } else {
                None
            };
            let signature = group_signature(&group.grouping);
            if let Some(after) = &mut after {
                after.row_signature = signature;
            }
            let added = work.rows.values().any(|row| {
                row.dirty
                    && row.group == Some(id)
                    && delivered(&row.input.applied.last_delivered).is_some()
                    && (row.old_group != Some(id)
                        || row
                            .old
                            .as_ref()
                            .and_then(|old| delivered(&old.applied.last_delivered))
                            .is_none())
            });
            let removed = work.rows.values().any(|row| {
                row.dirty
                    && row.old_group == Some(id)
                    && row
                        .old
                        .as_ref()
                        .and_then(|old| delivered(&old.applied.last_delivered))
                        .is_some()
                    && (row.group != Some(id)
                        || delivered(&row.input.applied.last_delivered).is_none())
            });
            let is_default = real.is_none() && after.is_some();
            push_change(
                &mut result.changes,
                InputOrigin::Group(id),
                before
                    .as_ref()
                    .filter(|_| !downstream_aggregates || !group.default_row),
                after
                    .as_ref()
                    .filter(|_| !downstream_aggregates || !is_default),
                &context,
            );
            if !optional_variables_equal(
                before.as_ref().map(|row| &row.variables),
                after.as_ref().map(|row| &row.variables),
            )? {
                if let Some(after) = &after {
                    result.output.push(QueryPartEvaluationContext::Aggregation {
                        before: before
                            .as_ref()
                            .or_else(|| work.initial_defaults.get(&id))
                            .map(|row| row.variables.clone()),
                        after: after.variables.clone(),
                        grouping_keys: grouping_keys.clone(),
                        default_before: (added && before.is_some())
                            || work.initial_defaults.contains_key(&id),
                        default_after: removed || is_default,
                        row_signature: signature,
                    });
                }
            }
            let group = work
                .groups
                .get_mut(&id)
                .ok_or(EvaluationError::CorruptData)?;
            group.default_row = is_default;
            group.last_delivered = match after {
                Some(row) if is_default => Presence::Default(row),
                Some(row) => Presence::Real(row),
                None => Presence::Absent,
            };
        }
        Ok(result)
    }

    async fn persist_part(
        &self,
        part: &PartProgram,
        mut work: PartWork,
    ) -> Result<(), EvaluationError> {
        let mut batch = TemporalBatch::new(self.namespace);
        let affected_groups: BTreeSet<_> = work
            .rows
            .values()
            .filter(|row| row.dirty)
            .flat_map(|row| row.old_group.into_iter().chain(row.group))
            .collect();
        let mut changed_cells = BTreeSet::new();
        for row in work.rows.values_mut().filter(|row| row.dirty) {
            let obsolete: Vec<_> = row
                .input
                .dependencies
                .difference(&row.dependencies)
                .cloned()
                .collect();
            for id in obsolete {
                let cell = work
                    .cells
                    .get_mut(&id)
                    .ok_or(EvaluationError::CorruptData)?;
                cell.subscribers.remove(&row.input.id);
                changed_cells.insert(id);
            }
            changed_cells.extend(row.dependencies.iter().cloned());
            if row.removed {
                batch.retire_input(&row.input).map_err(index_error)?;
                self.future_queue
                    .remove(0, row.input.id.incarnation.0)
                    .await?;
                continue;
            }
            self.reconcile_tickets(&mut row.input, std::mem::take(&mut row.requests))
                .await?;
            row.input.dependencies = std::mem::take(&mut row.dependencies);
            row.input.applied.predicate = std::mem::take(&mut row.predicate);
            row.input.applied.projection = std::mem::take(&mut row.projection);
            row.input
                .history
                .retain(|call, _| row.visited.contains(call));
            let frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
            row.input.values = frame
                .values
                .iter()
                .filter(|(call, _)| row.visited.contains(*call))
                .map(|(call, value)| {
                    (
                        call.clone(),
                        FunctionValue {
                            value: value.clone(),
                        },
                    )
                })
                .collect();
            batch
                .put(TemporalRecord::Origin {
                    key: row.input.origin_key(),
                    input: row.input.id,
                })
                .map_err(index_error)?;
            batch
                .put(TemporalRecord::Input(row.input.clone()))
                .map_err(index_error)?;
        }
        for id in changed_cells {
            let cell = work
                .cells
                .get_mut(&id)
                .ok_or(EvaluationError::CorruptData)?;
            if cell.subscribers.is_empty() {
                if matches!(id.owner, StateOwner::Input(_)) {
                    batch.delete(TemporalKey::Cell(id)).map_err(index_error)?;
                    continue;
                }
                if let FunctionState::TrueFor(state) = &mut cell.state {
                    state.settle(false, 0).map_err(index_error)?;
                }
            }
            batch
                .put(TemporalRecord::Cell(cell.clone()))
                .map_err(index_error)?;
        }
        for id in affected_groups {
            let group = work.groups.get(&id).ok_or(EvaluationError::CorruptData)?;
            if group.is_live() {
                batch
                    .put(TemporalRecord::GroupOrigin {
                        key: group.origin(),
                        group: id,
                    })
                    .map_err(index_error)?;
                batch
                    .put(TemporalRecord::Group(group.clone()))
                    .map_err(index_error)?;
            } else {
                for cell in &group.cells {
                    if work
                        .cells
                        .get(cell)
                        .is_none_or(|cell| !cell.subscribers.is_empty())
                    {
                        return Err(EvaluationError::CorruptData);
                    }
                }
                batch.retire_group(group).map_err(index_error)?;
            }
        }
        batch
            .put(TemporalRecord::Part(PartState {
                namespace: self.namespace,
                part: part.id,
                inputs: work
                    .rows
                    .values()
                    .filter(|row| !row.removed)
                    .map(|row| row.input.id)
                    .collect(),
            }))
            .map_err(index_error)?;
        self.index.apply(batch).await?;
        Ok(())
    }

    async fn reconcile_tickets(
        &self,
        input: &mut RetainedInput,
        requests: BTreeMap<(FunctionCall, u32), RequestedTicket>,
    ) -> Result<(), EvaluationError> {
        let mut retained = BTreeMap::new();
        for request in requests.into_values() {
            let existing = input
                .tickets
                .values()
                .find(|ticket| same_request(ticket, &request));
            if let Some(existing) = existing {
                retained.insert(existing.id.clone(), existing.clone());
            } else {
                let ticket = FutureTicket {
                    id: input
                        .allocate_ticket(request.call, request.slot)
                        .map_err(index_error)?,
                    cell: request.cell,
                    activation: request.activation,
                    original_time: request.original_time,
                    due_time: request.due_time,
                    attribution: request.attribution,
                };
                retained.insert(ticket.id.clone(), ticket);
            }
        }
        let entries = retained
            .values()
            .map(|ticket| Ok((ticket, queue::encode(ticket)?)))
            .collect::<Result<Vec<_>, IndexError>>()?;
        self.future_queue.remove(0, input.id.incarnation.0).await?;
        for (ticket, reference) in entries {
            self.future_queue
                .push(
                    PushType::Always,
                    0,
                    input.id.incarnation.0,
                    &reference,
                    ticket.original_time,
                    ticket.due_time,
                )
                .await?;
        }
        input.tickets = retained;
        Ok(())
    }
}

fn index_error(error: impl std::error::Error + Send + Sync + 'static) -> EvaluationError {
    EvaluationError::IndexError(IndexError::other(error))
}

fn touch(row: &mut WorkRow, event: EvaluationEvent) {
    if let Some(revision) = event.revision {
        row.input.source_revision = revision;
    }
    row.input.evaluated_at = ClockStamp {
        transaction_time: row.input.source_clock.transaction_time,
        realtime: row.input.evaluated_at.realtime.max(event.realtime),
    };
    row.input.context.clock = row.input.evaluated_at;
}

fn output_context(saved: &SavedContext, event: EvaluationEvent) -> SavedContext {
    let mut context = saved.clone();
    if let Some(transaction_time) = event.transaction_time {
        context.clock.transaction_time = transaction_time;
    }
    context.clock.realtime = context.clock.realtime.max(event.realtime);
    context
}

fn evaluation_context<'a>(
    part: &'a PartProgram,
    row: &'a WorkRow,
    snapshot: bool,
) -> ExpressionEvaluationContext<'a> {
    let mut context =
        ExpressionEvaluationContext::from_saved(&row.input.variables, &row.input.context);
    context.set_output_grouping_key(&part.grouping);
    context.set_temporal(TemporalEvaluation {
        input: row.input.id,
        occurrence: Vec::new(),
        frame: row.frame.clone(),
    });
    context.set_side_effects(if snapshot {
        SideEffects::Snapshot
    } else {
        SideEffects::Apply
    });
    context
}

fn snapshot_row(input: &RetainedInput, part: &PartProgram) -> Result<WorkRow, EvaluationError> {
    let row = WorkRow::new(input.clone(), Some(input.clone()), false);
    {
        let mut frame = row.frame.lock().map_err(|_| EvaluationError::CorruptData)?;
        for expression in part.predicates.iter().chain(&part.projection) {
            for effect in &expression.effects {
                frame.settled.insert(FunctionSite {
                    part: part.id,
                    position_in_query: effect.position_in_query,
                });
            }
        }
    }
    Ok(row)
}

fn subscribe(
    row: &mut WorkRow,
    id: FunctionCellId,
    initial: FunctionState,
    cells: &mut BTreeMap<FunctionCellId, FunctionCell>,
    groups: &mut BTreeMap<TemporalGroupId, GroupState>,
) -> Result<(), EvaluationError> {
    if let StateOwner::Group(group) = id.owner {
        groups
            .get_mut(&group)
            .ok_or(EvaluationError::CorruptData)?
            .cells
            .insert(id.clone());
    }
    if let Some(cell) = cells.get(&id) {
        match (&initial, &cell.state) {
            (FunctionState::Aggregate, FunctionState::Aggregate)
            | (FunctionState::TrueFor(_), FunctionState::TrueFor(_))
            | (
                FunctionState::UninitializedHistory,
                FunctionState::UninitializedHistory | FunctionState::History(_),
            ) => {}
            _ => return Err(EvaluationError::CorruptData),
        }
    }
    cells
        .entry(id.clone())
        .or_insert_with(|| FunctionCell {
            id: id.clone(),
            state: initial,
            subscribers: BTreeSet::new(),
        })
        .subscribers
        .insert(row.input.id);
    row.dependencies.insert(id);
    Ok(())
}

fn same_request(ticket: &FutureTicket, request: &RequestedTicket) -> bool {
    ticket.id.call == request.call
        && ticket.id.slot == request.slot
        && ticket.cell == request.cell
        && ticket.activation == request.activation
        && ticket.original_time == request.original_time
        && ticket.due_time == request.due_time
        && ticket.attribution == request.attribution
}

fn same_receipt(
    a: &AggregateContribution,
    b: &AggregateContribution,
) -> Result<bool, EvaluationError> {
    let owners_equal = match (&a.owner, &b.owner) {
        (ContributionOwner::Function(a), ContributionOwner::Function(b))
        | (ContributionOwner::PartCurrent(a), ContributionOwner::PartCurrent(b))
        | (ContributionOwner::PartDefault(a), ContributionOwner::PartDefault(b)) => a == b,
        _ => false,
    };
    let keys_equal = match (&a.key, &b.key) {
        (ContributionKey::GroupBy(a), ContributionKey::GroupBy(b)) => native_equal(
            &VariableValue::List(a.clone()),
            &VariableValue::List(b.clone()),
        )?,
        (ContributionKey::InputHash(a), ContributionKey::InputHash(b)) => a == b,
        (ContributionKey::Element(a), ContributionKey::Element(b)) => a == b,
        _ => false,
    };
    Ok(owners_equal
        && keys_equal
        && native_equal(
            &VariableValue::List(a.arguments.clone()),
            &VariableValue::List(b.arguments.clone()),
        )?)
}

fn native_equal(a: &VariableValue, b: &VariableValue) -> Result<bool, EvaluationError> {
    Ok(codec::encode_value(a).map_err(index_error)?
        == codec::encode_value(b).map_err(index_error)?)
}

fn variables_equal(a: &QueryVariables, b: &QueryVariables) -> Result<bool, EvaluationError> {
    if a.len() != b.len() {
        return Ok(false);
    }
    for ((ka, va), (kb, vb)) in a.iter().zip(b) {
        if ka != kb || !native_equal(va, vb)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn optional_variables_equal(
    a: Option<&QueryVariables>,
    b: Option<&QueryVariables>,
) -> Result<bool, EvaluationError> {
    match (a, b) {
        (Some(a), Some(b)) => variables_equal(a, b),
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}

fn coalesce_changes(
    changes: Vec<TemporalInputChange>,
    namespace: TemporalNamespace,
    part: PartId,
) -> Result<Vec<TemporalInputChange>, EvaluationError> {
    let mut merged: BTreeMap<Vec<u8>, TemporalInputChange> = BTreeMap::new();
    for mut change in changes {
        let key = codec::encode_key(&TemporalKey::Origin(OriginKey {
            namespace,
            part,
            origin: change.origin.clone(),
        }))
        .map_err(index_error)?;
        if let Some(previous) = merged.remove(&key) {
            change.before = previous.before;
        }
        merged.insert(key, change);
    }
    Ok(merged.into_values().collect())
}

fn delivered(presence: &Presence<DeliveredRow>) -> Option<&DeliveredRow> {
    match presence {
        Presence::Absent => None,
        Presence::Real(row) | Presence::Default(row) => Some(row),
    }
}

fn push_change(
    changes: &mut Vec<TemporalInputChange>,
    origin: InputOrigin,
    before: Option<&DeliveredRow>,
    after: Option<&DeliveredRow>,
    context: &SavedContext,
) {
    if let Some(row) = after.or(before) {
        changes.push(TemporalInputChange {
            origin,
            before: before.map(|row| row.variables.clone()),
            after: after.map(|row| row.variables.clone()),
            context: context.clone(),
            row_signature: row.row_signature,
        });
    }
}

fn group_signature(group: &GroupingKey) -> u64 {
    let mut hasher = SpookyHasher::default();
    group.0.hash(&mut hasher);
    hasher.finish()
}

fn expression_dependencies(expression: &Expression, registry: &FunctionRegistry) -> (bool, bool) {
    if let Expression::FunctionExpression(function) = expression {
        if registry
            .get_function(&function.name)
            .is_some_and(|function| function.effect() == FunctionEffect::Aggregate)
        {
            return (true, false);
        }
    }
    let free = matches!(
        expression,
        Expression::UnaryExpression(
            UnaryExpression::Identifier(_)
                | UnaryExpression::Parameter(_)
                | UnaryExpression::Property { .. }
        )
    );
    expression_children(expression)
        .into_iter()
        .fold((false, free), |(aggregate, free), child| {
            let (child_aggregate, child_free) = expression_dependencies(child, registry);
            (aggregate || child_aggregate, free || child_free)
        })
}

fn expression_children(expression: &Expression) -> Vec<&Expression> {
    match expression {
        Expression::UnaryExpression(UnaryExpression::ExpressionProperty { exp, .. }) => vec![exp],
        Expression::UnaryExpression(UnaryExpression::ListRange {
            start_bound,
            end_bound,
        }) => start_bound
            .iter()
            .chain(end_bound)
            .map(Box::as_ref)
            .collect(),
        Expression::UnaryExpression(UnaryExpression::Literal(literal)) => {
            let mut children = Vec::new();
            literal_children(literal, &mut children);
            children
        }
        Expression::IteratorExpression(iterator) => {
            let mut children = vec![iterator.list_expression.as_ref()];
            children.extend(iterator.filter.as_deref());
            children.extend(iterator.map_expression.as_deref());
            children
        }
        other => other.get_children(),
    }
}

fn literal_children<'a>(literal: &'a Literal, children: &mut Vec<&'a Expression>) {
    match literal {
        Literal::Expression(expression) => children.push(expression),
        Literal::Object(values) => {
            for (_, value) in values {
                literal_children(value, children);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
