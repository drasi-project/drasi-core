#![allow(clippy::unwrap_used)]
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
#![allow(dead_code)]
pub mod match_path;
pub mod solution;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use async_stream::stream;
use drasi_query_ast::ast::{NodeMatch, RelationMatch};
use futures::{stream::StreamExt, Stream};
#[allow(unused_imports)]
use tokio::{
    sync::Semaphore,
    task::{JoinError, JoinHandle},
};

use self::solution::MatchPathSolution;
use crate::{
    evaluation::{context::QueryVariables, EvaluationError},
    interface::{ElementIndex, ElementResult, ElementStream, IndexError, QueryClock},
    models::Element,
};

#[cfg(feature = "parallel_solver")]
const MAX_CONCURRENT_SOLUTIONS: usize = 10;

#[derive(Debug, Clone)]
pub struct MatchSolveContext<'a> {
    pub variables: &'a QueryVariables,
    pub clock: Arc<dyn QueryClock>,
}

impl<'a> MatchSolveContext<'a> {
    pub fn new(variables: &'a QueryVariables, clock: Arc<dyn QueryClock>) -> MatchSolveContext<'a> {
        MatchSolveContext { variables, clock }
    }
}

enum SolveDirection {
    Outward,
    Inward,
}

pub struct MatchPathSolver {
    element_index: Arc<dyn ElementIndex>,
}

impl MatchPathSolver {
    pub fn new(element_index: Arc<dyn ElementIndex>) -> MatchPathSolver {
        MatchPathSolver { element_index }
    }

    #[tracing::instrument(skip_all, err)]
    pub async fn solve(
        &self,
        path: Arc<match_path::MatchPath>,
        anchor_element: Arc<Element>,
        anchor_slot: usize,
    ) -> Result<HashMap<u64, solution::MatchPathSolution>, EvaluationError> {
        let total_slots = path.slots.len();
        let mut start_solution = MatchPathSolution::new(total_slots, anchor_slot);
        start_solution.enqueue_slot(anchor_slot, Some(anchor_element));

        let sol_stream =
            create_solution_stream(start_solution, path.clone(), self.element_index.clone()).await;

        let mut result = HashMap::new();
        tokio::pin!(sol_stream);

        while let Some(o) = sol_stream.next().await {
            match o {
                Ok((hash, solution)) => {
                    result.insert(hash, solution);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(result)
    }
}

#[allow(dead_code)]
enum SolutionStreamCommand {
    Partial(MatchPathSolution),
    Complete((u64, MatchPathSolution)),
    Error(EvaluationError),
    Panic(JoinError),
    Unsolvable,
}

async fn create_solution_stream(
    initial_sol: MatchPathSolution,
    path: Arc<match_path::MatchPath>,
    element_index: Arc<dyn ElementIndex>,
) -> impl Stream<Item = Result<(u64, MatchPathSolution), EvaluationError>> {
    #[cfg(feature = "parallel_solver")]
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_SOLUTIONS));

    stream! {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SolutionStreamCommand>();
        cmd_tx.send(SolutionStreamCommand::Partial(initial_sol)).unwrap();
        let mut inflight = 0;

        #[cfg(feature = "parallel_solver")]
        let (task_tx, mut task_rx) = tokio::sync::mpsc::unbounded_channel::<JoinHandle<()>>();

        #[cfg(feature = "parallel_solver")]
        {
            let cmd_tx2 = cmd_tx.clone();
            tokio::spawn(async move {
                while let Some(task) = task_rx.recv().await {
                    if let Err(err) = task.await {
                        cmd_tx2.send(SolutionStreamCommand::Panic(err)).unwrap();
                    }
                }
            });
        }

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                SolutionStreamCommand::Partial(solution) => {
                    inflight += 1;
                    let path = path.clone();
                    let element_index = element_index.clone();
                    let cmd_tx = cmd_tx.clone();

                    #[cfg(not(feature = "parallel_solver"))]
                    try_complete_solution(solution, path, element_index, cmd_tx).await;

                    #[cfg(feature = "parallel_solver")]
                    {
                        let permits = permits.clone();
                        let task = tokio::spawn(async move {
                            let _permit = permits.acquire().await.unwrap();
                            try_complete_solution(solution, path, element_index, cmd_tx).await;
                        });
                        task_tx.send(task).unwrap();
                    }
                },
                SolutionStreamCommand::Complete((hash, solution)) => {
                    inflight -= 1;
                    yield Ok((hash, solution));
                },
                SolutionStreamCommand::Error(e) => {
                    yield Err(e);
                    break;
                },
                SolutionStreamCommand::Panic(e) => {
                    panic!("Error in solution task: {:?}", e);
                },
                SolutionStreamCommand::Unsolvable => {
                    inflight -= 1;
                },
            }

            if inflight == 0 {
                break;
            }
        }
    }
}

