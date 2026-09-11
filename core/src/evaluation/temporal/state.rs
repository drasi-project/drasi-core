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

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    evaluation::{context::QueryVariables, variable_value::VariableValue},
    interface::{ResultKey, ResultOwner},
    models::{Element, ElementReference},
};

use super::{value, PlanVersion, QueryEpoch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TemporalNamespace {
    pub epoch: QueryEpoch,
    pub plan: PlanVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PartId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Incarnation(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TemporalInputId {
    pub namespace: TemporalNamespace,
    pub part: PartId,
    pub incarnation: Incarnation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TemporalGroupId {
    pub namespace: TemporalNamespace,
    pub producer: PartId,
    pub incarnation: Incarnation,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FunctionSite {
    pub part: PartId,
    pub position_in_query: usize,
}

/// Occurrences distinguish calls evaluated inside nested iterators.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FunctionCall {
    pub site: FunctionSite,
    pub occurrence: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MatchIdentity {
    Fixed {
        slots: Vec<Option<ElementReference>>,
    },
    Bounded {
        nodes: Vec<ElementReference>,
        paths: Vec<PathIdentity>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathIdentity {
    pub nodes: Vec<ElementReference>,
    pub relationships: Vec<ElementReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputOrigin {
    Match(MatchIdentity),
    Group(TemporalGroupId),
    Derived {
        input: TemporalInputId,
        ordinal: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginKey {
    pub namespace: TemporalNamespace,
    pub part: PartId,
    pub origin: InputOrigin,
}

/// Ordered, typed grouping values, never a public row-signature hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupingKey(#[serde(with = "value::values")] pub Vec<VariableValue>);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupOrigin {
    pub namespace: TemporalNamespace,
    pub producer: PartId,
    pub grouping: GroupingKey,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum StateOwner {
    Input(TemporalInputId),
    Group(TemporalGroupId),
}

impl StateOwner {
    pub fn namespace(&self) -> TemporalNamespace {
        match self {
            Self::Input(id) => id.namespace,
            Self::Group(id) => id.namespace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FunctionCellId {
    pub owner: StateOwner,
    pub call: FunctionCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockStamp {
    pub transaction_time: u64,
    pub realtime: u64,
}

impl ClockStamp {
    pub fn at_deadline(self, due_time: u64) -> Self {
        Self {
            realtime: self.realtime.max(due_time),
            ..self
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedContext {
    pub clock: ClockStamp,
    pub input_grouping_hash: u64,
    pub solution_signature: Option<u64>,
    #[serde(with = "value::optional_element")]
    pub anchor: Option<Arc<Element>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContributionOwner {
    Function(usize),
    PartCurrent(usize),
    PartDefault(usize),
}

impl From<&ResultOwner> for ContributionOwner {
    fn from(owner: &ResultOwner) -> Self {
        match owner {
            ResultOwner::Function(v) => Self::Function(*v),
            ResultOwner::PartCurrent(v) => Self::PartCurrent(*v),
            ResultOwner::PartDefault(v) => Self::PartDefault(*v),
        }
    }
}

impl From<&ContributionOwner> for ResultOwner {
    fn from(owner: &ContributionOwner) -> Self {
        match owner {
            ContributionOwner::Function(v) => Self::Function(*v),
            ContributionOwner::PartCurrent(v) => Self::PartCurrent(*v),
            ContributionOwner::PartDefault(v) => Self::PartDefault(*v),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContributionKey {
    GroupBy(#[serde(with = "value::values")] Vec<VariableValue>),
    InputHash(u64),
    Element(ElementReference),
}

impl From<&ResultKey> for ContributionKey {
    fn from(key: &ResultKey) -> Self {
        match key {
            ResultKey::GroupBy(v) => Self::GroupBy(v.as_ref().clone()),
            ResultKey::InputHash(v) => Self::InputHash(*v),
            ResultKey::Element(v) => Self::Element(v.clone()),
        }
    }
}

impl From<&ContributionKey> for ResultKey {
    fn from(key: &ContributionKey) -> Self {
        match key {
            ContributionKey::GroupBy(v) => Self::GroupBy(Arc::new(v.clone())),
            ContributionKey::InputHash(v) => Self::InputHash(*v),
            ContributionKey::Element(v) => Self::Element(v.clone()),
        }
    }
}

/// Arguments already applied to an accumulator, not arguments to reevaluate on removal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateContribution {
    pub call: FunctionCall,
    pub owner: ContributionOwner,
    pub key: ContributionKey,
    #[serde(with = "value::values")]
    pub arguments: Vec<VariableValue>,
    pub context: SavedContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveredRow {
    #[serde(with = "value::variables")]
    pub variables: QueryVariables,
    pub row_signature: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum Presence<T> {
    #[default]
    Absent,
    Real(T),
    Default(T),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppliedContribution {
    pub predicate: Vec<AggregateContribution>,
    pub projection: Vec<AggregateContribution>,
    pub last_delivered: Presence<DeliveredRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceRevision(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemporalCatalog {
    pub namespace: TemporalNamespace,
    pub query: String,
    pub next_revision: SourceRevision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryCapture {
    pub revision: SourceRevision,
    #[serde(with = "value")]
    pub value: VariableValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionValue {
    #[serde(with = "value")]
    pub value: VariableValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryState {
    pub revision: SourceRevision,
    #[serde(with = "value")]
    pub current: VariableValue,
    #[serde(with = "value")]
    pub previous: VariableValue,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TicketId {
    pub input: TemporalInputId,
    pub call: FunctionCall,
    pub slot: u32,
    pub generation: Generation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FutureTicket {
    pub id: TicketId,
    pub cell: Option<FunctionCellId>,
    pub activation: Generation,
    pub original_time: u64,
    pub due_time: u64,
    pub attribution: Option<ElementReference>,
}

impl FutureTicket {
    pub fn validate(&self) -> Result<(), TemporalStateError> {
        if self.id.call.site.part != self.id.input.part {
            return Err(TemporalStateError::Invalid(
                "ticket site is in another part",
            ));
        }
        if let Some(cell) = &self.cell {
            if cell.owner.namespace() != self.id.input.namespace {
                return Err(TemporalStateError::NamespaceMismatch);
            }
            if cell.call != self.id.call
                || matches!(cell.owner, StateOwner::Input(input) if input != self.id.input)
            {
                return Err(TemporalStateError::Invalid(
                    "ticket refers to another input or call",
                ));
            }
        }
        Ok(())
    }
}

/// Only temporal query parts retain rows. Snapshot evaluation must not mutate this record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedInput {
    pub id: TemporalInputId,
    pub origin: InputOrigin,
    #[serde(with = "value::variables")]
    pub variables: QueryVariables,
    pub source_revision: SourceRevision,
    pub source_clock: ClockStamp,
    pub evaluated_at: ClockStamp,
    pub context: SavedContext,
    pub row_signature: u64,
    pub applied: AppliedContribution,
    pub dependencies: BTreeSet<FunctionCellId>,
    pub tickets: BTreeMap<TicketId, FutureTicket>,
    pub history: BTreeMap<FunctionCall, HistoryCapture>,
    pub values: BTreeMap<FunctionCall, FunctionValue>,
    pub next_ticket_generation: Generation,
}

impl RetainedInput {
    pub fn origin_key(&self) -> OriginKey {
        OriginKey {
            namespace: self.id.namespace,
            part: self.id.part,
            origin: self.origin.clone(),
        }
    }

    pub fn allocate_ticket(
        &mut self,
        call: FunctionCall,
        slot: u32,
    ) -> Result<TicketId, TemporalStateError> {
        if call.site.part != self.id.part {
            return Err(TemporalStateError::Invalid(
                "ticket site is in another part",
            ));
        }
        let generation = self.next_ticket_generation;
        self.next_ticket_generation = Generation(
            generation
                .0
                .checked_add(1)
                .ok_or(TemporalStateError::GenerationExhausted)?,
        );
        Ok(TicketId {
            input: self.id,
            call,
            slot,
            generation,
        })
    }

    /// Queue delivery is valid only while the exact ticket is still retained.
    pub fn accepts(&self, ticket: &FutureTicket) -> bool {
        self.tickets.get(&ticket.id).is_some_and(|live| {
            live.due_time == ticket.due_time
                && live.original_time == ticket.original_time
                && live.activation == ticket.activation
                && live.cell == ticket.cell
                && live.attribution == ticket.attribution
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupState {
    pub id: TemporalGroupId,
    pub grouping: GroupingKey,
    pub members: BTreeSet<TemporalInputId>,
    pub default_row: bool,
    pub cells: BTreeSet<FunctionCellId>,
    pub last_delivered: Presence<DeliveredRow>,
}

impl GroupState {
    pub fn origin(&self) -> GroupOrigin {
        GroupOrigin {
            namespace: self.id.namespace,
            producer: self.id.producer,
            grouping: self.grouping.clone(),
        }
    }

    pub fn is_live(&self) -> bool {
        self.default_row || !self.members.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateState {
    False,
    True { since: u64, activation: Generation },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrueForState {
    pub predicate: PredicateState,
    pub next_activation: Generation,
}

impl Default for TrueForState {
    fn default() -> Self {
        Self {
            predicate: PredicateState::False,
            next_activation: Generation(0),
        }
    }
}

impl TrueForState {
    /// Call once with the settled net condition, never between a removal and its replacement.
    pub fn settle(&mut self, condition: bool, realtime: u64) -> Result<(), TemporalStateError> {
        match (&self.predicate, condition) {
            (_, false) => self.predicate = PredicateState::False,
            (PredicateState::True { .. }, true) => {}
            (PredicateState::False, true) => {
                let activation = self.next_activation;
                self.next_activation = Generation(
                    activation
                        .0
                        .checked_add(1)
                        .ok_or(TemporalStateError::GenerationExhausted)?,
                );
                self.predicate = PredicateState::True {
                    since: realtime,
                    activation,
                };
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSample {
    pub revision: SourceRevision,
    pub expires_at: u64,
    #[serde(with = "value")]
    pub value: VariableValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlidingWindowState {
    pub samples: BTreeMap<Generation, WindowSample>,
    pub next_sample: Generation,
}

impl SlidingWindowState {
    /// Return each expired sample once so its applied contribution can be retracted once.
    pub fn expire(&mut self, realtime: u64) -> Vec<WindowSample> {
        let mut expired = Vec::new();
        self.samples.retain(|_, sample| {
            if sample.expires_at <= realtime {
                expired.push(sample.clone());
                false
            } else {
                true
            }
        });
        expired
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FunctionState {
    Aggregate,
    TrueFor(TrueForState),
    History(HistoryState),
    SlidingWindow(SlidingWindowState),
    UninitializedHistory,
}

/// A cell shares its owner's lifetime. Keep its generation counters while the owner is live,
/// even when its predicate becomes false or its last subscriber is temporarily removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCell {
    pub id: FunctionCellId,
    pub state: FunctionState,
    pub subscribers: BTreeSet<TemporalInputId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpochState {
    pub namespace: TemporalNamespace,
    pub next_incarnation: Incarnation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartState {
    pub namespace: TemporalNamespace,
    pub part: PartId,
    pub inputs: BTreeSet<TemporalInputId>,
}

impl EpochState {
    pub fn new(namespace: TemporalNamespace) -> Self {
        Self {
            namespace,
            next_incarnation: Incarnation(0),
        }
    }

    pub fn allocate(&mut self) -> Result<Incarnation, TemporalStateError> {
        let id = self.next_incarnation;
        self.next_incarnation = Incarnation(
            id.0.checked_add(1)
                .ok_or(TemporalStateError::IncarnationExhausted)?,
        );
        Ok(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TemporalKey {
    Epoch(TemporalNamespace),
    Origin(OriginKey),
    Input(TemporalInputId),
    GroupOrigin(GroupOrigin),
    Group(TemporalGroupId),
    Cell(FunctionCellId),
    Part {
        namespace: TemporalNamespace,
        part: PartId,
    },
}

impl TemporalKey {
    pub fn namespace(&self) -> TemporalNamespace {
        match self {
            Self::Epoch(ns) => *ns,
            Self::Origin(key) => key.namespace,
            Self::Input(id) => id.namespace,
            Self::GroupOrigin(key) => key.namespace,
            Self::Group(id) => id.namespace,
            Self::Cell(id) => id.owner.namespace(),
            Self::Part { namespace, .. } => *namespace,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TemporalRecord {
    Epoch(EpochState),
    Origin {
        key: OriginKey,
        input: TemporalInputId,
    },
    Input(RetainedInput),
    GroupOrigin {
        key: GroupOrigin,
        group: TemporalGroupId,
    },
    Group(GroupState),
    Cell(FunctionCell),
    Part(PartState),
}

impl TemporalRecord {
    pub fn key(&self) -> TemporalKey {
        match self {
            Self::Epoch(state) => TemporalKey::Epoch(state.namespace),
            Self::Origin { key, .. } => TemporalKey::Origin(key.clone()),
            Self::Input(row) => TemporalKey::Input(row.id),
            Self::GroupOrigin { key, .. } => TemporalKey::GroupOrigin(key.clone()),
            Self::Group(group) => TemporalKey::Group(group.id),
            Self::Cell(cell) => TemporalKey::Cell(cell.id.clone()),
            Self::Part(state) => TemporalKey::Part {
                namespace: state.namespace,
                part: state.part,
            },
        }
    }

    pub fn validate(&self) -> Result<(), TemporalStateError> {
        let namespace = self.key().namespace();
        let same_namespace = |other| {
            if namespace == other {
                Ok(())
            } else {
                Err(TemporalStateError::NamespaceMismatch)
            }
        };
        match self {
            Self::Epoch(_) => {}
            Self::Part(state) => {
                for input in &state.inputs {
                    same_namespace(input.namespace)?;
                    if input.part != state.part {
                        return Err(TemporalStateError::Invalid(
                            "part membership contains an input from another part",
                        ));
                    }
                }
            }
            Self::Origin { key, input } => {
                same_namespace(input.namespace)?;
                validate_origin(&key.origin, namespace)?;
                if input.part != key.part {
                    return Err(TemporalStateError::Invalid("origin points to another part"));
                }
            }
            Self::Input(row) => {
                validate_origin(&row.origin, namespace)?;
                for cell in &row.dependencies {
                    same_namespace(cell.owner.namespace())?;
                    if matches!(cell.owner, StateOwner::Input(input) if input != row.id) {
                        return Err(TemporalStateError::Invalid(
                            "input depends on another input's cell",
                        ));
                    }
                }
                for (id, ticket) in &row.tickets {
                    ticket.validate()?;
                    if *id != ticket.id
                        || id.input != row.id
                        || id.generation >= row.next_ticket_generation
                    {
                        return Err(TemporalStateError::Invalid("inconsistent retained ticket"));
                    }
                }
                for capture in row.history.values() {
                    if capture.revision > row.source_revision {
                        return Err(TemporalStateError::Invalid(
                            "history is from a future revision",
                        ));
                    }
                }
            }
            Self::GroupOrigin { key, group } => {
                same_namespace(group.namespace)?;
                if group.producer != key.producer {
                    return Err(TemporalStateError::Invalid(
                        "group origin points to another part",
                    ));
                }
            }
            Self::Group(group) => {
                if group.default_row != matches!(group.last_delivered, Presence::Default(_)) {
                    return Err(TemporalStateError::Invalid(
                        "group default lifetime disagrees with its delivered presence",
                    ));
                }
                for member in &group.members {
                    same_namespace(member.namespace)?;
                }
                for cell in &group.cells {
                    if cell.owner != StateOwner::Group(group.id) {
                        return Err(TemporalStateError::Invalid("group cell has another owner"));
                    }
                }
            }
            Self::Cell(cell) => {
                for subscriber in &cell.subscribers {
                    same_namespace(subscriber.namespace)?;
                }
                match &cell.state {
                    FunctionState::TrueFor(state) => {
                        if let PredicateState::True { activation, .. } = state.predicate {
                            if activation >= state.next_activation {
                                return Err(TemporalStateError::Invalid(
                                    "invalid predicate activation",
                                ));
                            }
                        }
                    }
                    FunctionState::Aggregate
                    | FunctionState::History(_)
                    | FunctionState::UninitializedHistory => {}
                    FunctionState::SlidingWindow(state) => {
                        if state.samples.keys().any(|id| id >= &state.next_sample) {
                            return Err(TemporalStateError::Invalid(
                                "invalid window sample generation",
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_origin(
    origin: &InputOrigin,
    namespace: TemporalNamespace,
) -> Result<(), TemporalStateError> {
    let origin_namespace = match origin {
        InputOrigin::Match(_) => namespace,
        InputOrigin::Group(id) => id.namespace,
        InputOrigin::Derived { input, .. } => input.namespace,
    };
    if origin_namespace != namespace {
        return Err(TemporalStateError::NamespaceMismatch);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum TemporalMutation {
    Put(TemporalRecord),
    Delete(TemporalKey),
}

#[derive(Debug, Clone)]
pub struct TemporalBatch {
    namespace: TemporalNamespace,
    mutations: Vec<TemporalMutation>,
}

impl TemporalBatch {
    pub fn new(namespace: TemporalNamespace) -> Self {
        Self {
            namespace,
            mutations: Vec::new(),
        }
    }

    pub fn namespace(&self) -> TemporalNamespace {
        self.namespace
    }

    pub fn mutations(&self) -> &[TemporalMutation] {
        &self.mutations
    }

    pub fn put(&mut self, record: TemporalRecord) -> Result<(), TemporalStateError> {
        record.validate()?;
        self.check_namespace(record.key().namespace())?;
        self.mutations.push(TemporalMutation::Put(record));
        Ok(())
    }

    pub fn delete(&mut self, key: TemporalKey) -> Result<(), TemporalStateError> {
        self.check_namespace(key.namespace())?;
        self.mutations.push(TemporalMutation::Delete(key));
        Ok(())
    }

    /// Also retract `row.applied`, unsubscribe its dependencies, and remove these tickets
    /// from the queue in the same root transaction. No tombstone is retained.
    pub fn retire_input(
        &mut self,
        row: &RetainedInput,
    ) -> Result<Vec<FutureTicket>, TemporalStateError> {
        self.check_namespace(row.id.namespace)?;
        self.delete(TemporalKey::Origin(row.origin_key()))?;
        self.delete(TemporalKey::Input(row.id))?;
        for cell in &row.dependencies {
            if cell.owner == StateOwner::Input(row.id) {
                self.delete(TemporalKey::Cell(cell.clone()))?;
            }
        }
        Ok(row.tickets.values().cloned().collect())
    }

    /// The caller must detach subscribers and cancel their tickets before retiring a group.
    pub fn retire_group(&mut self, group: &GroupState) -> Result<(), TemporalStateError> {
        self.check_namespace(group.id.namespace)?;
        if group.is_live() {
            return Err(TemporalStateError::Invalid("cannot retire a live group"));
        }
        for cell in &group.cells {
            if cell.owner != StateOwner::Group(group.id) {
                return Err(TemporalStateError::Invalid("group cell has another owner"));
            }
        }
        self.delete(TemporalKey::GroupOrigin(group.origin()))?;
        self.delete(TemporalKey::Group(group.id))?;
        for cell in &group.cells {
            self.delete(TemporalKey::Cell(cell.clone()))?;
        }
        Ok(())
    }

    fn check_namespace(&self, namespace: TemporalNamespace) -> Result<(), TemporalStateError> {
        if namespace != self.namespace {
            return Err(TemporalStateError::NamespaceMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TemporalStateError {
    #[error("temporal records belong to different query epochs or plan formats")]
    NamespaceMismatch,
    #[error("temporal input incarnation space exhausted; rebuild into a new query epoch")]
    IncarnationExhausted,
    #[error("temporal generation space exhausted; retire this lifetime")]
    GenerationExhausted,
    #[error("invalid temporal state: {0}")]
    Invalid(&'static str),
}

#[cfg(test)]
mod tests;
