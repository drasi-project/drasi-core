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

use drasi_query_ast::ast::Expression;

use super::merge_relation_match;

use super::merge_node_match;

use crate::evaluation::EvaluationError;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use drasi_query_ast::ast;

#[derive(Debug)]
pub struct MatchPath {
    pub slots: Vec<MatchPathSlot>,
    pub(crate) segments: Vec<MatchPathSegment>,
    pub(crate) clauses: Vec<MatchPathClause>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MatchPathSegment {
    pub(crate) start_slot: usize,
    pub(crate) relation_slot: usize,
    pub(crate) end_slot: usize,
    pub(crate) direction: ast::Direction,
    pub(crate) pattern_id: usize,
    pub(crate) clause_id: usize,
}

impl MatchPathSegment {
    pub(crate) fn contains_slot(&self, slot_num: usize) -> bool {
        self.start_slot == slot_num || self.relation_slot == slot_num || self.end_slot == slot_num
    }
}

#[derive(Debug)]
pub(crate) struct MatchPathClause {
    pub(crate) optional: bool,
    pub(crate) slots: HashSet<usize>,
    pub(crate) introduced_slots: HashSet<usize>,
}

impl MatchPath {
    pub fn from_query(query_part: &ast::QueryPart) -> Result<Self, EvaluationError> {
        let mut slots = Vec::new();

        let mut alias_map = HashMap::new();
        let mut segments = Vec::new();
        let clause_count = query_part
            .match_clauses
            .iter()
            .map(|clause| clause.clause_id)
            .max()
            .map_or(0, |max_id| max_id + 1);
        let mut clauses = (0..clause_count)
            .map(|_| MatchPathClause {
                optional: false,
                slots: HashSet::new(),
                introduced_slots: HashSet::new(),
            })
            .collect::<Vec<_>>();
        let mut seen_clause_ids = HashSet::new();

        for (path_index, mc) in query_part.match_clauses.iter().enumerate() {
            if mc.clause_id >= clauses.len() {
                return Err(EvaluationError::ParseError);
            }
            if seen_clause_ids.insert(mc.clause_id) {
                clauses[mc.clause_id].optional = mc.optional;
            } else if clauses[mc.clause_id].optional != mc.optional {
                return Err(EvaluationError::ParseError);
            }
            let slot_num = merge_node_match(
                &mc.start,
                &mut slots,
                &mut alias_map,
                path_index,
                mc.clause_id,
                mc.optional,
            )?;
            clauses[mc.clause_id].slots.insert(slot_num);
            if slots[slot_num].introduction_clause == mc.clause_id {
                clauses[mc.clause_id].introduced_slots.insert(slot_num);
            }
            let mut prev_slot_num = slot_num;

            for p in &mc.path {
                let rel_slot_num = merge_relation_match(
                    &p.0,
                    &mut slots,
                    &mut alias_map,
                    path_index,
                    mc.clause_id,
                    mc.optional,
                )?;
                let node_slot_num = merge_node_match(
                    &p.1,
                    &mut slots,
                    &mut alias_map,
                    path_index,
                    mc.clause_id,
                    mc.optional,
                )?;
                clauses[mc.clause_id].slots.insert(rel_slot_num);
                clauses[mc.clause_id].slots.insert(node_slot_num);
                if slots[rel_slot_num].introduction_clause == mc.clause_id {
                    clauses[mc.clause_id].introduced_slots.insert(rel_slot_num);
                }
                if slots[node_slot_num].introduction_clause == mc.clause_id {
                    clauses[mc.clause_id].introduced_slots.insert(node_slot_num);
                }
                segments.push(MatchPathSegment {
                    start_slot: prev_slot_num,
                    relation_slot: rel_slot_num,
                    end_slot: node_slot_num,
                    direction: p.0.direction,
                    pattern_id: path_index,
                    clause_id: mc.clause_id,
                });

                match p.0.direction {
                    ast::Direction::Right => {
                        slots[prev_slot_num].out_slots.push(rel_slot_num);
                        slots[rel_slot_num].in_slots.push(prev_slot_num);

                        slots[rel_slot_num].out_slots.push(node_slot_num);
                        slots[node_slot_num].in_slots.push(rel_slot_num);
                    }
                    ast::Direction::Left => {
                        slots[prev_slot_num].in_slots.push(rel_slot_num);
                        slots[rel_slot_num].out_slots.push(prev_slot_num);

                        slots[rel_slot_num].in_slots.push(node_slot_num);
                        slots[node_slot_num].out_slots.push(rel_slot_num);
                    }
                    ast::Direction::Either => {
                        slots[prev_slot_num].in_slots.push(rel_slot_num);
                        slots[prev_slot_num].out_slots.push(rel_slot_num);
                        slots[rel_slot_num].in_slots.push(prev_slot_num);
                        slots[rel_slot_num].out_slots.push(prev_slot_num);

                        slots[node_slot_num].in_slots.push(rel_slot_num);
                        slots[node_slot_num].out_slots.push(rel_slot_num);
                        slots[rel_slot_num].in_slots.push(node_slot_num);
                        slots[rel_slot_num].out_slots.push(node_slot_num);
                    }
                }

                prev_slot_num = node_slot_num;
            }
        }
        if seen_clause_ids.len() != clause_count {
            return Err(EvaluationError::ParseError);
        }

        Ok(MatchPath {
            slots,
            segments,
            clauses,
        })
    }

