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

use drasi_query_ast::ast::{Expression, NodeMatch, RelationMatch};

use crate::{evaluation::EvaluationError, interface::IndexJoin, models::QueryJoin};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use drasi_query_ast::ast;

#[derive(Debug)]
pub struct MatchPath {
    pub slots: Vec<MatchPathSlot>,
    pub(crate) optional_paths: HashSet<usize>,
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

    pub fn optional_paths(&self) -> &HashSet<usize> {
        &self.optional_paths
    }

    /// Prepare the part of synthetic-join configuration needed by an index.
    /// The complete path, including predicates and topology, stays with the evaluator.
    pub fn index_joins(&self, joins: &[Arc<QueryJoin>]) -> Vec<IndexJoin> {
        joins
            .iter()
            .filter_map(|join| {
                let slots: Vec<_> = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, path)| {
                        path.spec
                            .labels
                            .iter()
                            .any(|label| label.as_ref() == join.id)
                            .then_some(slot)
                    })
                    .collect();
                (!slots.is_empty()).then(|| IndexJoin {
                    join: join.clone(),
                    slots,
                })
            })
            .collect()
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

fn merge_node_match(
    mtch: &NodeMatch,
    slots: &mut Vec<MatchPathSlot>,
    alias_map: &mut HashMap<Arc<str>, usize>,
    path_index: usize,
    optional: bool,
) -> Result<usize, EvaluationError> {
    match &mtch.annotation.name {
        Some(alias) => {
            if let Some(slot_num) = alias_map.get(alias) {
                slots[*slot_num].optional = optional && slots[*slot_num].optional;
                slots[*slot_num].paths.insert(path_index);
                Ok(*slot_num)
            } else {
                slots.push(MatchPathSlot {
                    spec: SlotElementSpec::from_node_match(mtch),
                    in_slots: Vec::new(),
                    out_slots: Vec::new(),
                    optional,
                    paths: HashSet::from([path_index]),
                });
                alias_map.insert(alias.clone(), slots.len() - 1);
                Ok(slots.len() - 1)
            }
        }
        None => {
            slots.push(MatchPathSlot {
                spec: SlotElementSpec::from_node_match(mtch),
                in_slots: Vec::new(),
                out_slots: Vec::new(),
                optional,
                paths: HashSet::from([path_index]),
            });
            Ok(slots.len() - 1)
        }
    }
}

fn merge_relation_match(
    mtch: &RelationMatch,
    slots: &mut Vec<MatchPathSlot>,
    alias_map: &mut HashMap<Arc<str>, usize>,
    path_index: usize,
    optional: bool,
) -> Result<usize, EvaluationError> {
    match &mtch.annotation.name {
        Some(alias) => {
            if let Some(slot_num) = alias_map.get(alias) {
                slots[*slot_num].optional = optional && slots[*slot_num].optional;
                slots[*slot_num].paths.insert(path_index);
                Ok(*slot_num)
            } else {
                slots.push(MatchPathSlot {
                    spec: SlotElementSpec::from_relation_match(mtch),
                    in_slots: Vec::new(),
                    out_slots: Vec::new(),
                    optional,
                    paths: HashSet::from([path_index]),
                });
                alias_map.insert(alias.clone(), slots.len() - 1);
                Ok(slots.len() - 1)
            }
        }

        None => {
            slots.push(MatchPathSlot {
                spec: SlotElementSpec::from_relation_match(mtch),
                in_slots: Vec::new(),
                out_slots: Vec::new(),
                optional,
                paths: HashSet::from([path_index]),
            });
            Ok(slots.len() - 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{interface::index_joins_by_label, models::QueryJoinKey};

    fn slot(labels: &[&str], predicate: Expression) -> MatchPathSlot {
        MatchPathSlot {
            spec: SlotElementSpec::new(
                Some(Arc::from("kept")),
                labels.iter().map(|label| Arc::from(*label)).collect(),
                vec![predicate],
            ),
            in_slots: vec![1],
            out_slots: vec![2],
            paths: HashSet::from([0, 1]),
            optional: true,
        }
    }

    #[test]
    fn index_joins_are_portable_without_changing_the_evaluator_path() {
        let predicate =
            Expression::UnaryExpression(ast::UnaryExpression::Literal(ast::Literal::Boolean(true)));
        let path = MatchPath {
            slots: vec![
                slot(&["REL", "other"], predicate.clone()),
                slot(&["unrelated"], predicate.clone()),
                slot(&["REL"], predicate.clone()),
                slot(&["OTHER"], predicate.clone()),
            ],
            optional_paths: HashSet::from([1]),
        };
        let key = |label: &str, property: &str| QueryJoinKey {
            label: label.to_string(),
            property: property.to_string(),
        };
        let first = Arc::new(QueryJoin {
            id: "REL".into(),
            keys: vec![key("Person", "a"), key("Company", "b"), key("Person", "c")],
        });
        let second = Arc::new(QueryJoin {
            id: "OTHER".into(),
            keys: vec![key("Person", "d")],
        });
        let unused = Arc::new(QueryJoin {
            id: "unused".into(),
            keys: vec![key("Person", "e")],
        });

        let specs = path.index_joins(&[first.clone(), unused, second.clone()]);
        assert_eq!(
            specs,
            vec![
                IndexJoin {
                    join: first.clone(),
                    slots: vec![0, 2],
                },
                IndexJoin {
                    join: second.clone(),
                    slots: vec![3],
                },
            ]
        );
        let encoded = rmp_serde::to_vec(&specs).unwrap();
        let decoded: Vec<IndexJoin> = rmp_serde::from_slice(&encoded).unwrap();
        assert_eq!(decoded, specs);

        let by_label = index_joins_by_label(&decoded).unwrap();
        assert_eq!(
            by_label["Person"],
            vec![
                (first.clone(), vec![0, 2]),
                (first.clone(), vec![0, 2]),
                (second, vec![3]),
            ]
        );
        assert_eq!(by_label["Company"], vec![(first, vec![0, 2])]);
        assert_eq!(path.slots[0].spec.predicates, vec![predicate]);
        assert_eq!(path.slots[0].in_slots, vec![1]);
        assert_eq!(path.slots[0].out_slots, vec![2]);
        assert_eq!(path.slots[0].paths, HashSet::from([0, 1]));
        assert!(path.slots[0].optional);
        assert_eq!(path.optional_paths(), &HashSet::from([1]));
    }

    #[test]
    fn indexed_join_requires_a_matching_slot() {
        let invalid = IndexJoin {
            join: Arc::new(QueryJoin {
                id: "REL".into(),
                keys: Vec::new(),
            }),
            slots: Vec::new(),
        };
        assert!(index_joins_by_label(&[invalid]).is_err());
    }
}
