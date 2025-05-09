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
}

impl MatchPathSolution {
    pub fn new(total_slots: usize, anchor_slot: usize) -> Self {
        let mut queued_slots = Vec::new();
        queued_slots.resize(total_slots, false);

        MatchPathSolution {
            solved_slots: BTreeMap::new(),
            total_slots,
            queued_slots,
            slot_cursors: VecDeque::new(),
            solution_signature: None,
            anchor_slot,
        }
    }

    pub fn mark_slot_solved(&mut self, slot_num: usize, value: Option<Arc<Element>>) {
        self.solved_slots.insert(slot_num, value);

        if self.solved_slots.len() == self.total_slots {
            let mut hasher = SpookyHasher::default();
            for (slot_num, value) in &self.solved_slots {
                slot_num.hash(&mut hasher);
                match value {
                    Some(value) => {
                        let elem_ref = value.get_reference();
                        elem_ref.source_id.hash(&mut hasher);
                        elem_ref.element_id.hash(&mut hasher);
                    }
                    None => 0.hash(&mut hasher),
                }
            }
            self.solution_signature = Some(hasher.finish());
        }
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
        if !match_path.slots[self.anchor_slot].optional {
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

        let opt_slots =
            match_path.get_optional_slots_on_common_paths(self.anchor_slot, empty_slots);

        let mut result = self.clone();
        for slot_num in &opt_slots {
            result.solved_slots.remove(slot_num);
        }
        result.solution_signature = None;
        for slot_num in &opt_slots {
            result.mark_slot_solved(*slot_num, None);
        }

        return Some(result);
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
