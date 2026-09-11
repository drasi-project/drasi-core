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

use crate::evaluation::QueryExecutionError;

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
    time::Instant,
};

use drasi_query_ast::ast::Direction;
use futures::StreamExt;
use hashers::jenkins::spooky_hash::SpookyHasher;

use crate::{
    evaluation::{
        context::QueryVariables, variable_value::VariableValue, EvaluationError,
        ExpressionEvaluationContext, ExpressionEvaluator,
    },
    interface::{ElementIndex, QueryClock},
    models::{Element, ElementReference},
    path_solver::match_path::SlotElementSpec,
};

use super::{
    plan::{labels_match, Length, NodeId},
    VariableLengthMatchLimits, VariableLengthMatchPlan,
};

pub(crate) struct WorkBudget {
    limits: VariableLengthMatchLimits,
    started: Instant,
    work: u64,
    states: u64,
    matches: u64,
}

impl WorkBudget {
    pub fn new(limits: &VariableLengthMatchLimits) -> Self {
        Self {
            limits: limits.clone(),
            started: Instant::now(),
            work: 0,
            states: 0,
            matches: 0,
        }
    }

    pub fn check(&self) -> Result<(), EvaluationError> {
        if self.started.elapsed() > self.limits.max_duration {
            return Err(EvaluationError::from(
                QueryExecutionError::MatchResourceLimit {
                    resource: "preparation milliseconds",
                    limit: u64::try_from(self.limits.max_duration.as_millis()).unwrap_or(u64::MAX),
                },
            ));
        }
        Ok(())
    }

    fn work(&mut self) -> Result<(), EvaluationError> {
        self.check()?;
        charge(&mut self.work, self.limits.max_work, "work")
    }

    fn state(&mut self) -> Result<(), EvaluationError> {
        self.work()?;
        charge(&mut self.states, self.limits.max_states, "generated states")
    }

    fn matched(&mut self) -> Result<(), EvaluationError> {
        self.check()?;
        charge(
            &mut self.matches,
            self.limits.max_matches,
            "buffered matches",
        )
    }
}

fn charge(counter: &mut u64, limit: u64, resource: &'static str) -> Result<(), EvaluationError> {
    if *counter >= limit {
        return Err(EvaluationError::from(
            QueryExecutionError::MatchResourceLimit { resource, limit },
        ));
    }
    *counter += 1;
    Ok(())
}

/// A single-event read overlay. The backing index is never changed during solving.
pub(crate) struct GraphView<'a> {
    index: &'a dyn ElementIndex,
    replacement: Option<(&'a ElementReference, Arc<Element>)>,
}

impl<'a> GraphView<'a> {
    pub fn current(index: &'a dyn ElementIndex) -> Self {
        Self {
            index,
            replacement: None,
        }
    }

    pub fn changed(
        index: &'a dyn ElementIndex,
        reference: &'a ElementReference,
        element: Arc<Element>,
    ) -> Self {
        Self {
            index,
            replacement: Some((reference, element)),
        }
    }

    async fn node(
        &self,
        reference: &ElementReference,
        budget: &mut WorkBudget,
    ) -> Result<Option<Arc<Element>>, EvaluationError> {
        budget.work()?;
        let element = match &self.replacement {
            Some((changed, value)) if *changed == reference => Some(value.clone()),
            _ => self.index.get_element(reference).await?,
        };
        budget.check()?;
        Ok(element.filter(|element| matches!(element.as_ref(), Element::Node { .. })))
    }

    async fn edges(
        &self,
        plan: &VariableLengthMatchPlan,
        segment: usize,
        node: &ElementReference,
        forward: bool,
        budget: &mut WorkBudget,
    ) -> Result<Vec<Arc<Element>>, EvaluationError> {
        let direction = plan.segments[segment].direction;
        let inbound = direction == Direction::Either || (direction == Direction::Right) == forward;
        let outbound = direction == Direction::Either || !inbound;
        let mut edges = Vec::new();
        let mut seen = HashSet::new();
        for (enabled, by_inbound) in [(inbound, true), (outbound, false)] {
            if !enabled {
                continue;
            }
            budget.work()?;
            let mut stream = if by_inbound {
                self.index
                    .get_slot_elements_by_inbound(segment, node)
                    .await?
            } else {
                self.index
                    .get_slot_elements_by_outbound(segment, node)
                    .await?
            };
            while let Some(edge) = stream.next().await {
                budget.work()?;
                let edge = edge?;
                if self
                    .replacement
                    .as_ref()
                    .is_some_and(|(reference, _)| *reference == edge.get_reference())
                {
                    continue;
                }
                if seen.insert(edge.get_reference().clone()) {
                    budget.state()?;
                    edges.push(edge);
                }
            }
        }
        if let Some((_, edge)) = &self.replacement {
            if let Element::Relation {
                in_node, out_node, ..
            } = edge.as_ref()
            {
                if ((inbound && in_node == node) || (outbound && out_node == node))
                    && labels_match(&plan.segments[segment].spec, edge)
                {
                    budget.state()?;
                    edges.push(edge.clone());
                }
            }
        }
        budget.check()?;
        Ok(edges)
    }
}

