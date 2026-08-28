// Copyright 2024 The Drasi Authors.
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

use hashers::jenkins::spooky_hash::SpookyHasher;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::variable_value::VariableValue;

use std::hash::{Hash, Hasher};

use std::collections::{HashSet, VecDeque};

use crate::models::Element;

use std::sync::Arc;

use std::collections::BTreeMap;

use super::match_path::MatchPath;

pub(crate) type SolutionSignature = u64;

#[derive(Clone, Debug)]
pub struct MatchPathSolution {
    pub(crate) solved_slots: BTreeMap<usize, Option<Arc<Element>>>,
    pub(crate) total_slots: usize,
    pub(crate) queued_slots: Vec<bool>,
    pub(crate) slot_cursors: VecDeque<(usize, Option<Arc<Element>>)>,
    pub(crate) solution_signature: Option<SolutionSignature>,
    pub(crate) anchor_slot: usize,
    pub(crate) anchor_slots: HashSet<usize>,
    pub(crate) defaulted_clauses: HashSet<usize>,
}

impl MatchPathSolution {
    pub fn new(total_slots: usize, anchor_slot: usize) -> Self {
        let mut queued_slots = Vec::new();
        queued_slots.resize(total_slots, false);

        let mut anchor_slots = HashSet::new();
        anchor_slots.insert(anchor_slot);
        MatchPathSolution {
            solved_slots: BTreeMap::new(),
            total_slots,
            queued_slots,
            slot_cursors: VecDeque::new(),
            solution_signature: None,
            anchor_slot,
            anchor_slots,
            defaulted_clauses: HashSet::new(),
        }
    }

    pub fn mark_slot_solved(&mut self, slot_num: usize, value: Option<Arc<Element>>) {
        self.solved_slots.insert(slot_num, value);

        if self.solved_slots.len() == self.total_slots {
            self.refresh_signature();
        }
    }

    pub(crate) fn canonicalize_optional_defaults(&mut self, match_path: &MatchPath) {
        for (clause_id, clause) in match_path.clauses.iter().enumerate() {
            if !clause.optional
                || clause
                    .slots
                    .iter()
                    .all(|slot_num| matches!(self.solved_slots.get(slot_num), Some(Some(_))))
            {
                continue;
            }

            self.defaulted_clauses.insert(clause_id);
            for slot_num in &clause.introduced_slots {
                self.solved_slots.insert(*slot_num, None);
            }
        }
        self.refresh_signature();
    }

    pub(crate) fn into_optional_fallback(
        mut self,
        match_path: &MatchPath,
        failed_clause: Option<usize>,
    ) -> Option<MatchPathSolution> {
        let mut defaulted = false;
        if let Some(clause_id) = failed_clause {
            let clause = &match_path.clauses[clause_id];
            if !clause.optional {
                return None;
            }
            for slot_num in &clause.introduced_slots {
                self.solved_slots.insert(*slot_num, None);
            }
            self.defaulted_clauses.insert(clause_id);
            defaulted = true;
        }
        for (slot_num, slot) in match_path.slots.iter().enumerate() {
            if self.solved_slots.contains_key(&slot_num) {
                continue;
            }
            if !match_path.clauses[slot.introduction_clause].optional {
                return None;
            }
            self.solved_slots.insert(slot_num, None);
            defaulted = true;
        }
        self.canonicalize_optional_defaults(match_path);
        for clause in &match_path.clauses {
            if clause.optional {
                defaulted |= clause
                    .introduced_slots
                    .iter()
                    .any(|slot_num| matches!(self.solved_slots.get(slot_num), Some(None)));
            } else if !clause
                .slots
                .iter()
                .all(|slot_num| matches!(self.solved_slots.get(slot_num), Some(Some(_))))
            {
                return None;
            }
        }
        defaulted.then_some(self)
    }

    pub(crate) fn clause_is_real(&self, match_path: &MatchPath, clause_id: usize) -> bool {
        !self.defaulted_clauses.contains(&clause_id)
            && match_path.clauses[clause_id]
                .slots
                .iter()
                .all(|slot_num| matches!(self.solved_slots.get(slot_num), Some(Some(_))))
    }

    pub(crate) fn defaulted_clause_count(&self) -> usize {
        self.defaulted_clauses.len()
    }

    pub(crate) fn merge_anchor_provenance(&mut self, other: &MatchPathSolution) {
        self.anchor_slots.extend(&other.anchor_slots);
    }

    pub(crate) fn optional_clause_memberships(
        &self,
        match_path: &MatchPath,
    ) -> Vec<(usize, SolutionSignature)> {
        match_path
            .clauses
            .iter()
            .enumerate()
            .filter(|(clause_id, clause)| {
                clause.optional && self.clause_is_real(match_path, *clause_id)
            })
            .map(|(clause_id, _)| (clause_id, self.upstream_signature(match_path, clause_id)))
            .collect()
    }

