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
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use drasi_query_ast::ast;

#[derive(Debug)]
pub struct MatchPath {
    pub slots: Vec<MatchPathSlot>,
    optional_paths: HashSet<usize>,
}

impl MatchPath {
    pub fn from_query(query_part: &ast::QueryPart) -> Result<Self, EvaluationError> {
        let mut slots = Vec::new();

        let mut alias_map = HashMap::new();
        let mut optional_paths = HashSet::new();

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
        })
    }

    pub fn get_optional_slots_on_common_paths(
        &self,
        anchor_slot_num: usize,
        empty_slots: HashSet<usize>,
    ) -> HashSet<usize> {
        let mut optional_slots = HashSet::new();
        for path in &self.slots[anchor_slot_num].paths {
            let mut has_empty_slots = false;
            let mut path_slots = HashSet::new();

            for (slot_num, slot) in self.slots.iter().enumerate() {
                if slot.optional && slot.paths.contains(path) {
                    if empty_slots.contains(&slot_num) {
                        has_empty_slots = true;
                        break;
                    }

                    if slot_num != anchor_slot_num && slot.paths.len() > 1 {
                        continue;
                    }

                    path_slots.insert(slot_num);
                }
            }
            if !has_empty_slots {
                optional_slots.extend(path_slots);
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
