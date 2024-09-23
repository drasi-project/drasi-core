use hashers::jenkins::spooky_hash::SpookyHasher;

use crate::evaluation::context::QueryVariables;

use std::hash::{Hash, Hasher};

use std::collections::VecDeque;

use crate::models::Element;

use std::sync::Arc;

use std::collections::BTreeMap;

use super::match_path::MatchPath;

pub(crate) type SolutionSignature = u64;

#[derive(Clone, Debug)]
pub struct MatchPathSolution {
    pub(crate) solved_slots: BTreeMap<usize, Arc<Element>>,
    pub(crate) total_slots: usize,
    pub(crate) queued_slots: Vec<bool>,
    pub(crate) slot_cursors: VecDeque<(usize, Arc<Element>)>,
    pub(crate) solution_signature: Option<SolutionSignature>,
}

impl MatchPathSolution {
    pub fn new(total_slots: usize) -> Self {
        let mut queued_slots = Vec::new();
        queued_slots.resize(total_slots, false);

        MatchPathSolution {
            solved_slots: BTreeMap::new(),
            total_slots,
            queued_slots,
            slot_cursors: VecDeque::new(),
            solution_signature: None,
        }
    }

    pub fn mark_slot_solved(&mut self, slot_num: usize, value: Arc<Element>) {
        self.solved_slots.insert(slot_num, value);
        if self.solved_slots.len() == self.total_slots {
            let mut hasher = SpookyHasher::default();
            for (slot_num, value) in &self.solved_slots {
                slot_num.hash(&mut hasher);
                let elem_ref = value.get_reference();
                elem_ref.source_id.hash(&mut hasher);
                elem_ref.element_id.hash(&mut hasher);
            }
            self.solution_signature = Some(hasher.finish());
        }
    }

    pub fn enqueue_slot(&mut self, slot_num: usize, value: Arc<Element>) {
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
                            element.to_expression_variable(),
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