    /// Finds the slots that must be null in the solution that existed before an
    /// optional anchor completed its match.
    pub fn get_optional_slots_for_default(
        &self,
        anchor_slot_num: usize,
        empty_slots: &HashSet<usize>,
    ) -> HashSet<usize> {
        let mut optional_slots = HashSet::new();
        let anchor_clause = self.slots[anchor_slot_num].introduction_clause;
        if !self.clauses[anchor_clause].optional {
            return optional_slots;
        }
        let mut pending_clauses = VecDeque::from([(anchor_clause, false)]);
        let mut processed_clauses = HashMap::new();

        while let Some((clause_id, upstream_cleared)) = pending_clauses.pop_front() {
            match processed_clauses.get(&clause_id) {
                Some(true) => continue,
                Some(false) if !upstream_cleared => continue,
                _ => {
                    processed_clauses.insert(clause_id, upstream_cleared);
                }
            }

            if !upstream_cleared
                && self.clauses[clause_id]
                    .slots
                    .iter()
                    .any(|slot_num| empty_slots.contains(slot_num))
            {
                continue;
            }

            for slot_num in &self.clauses[clause_id].introduced_slots {
                if optional_slots.insert(*slot_num) {
                    for (downstream_id, downstream) in self.clauses.iter().enumerate() {
                        if downstream_id > clause_id
                            && downstream.optional
                            && downstream.slots.contains(slot_num)
                        {
                            pending_clauses.push_back((downstream_id, true));
                        }
                    }
                }
            }
        }

        optional_slots
    }
}

#[derive(Debug)]
pub struct MatchPathSlot {
    pub spec: SlotElementSpec,
    pub in_slots: Vec<usize>,
    pub out_slots: Vec<usize>,
    pub paths: HashSet<usize>,
    pub optional: bool,
    pub(crate) introduction_clause: usize,
}

#[derive(Debug)]
pub struct SlotElementSpec {
    pub annotation: Option<Arc<str>>,
    pub labels: Vec<Arc<str>>,
    pub predicates: Vec<Expression>,
}

impl SlotElementSpec {
    pub fn new(
        annotation: Option<Arc<str>>,
        labels: Vec<Arc<str>>,
        predicates: Vec<Expression>,
    ) -> SlotElementSpec {
        SlotElementSpec {
            annotation,
            labels,
            predicates,
        }
    }

    pub fn from_node_match(node_match: &ast::NodeMatch) -> SlotElementSpec {
        let annotation = &node_match.annotation.name;
        let labels = node_match.labels.clone();
        let predicates = node_match.property_predicates.clone();

        SlotElementSpec::new(annotation.clone(), labels, predicates)
    }

    pub fn from_relation_match(node_match: &ast::RelationMatch) -> SlotElementSpec {
        let annotation = &node_match.annotation.name;
        let labels = node_match.labels.clone();
        let predicates = node_match.property_predicates.clone();

        SlotElementSpec::new(annotation.clone(), labels, predicates)
    }
}