#[derive(Clone)]
struct Trail {
    nodes: Vec<Arc<Element>>,
    relationships: Vec<Arc<Element>>,
}

impl Trail {
    fn start(node: Arc<Element>) -> Self {
        Self {
            nodes: vec![node],
            relationships: Vec::new(),
        }
    }

    fn last(&self) -> &Arc<Element> {
        &self.nodes[self.relationships.len()]
    }

    fn reverse(mut self) -> Self {
        self.nodes.reverse();
        self.relationships.reverse();
        self
    }

    fn contains_relationship(&self, reference: &ElementReference) -> bool {
        self.relationships
            .iter()
            .any(|edge| edge.get_reference() == reference)
    }

    fn key(&self) -> TrailKey {
        TrailKey {
            nodes: self
                .nodes
                .iter()
                .map(|node| node.get_reference().clone())
                .collect(),
            relationships: self
                .relationships
                .iter()
                .map(|edge| edge.get_reference().clone())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TrailKey {
    nodes: Vec<ElementReference>,
    relationships: Vec<ElementReference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct VariableLengthMatchKey {
    nodes: Vec<ElementReference>,
    segments: Vec<TrailKey>,
}

impl VariableLengthMatchKey {
    pub fn temporal_identity(&self) -> crate::evaluation::temporal::MatchIdentity {
        use crate::evaluation::temporal::{MatchIdentity, PathIdentity};
        MatchIdentity::Bounded {
            nodes: self.nodes.clone(),
            paths: self
                .segments
                .iter()
                .map(|segment| PathIdentity {
                    nodes: segment.nodes.clone(),
                    relationships: segment.relationships.clone(),
                })
                .collect(),
        }
    }

    pub fn signature(&self) -> u64 {
        let mut hasher = SpookyHasher::default();
        // Keep the hash domain stable across internal matcher renames.
        "native-bounded-match-v1".hash(&mut hasher);
        self.hash(&mut hasher);
        hasher.finish()
    }
}

pub(crate) struct VariableLengthSolution {
    nodes: Vec<Arc<Element>>,
    segments: Vec<Trail>,
}

impl VariableLengthSolution {
    fn key(&self) -> VariableLengthMatchKey {
        VariableLengthMatchKey {
            nodes: self
                .nodes
                .iter()
                .map(|node| node.get_reference().clone())
                .collect(),
            segments: self.segments.iter().map(Trail::key).collect(),
        }
    }

    pub fn variables(
        &self,
        plan: &VariableLengthMatchPlan,
        base: &QueryVariables,
    ) -> QueryVariables {
        let mut variables = base.clone();
        for (spec, node) in plan.nodes.iter().zip(&self.nodes) {
            if let Some(alias) = &spec.alias {
                variables.insert(
                    alias.to_string().into_boxed_str(),
                    VariableValue::Element(node.clone()),
                );
            }
        }
        for (spec, trail) in plan.segments.iter().zip(&self.segments) {
            if let Some(alias) = &spec.spec.annotation {
                let value = match spec.length {
                    Length::Single => VariableValue::Element(trail.relationships[0].clone()),
                    Length::Repeated { .. } => VariableValue::List(
                        trail
                            .relationships
                            .iter()
                            .cloned()
                            .map(VariableValue::Element)
                            .collect(),
                    ),
                };
                variables.insert(alias.to_string().into_boxed_str(), value);
            }
        }
        variables
    }
}

#[derive(Clone)]
struct PartialSolution {
    nodes: Vec<Option<Arc<Element>>>,
    segments: Vec<Option<Trail>>,
    used: HashMap<usize, HashSet<ElementReference>>,
}

impl PartialSolution {
    fn new(plan: &VariableLengthMatchPlan) -> Self {
        Self {
            nodes: vec![None; plan.nodes.len()],
            segments: vec![None; plan.segments.len()],
            used: HashMap::new(),
        }
    }
}

pub(crate) struct VariableLengthSolver<'a> {
    pub plan: &'a VariableLengthMatchPlan,
    pub graph: GraphView<'a>,
    pub evaluator: &'a ExpressionEvaluator,
    pub clock: Arc<dyn QueryClock>,
}

impl VariableLengthSolver<'_> {
    pub async fn affected(
        &self,
        anchor: Arc<Element>,
        budget: &mut WorkBudget,
    ) -> Result<HashMap<VariableLengthMatchKey, VariableLengthSolution>, EvaluationError> {
        let mut results = HashMap::new();
        for segment in 0..self.plan.segments.len() {
            budget.work()?;
            let (_, max) = self.plan.segments[segment].length.bounds();
            match anchor.as_ref() {
                Element::Node { .. } => {
                    let prefixes = self
                        .walks(segment, anchor.clone(), false, max, &HashSet::new(), budget)
                        .await?;
                    for prefix in prefixes {
                        let prefix = prefix.reverse();
                        if !self
                            .node_matches(self.plan.segments[segment].from, &prefix.nodes[0])
                            .await?
                        {
                            continue;
                        }
                        self.finish_anchor(segment, prefix, &mut results, budget)
                            .await?;
                    }
                }
                Element::Relation {
                    in_node, out_node, ..
                } => {
                    if max == 0
                        || !self
                            .matches(&self.plan.segments[segment].spec, &anchor)
                            .await?
                    {
                        continue;
                    }
                    let mut orientations = Vec::new();
                    let direction = self.plan.segments[segment].direction;
                    if direction != Direction::Left {
                        orientations.push((in_node, out_node));
                    }
                    if direction != Direction::Right
                        && (direction != Direction::Either || in_node != out_node)
                    {
                        orientations.push((out_node, in_node));
                    }
                    for (from, to) in orientations {
                        let (Some(from), Some(to)) = (
                            self.graph.node(from, budget).await?,
                            self.graph.node(to, budget).await?,
                        ) else {
                            continue;
                        };
                        let excluded = HashSet::from([anchor.get_reference().clone()]);
                        let prefixes = self
                            .walks(segment, from, false, max - 1, &excluded, budget)
                            .await?;
                        for prefix in prefixes {
                            let mut prefix = prefix.reverse();
                            if !self
                                .node_matches(self.plan.segments[segment].from, &prefix.nodes[0])
                                .await?
                            {
                                continue;
                            }
                            prefix.relationships.push(anchor.clone());
                            prefix.nodes.push(to.clone());
                            self.finish_anchor(segment, prefix, &mut results, budget)
                                .await?;
                        }
                    }
                }
            }
        }
        budget.check()?;
        Ok(results)
    }

    async fn finish_anchor(
        &self,
        segment: usize,
        prefix: Trail,
        results: &mut HashMap<VariableLengthMatchKey, VariableLengthSolution>,
        budget: &mut WorkBudget,
    ) -> Result<(), EvaluationError> {
        let (min, max) = self.plan.segments[segment].length.bounds();
        let excluded = prefix
            .relationships
            .iter()
            .map(|edge| edge.get_reference().clone())
            .collect();
        let suffixes = self
            .walks(
                segment,
                prefix.last().clone(),
                true,
                max - prefix.relationships.len(),
                &excluded,
                budget,
            )
            .await?;
        for suffix in suffixes {
            if prefix.relationships.len() + suffix.relationships.len() < min {
                continue;
            }
            budget.state()?;
            let mut trail = prefix.clone();
            trail.relationships.extend(suffix.relationships);
            trail.nodes.extend(suffix.nodes.into_iter().skip(1));
            let mut partial = PartialSolution::new(self.plan);
            if self.bind(&mut partial, segment, trail).await? {
                self.complete(partial, results, budget).await?;
            }
        }
        Ok(())
    }

    async fn complete(
        &self,
        initial: PartialSolution,
        results: &mut HashMap<VariableLengthMatchKey, VariableLengthSolution>,
        budget: &mut WorkBudget,
    ) -> Result<(), EvaluationError> {
        let mut pending = vec![initial];
        while let Some(partial) = pending.pop() {
            budget.work()?;
            let frontier = self.plan.segments.iter().enumerate().find(|(id, segment)| {
                partial.segments[*id].is_none()
                    && (partial.nodes[segment.from.0].is_some()
                        || partial.nodes[segment.to.0].is_some())
            });
            let Some((id, segment)) = frontier else {
                let solution = VariableLengthSolution {
                    nodes: partial
                        .nodes
                        .into_iter()
                        .collect::<Option<Vec<_>>>()
                        .ok_or(EvaluationError::CorruptData)?,
                    segments: partial
                        .segments
                        .into_iter()
                        .collect::<Option<Vec<_>>>()
                        .ok_or(EvaluationError::CorruptData)?,
                };
                let key = solution.key();
                if let std::collections::hash_map::Entry::Vacant(entry) = results.entry(key) {
                    budget.matched()?;
                    entry.insert(solution);
                }
                continue;
            };
            let (start, forward) = match &partial.nodes[segment.from.0] {
                Some(start) => (start.clone(), true),
                None => (
                    partial.nodes[segment.to.0]
                        .clone()
                        .ok_or(EvaluationError::CorruptData)?,
                    false,
                ),
            };
            let (min, max) = segment.length.bounds();
            let empty = HashSet::new();
            let excluded = partial.used.get(&segment.scope).unwrap_or(&empty);
            let trails = self
                .walks(id, start, forward, max, excluded, budget)
                .await?;
            for trail in trails {
                if trail.relationships.len() < min {
                    continue;
                }
                let trail = if forward { trail } else { trail.reverse() };
                budget.state()?;
                let mut next = partial.clone();
                if self.bind(&mut next, id, trail).await? {
                    pending.push(next);
                }
            }
        }
        Ok(())
    }

    async fn bind(
        &self,
        partial: &mut PartialSolution,
        id: usize,
        trail: Trail,
    ) -> Result<bool, EvaluationError> {
        let segment = &self.plan.segments[id];
        for (node_id, node) in [(segment.from, &trail.nodes[0]), (segment.to, trail.last())] {
            if let Some(bound) = &partial.nodes[node_id.0] {
                if bound.get_reference() != node.get_reference() {
                    return Ok(false);
                }
            }
            if !self.node_matches(node_id, node).await? {
                return Ok(false);
            }
            partial.nodes[node_id.0] = Some(node.clone());
        }
        let used = partial.used.entry(segment.scope).or_default();
        for edge in &trail.relationships {
            if !used.insert(edge.get_reference().clone()) {
                return Ok(false);
            }
        }
        partial.segments[id] = Some(trail);
        Ok(true)
    }

    async fn walks(
        &self,
        segment: usize,
        start: Arc<Element>,
        forward: bool,
        max: usize,
        excluded: &HashSet<ElementReference>,
        budget: &mut WorkBudget,
    ) -> Result<Vec<Trail>, EvaluationError> {
        budget.state()?;
        let mut pending = vec![Trail::start(start)];
        let mut trails = Vec::new();
        while let Some(trail) = pending.pop() {
            budget.work()?;
            if trail.relationships.len() < max {
                let node = trail.last().get_reference();
                for edge in self
                    .graph
                    .edges(self.plan, segment, node, forward, budget)
                    .await?
                {
                    if excluded.contains(edge.get_reference())
                        || trail.contains_relationship(edge.get_reference())
                    {
                        continue;
                    }
                    let Element::Relation {
                        in_node, out_node, ..
                    } = edge.as_ref()
                    else {
                        continue;
                    };
                    let direction = self.plan.segments[segment].direction;
                    let next = if direction == Direction::Either {
                        if node == in_node {
                            out_node
                        } else if node == out_node {
                            in_node
                        } else {
                            continue;
                        }
                    } else if (direction == Direction::Right) == forward {
                        if node != in_node {
                            continue;
                        }
                        out_node
                    } else {
                        if node != out_node {
                            continue;
                        }
                        in_node
                    };
                    if !self
                        .matches(&self.plan.segments[segment].spec, &edge)
                        .await?
                    {
                        continue;
                    }
                    let Some(next) = self.graph.node(next, budget).await? else {
                        continue;
                    };
                    budget.state()?;
                    let mut extension = trail.clone();
                    extension.relationships.push(edge);
                    extension.nodes.push(next);
                    pending.push(extension);
                }
            }
            budget.state()?;
            trails.push(trail);
        }
        Ok(trails)
    }

    async fn node_matches(&self, id: NodeId, node: &Arc<Element>) -> Result<bool, EvaluationError> {
        for occurrence in &self.plan.nodes[id.0].occurrences {
            if !self.matches(occurrence, node).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn matches(
        &self,
        spec: &SlotElementSpec,
        element: &Arc<Element>,
    ) -> Result<bool, EvaluationError> {
        if !labels_match(spec, element) {
            return Ok(false);
        }
        let variables =
            QueryVariables::from([(Box::from(""), VariableValue::Element(element.clone()))]);
        let context = ExpressionEvaluationContext::from_slot(
            &variables,
            self.clock.clone(),
            element.get_reference(),
        );
        for predicate in &spec.predicates {
            if !self
                .evaluator
                .evaluate_predicate(&context, predicate)
                .await?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
