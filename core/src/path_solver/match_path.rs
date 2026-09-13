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
    pub(crate) optional_paths: HashSet<usize>,
    pub(crate) segments: Vec<MatchPathSegment>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MatchPathSegment {
    pub(crate) start_slot: usize,
    pub(crate) relation_slot: usize,
    pub(crate) end_slot: usize,
    pub(crate) direction: ast::Direction,
}

impl MatchPathSegment {
    pub(crate) fn contains_slot(&self, slot_num: usize) -> bool {
        self.start_slot == slot_num || self.relation_slot == slot_num || self.end_slot == slot_num
    }
}

impl MatchPath {
    pub fn from_query(query_part: &ast::QueryPart) -> Result<Self, EvaluationError> {
        let mut slots = Vec::new();

        let mut alias_map = HashMap::new();
        let mut optional_paths = HashSet::new();
        let mut segments = Vec::new();

        for (path_index, mc) in query_part.match_clauses.iter().enumerate() {
            if mc.optional {
                optional_paths.insert(path_index);
            }
            let slot_num = merge_node_match(
                &mc.start,
                &mut slots,
                &mut alias_map,
                path_index,
                mc.optional,
            )?;
            let mut prev_slot_num = slot_num;

            for p in &mc.path {
                let rel_slot_num = merge_relation_match(
                    &p.0,
                    &mut slots,
                    &mut alias_map,
                    path_index,
                    mc.optional,
                )?;
                let node_slot_num =
                    merge_node_match(&p.1, &mut slots, &mut alias_map, path_index, mc.optional)?;
                segments.push(MatchPathSegment {
                    start_slot: prev_slot_num,
                    relation_slot: rel_slot_num,
                    end_slot: node_slot_num,
                    direction: p.0.direction,
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

        Ok(MatchPath {
            slots,
            optional_paths,
            segments,
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
        let mut pending_paths = self.slots[anchor_slot_num]
            .paths
            .iter()
            .filter(|path| self.optional_paths.contains(path))
            .map(|path| (*path, false))
            .collect::<VecDeque<_>>();
        let mut processed_paths = HashMap::new();

        while let Some((path, upstream_cleared)) = pending_paths.pop_front() {
            match processed_paths.get(&path) {
                Some(true) => continue,
                Some(false) if !upstream_cleared => continue,
                _ => {
                    processed_paths.insert(path, upstream_cleared);
                }
            }

            if !upstream_cleared
                && self.slots.iter().enumerate().any(|(slot_num, slot)| {
                    slot.paths.contains(&path) && empty_slots.contains(&slot_num)
                })
            {
                continue;
            }

            for (slot_num, slot) in self.slots.iter().enumerate() {
                if !slot.optional || !slot.paths.contains(&path) {
                    continue;
                }

                // Optional clauses are left-correlated in query order. Preserve slots
                // introduced by an earlier clause, but clear shared slots introduced
                // here and transitively default the later clauses that depend on them.
                let introduction_path = slot.paths.iter().min().copied();
                if introduction_path != Some(path) {
                    continue;
                }

                if optional_slots.insert(slot_num) {
                    for downstream_path in &slot.paths {
                        if *downstream_path > path && self.optional_paths.contains(downstream_path)
                        {
                            pending_paths.push_back((*downstream_path, true));
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