#[tracing::instrument(skip_all)]
async fn try_complete_solution(
    mut solution: MatchPathSolution,
    path: Arc<match_path::MatchPath>,
    element_index: Arc<dyn ElementIndex>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<SolutionStreamCommand>,
) {
    while let Some((slot_num, element)) = solution.slot_cursors.pop_front() {
        solution.mark_slot_solved(slot_num, element.clone());

        if let Some(hash) = solution.get_solution_signature() {
            cmd_tx
                .send(SolutionStreamCommand::Complete((hash, solution)))
                .unwrap();
            return;
        }

        let slot = &path.slots[slot_num];
        let mut alt_by_slot = HashMap::new();

        for out_slot in &slot.out_slots {
            if solution.is_slot_solved(*out_slot) {
                continue;
            }

            let adjacent_elements = alt_by_slot.entry(*out_slot).or_insert_with(Vec::new);
            let mut found_adjacent = false;

            if let Some(element) = &element {
                let mut adjacent_stream = match get_adjacent_elements(
                    element_index.clone(),
                    element.clone(),
                    *out_slot,
                    SolveDirection::Outward,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        cmd_tx.send(SolutionStreamCommand::Error(e.into())).unwrap();
                        return;
                    }
                };

                while let Some(adjacent_element) = adjacent_stream.next().await {
                    found_adjacent = true;
                    match adjacent_element {
                        Ok(adjacent_element) => adjacent_elements.push(Some(adjacent_element)),
                        Err(e) => {
                            cmd_tx.send(SolutionStreamCommand::Error(e.into())).unwrap();
                            return;
                        }
                    }
                }
            }

            if path.slots[*out_slot].optional && !found_adjacent {
                adjacent_elements.push(None);
            }
        }

        for in_slot in &slot.in_slots {
            if solution.is_slot_solved(*in_slot) {
                continue;
            }

            let adjacent_elements = alt_by_slot.entry(*in_slot).or_insert_with(Vec::new);
            let mut found_adjacent = false;

            if let Some(element) = &element {
                let mut adjacent_stream = match get_adjacent_elements(
                    element_index.clone(),
                    element.clone(),
                    *in_slot,
                    SolveDirection::Inward,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        cmd_tx.send(SolutionStreamCommand::Error(e.into())).unwrap();
                        return;
                    }
                };

                while let Some(adjacent_element) = adjacent_stream.next().await {
                    found_adjacent = true;
                    match adjacent_element {
                        Ok(adjacent_element) => adjacent_elements.push(Some(adjacent_element)),
                        Err(e) => {
                            cmd_tx.send(SolutionStreamCommand::Error(e.into())).unwrap();
                            return;
                        }
                    }
                }
            }

            if path.slots[*in_slot].optional && !found_adjacent {
                adjacent_elements.push(None);
            }
        }

        let mut pointers = BTreeMap::new();

        for (slot, adjacent_elements) in &mut alt_by_slot {
            match adjacent_elements.len() {
                0 => {}
                1 => {
                    solution.enqueue_slot(*slot, adjacent_elements.pop().unwrap());
                }
                _ => {
                    pointers.insert(*slot, 0);
                }
            }
        }

        if pointers.is_empty() {
            continue;
        }

        let mut permutations = vec![pointers];

        while let Some(mut p) = permutations.pop() {
            let mut alt_solution = solution.clone();

            for (slot, pointer) in &p {
                if let Some(adjacent_element) = alt_by_slot.get(slot).unwrap().get(*pointer) {
                    alt_solution.enqueue_slot(*slot, adjacent_element.clone());
                }
            }

            cmd_tx
                .send(SolutionStreamCommand::Partial(alt_solution))
                .unwrap();

            for (slot, pointer) in &mut p {
                if *pointer < alt_by_slot.get(slot).unwrap().len() - 1 {
                    *pointer += 1;
                    permutations.push(p);
                    break;
                } else {
                    *pointer = 0;
                }
            }
        }
    }
    cmd_tx.send(SolutionStreamCommand::Unsolvable).unwrap();
}

#[tracing::instrument(skip_all, err)]
async fn get_adjacent_elements(
    element_index: Arc<dyn ElementIndex>,
    element: Arc<Element>,
    target_slot: usize,
    direction: SolveDirection,
) -> Result<ElementStream, IndexError> {
    match element.as_ref() {
        Element::Node {
            metadata,
            properties: _,
        } => match direction {
            SolveDirection::Outward => Ok(element_index
                .get_slot_elements_by_inbound(target_slot, &metadata.reference)
                .await?),
            SolveDirection::Inward => Ok(element_index
                .get_slot_elements_by_outbound(target_slot, &metadata.reference)
                .await?),
        },
        Element::Relation {
            metadata: _,
            in_node,
            out_node,
            properties: _,
        } => {
            let adjecent_ref = match direction {
                SolveDirection::Outward => out_node,
                SolveDirection::Inward => in_node,
            };

            match element_index
                .get_slot_element_by_ref(target_slot, adjecent_ref)
                .await?
            {
                Some(adjacent_element) => Ok(Box::pin(tokio_stream::once::<ElementResult>(Ok(
                    adjacent_element,
                )))),
                None => Ok(Box::pin(tokio_stream::empty::<ElementResult>())),
            }
        }
    }
}

fn merge_node_match<'b>(
    mtch: &NodeMatch,
    slots: &'b mut Vec<match_path::MatchPathSlot>,
    alias_map: &'b mut HashMap<Arc<str>, usize>,
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
                slots.push(match_path::MatchPathSlot {
                    spec: match_path::SlotElementSpec::from_node_match(mtch),
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
            slots.push(match_path::MatchPathSlot {
                spec: match_path::SlotElementSpec::from_node_match(mtch),
                in_slots: Vec::new(),
                out_slots: Vec::new(),
                optional,
                paths: HashSet::from([path_index]),
            });
            Ok(slots.len() - 1)
        }
    }
}

fn merge_relation_match<'b>(
    mtch: &RelationMatch,
    slots: &'b mut Vec<match_path::MatchPathSlot>,
    alias_map: &'b mut HashMap<Arc<str>, usize>,
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
                slots.push(match_path::MatchPathSlot {
                    spec: match_path::SlotElementSpec::from_relation_match(mtch),
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
            slots.push(match_path::MatchPathSlot {
                spec: match_path::SlotElementSpec::from_relation_match(mtch),
                in_slots: Vec::new(),
                out_slots: Vec::new(),
                optional,
                paths: HashSet::from([path_index]),
            });
            Ok(slots.len() - 1)
        }
    }
}