    pub(crate) fn optional_anchor_memberships(
        &self,
        match_path: &MatchPath,
    ) -> Vec<(usize, SolutionSignature)> {
        let anchor_clauses = self
            .anchor_slots
            .iter()
            .map(|slot_num| match_path.slots[*slot_num].introduction_clause)
            .filter(|clause_id| {
                match_path.clauses[*clause_id].optional
                    && self.clause_is_real(match_path, *clause_id)
            })
            .collect::<HashSet<_>>();

        self.optional_clause_memberships(match_path)
            .into_iter()
            .filter(|(clause_id, _)| anchor_clauses.contains(clause_id))
            .collect()
    }

    pub(crate) fn default_clause(&mut self, match_path: &MatchPath, clause_id: usize) {
        self.defaulted_clauses.insert(clause_id);
        for slot_num in &match_path.clauses[clause_id].introduced_slots {
            self.solved_slots.insert(*slot_num, None);
        }
        self.canonicalize_optional_defaults(match_path);
    }

    pub(crate) fn upstream_signature(&self, match_path: &MatchPath, clause_id: usize) -> u64 {
        let mut hasher = SpookyHasher::default();
        clause_id.hash(&mut hasher);
        for (slot_num, value) in &self.solved_slots {
            if match_path.slots[*slot_num].introduction_clause >= clause_id {
                continue;
            }
            slot_num.hash(&mut hasher);
            hash_element(value, &mut hasher);
        }
        hasher.finish()
    }

    fn refresh_signature(&mut self) {
        if self.solved_slots.len() != self.total_slots {
            self.solution_signature = None;
            return;
        }

        let mut hasher = SpookyHasher::default();
        for (slot_num, value) in &self.solved_slots {
            slot_num.hash(&mut hasher);
            hash_element(value, &mut hasher);
        }
        self.solution_signature = Some(hasher.finish());
    }

    pub fn enqueue_slot(&mut self, slot_num: usize, value: Option<Arc<Element>>) {
        if !self.queued_slots[slot_num] {
            self.slot_cursors.push_back((slot_num, value));
            self.queued_slots[slot_num] = true;
        }
    }

    pub fn is_slot_solved(&self, slot_num: usize) -> bool {
        self.solved_slots.contains_key(&slot_num)
    }

    pub fn get_solution_signature(&self) -> Option<SolutionSignature> {
        self.solution_signature
    }

    pub fn get_empty_optional_solution(&self, match_path: &MatchPath) -> Option<MatchPathSolution> {
        let anchor_clauses = self
            .anchor_slots
            .iter()
            .filter(|slot_num| matches!(self.solved_slots.get(slot_num), Some(Some(_))))
            .map(|slot_num| match_path.slots[*slot_num].introduction_clause)
            .filter(|clause_id| match_path.clauses[*clause_id].optional)
            .collect::<HashSet<_>>();
        if anchor_clauses.is_empty() {
            return None;
        }

        if self.solved_slots.len() != self.total_slots {
            return None;
        }

        let empty_slots = self
            .solved_slots
            .iter()
            .filter(|(_, value)| value.is_none())
            .map(|(slot_num, _)| *slot_num)
            .collect::<HashSet<_>>();

        let mut opt_slots = HashSet::new();
        for anchor_slot in &self.anchor_slots {
            opt_slots.extend(match_path.get_optional_slots_for_default(*anchor_slot, &empty_slots));
        }

        let mut result = self.clone();
        for slot_num in &opt_slots {
            result.solved_slots.remove(slot_num);
        }
        result.solution_signature = None;
        for slot_num in &opt_slots {
            result.mark_slot_solved(*slot_num, None);
        }
        result.defaulted_clauses.extend(anchor_clauses);
        result.canonicalize_optional_defaults(match_path);

        Some(result)
    }

    #[allow(clippy::explicit_counter_loop)]
    pub fn into_query_variables(
        &self,
        match_path: &MatchPath,
        base_variables: &QueryVariables,
    ) -> QueryVariables {
        let mut result = base_variables.clone();
        let mut slot_num = 0;
        for slot in &match_path.slots {
            match self.solved_slots.get(&slot_num) {
                Some(element) => {
                    if let Some(annotation) = &slot.spec.annotation {
                        result.insert(
                            annotation.to_string().into_boxed_str(),
                            match element {
                                Some(element) => element.to_expression_variable(),
                                None => VariableValue::Null,
                            },
                        );
                    }
                }

                None => {
                    //log warning
                }
            }
            slot_num += 1;
        }
        result
    }
}

fn hash_element(value: &Option<Arc<Element>>, hasher: &mut SpookyHasher) {
    match value {
        Some(value) => {
            let elem_ref = value.get_reference();
            elem_ref.source_id.hash(hasher);
            elem_ref.element_id.hash(hasher);
        }
        None => 0.hash(hasher),
    }
}
