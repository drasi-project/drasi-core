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
    sync::Arc,
    time::Duration,
};

use drasi_query_ast::{
    api::ScopedQuery,
    ast::{self, BinaryExpression, Expression, Literal, UnaryExpression},
};

use crate::{
    interface::QueryBuilderError, models::Element, path_solver::match_path::SlotElementSpec,
};

/// Limits apply to both snapshots of one source event, before any index writes.
/// Exhaustion returns an error, never an incomplete set of matches.
/// These bound matcher work and generated states, not allocations inside an index
/// implementation or the size of a source element's properties.
#[derive(Debug, Clone)]
pub struct VariableLengthMatchLimits {
    /// Largest allowed upper repetition bound (default 64).
    pub max_hops: usize,
    /// Relationship candidates read and traversal/completion states visited (default 1,000,000).
    pub max_work: u64,
    /// Total partial trail/binding states generated, including discarded states (default 100,000).
    pub max_states: u64,
    /// Unique matches buffered across old and new snapshots (default 10,000).
    pub max_matches: u64,
    /// Wall-clock preparation deadline; checked between index operations (default 5 seconds).
    pub max_duration: Duration,
}

impl Default for VariableLengthMatchLimits {
    fn default() -> Self {
        Self {
            max_hops: 64,
            max_work: 1_000_000,
            max_states: 100_000,
            max_matches: 10_000,
            max_duration: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct NodeId(pub usize);

#[derive(Debug)]
pub(super) struct NodeSpec {
    pub alias: Option<Arc<str>>,
    pub occurrences: Vec<SlotElementSpec>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Length {
    Single,
    Repeated { min: usize, max: usize },
}

impl Length {
    pub fn bounds(self) -> (usize, usize) {
        match self {
            Self::Single => (1, 1),
            Self::Repeated { min, max } => (min, max),
        }
    }
}

#[derive(Debug)]
pub(super) struct Segment {
    pub from: NodeId,
    pub to: NodeId,
    pub scope: usize,
    pub direction: ast::Direction,
    pub length: Length,
    pub spec: SlotElementSpec,
}

#[derive(Debug)]
pub(crate) struct VariableLengthMatchPlan {
    pub(super) nodes: Vec<NodeSpec>,
    pub(super) segments: Vec<Segment>,
    pub limits: VariableLengthMatchLimits,
}

impl VariableLengthMatchPlan {
    pub fn compile(
        parsed: &ScopedQuery,
        has_joins: bool,
        has_middleware: bool,
        limits: VariableLengthMatchLimits,
    ) -> Result<Option<Self>, QueryBuilderError> {
        let query = &parsed.query;
        if !query
            .parts
            .iter()
            .flat_map(|p| &p.match_clauses)
            .flat_map(|p| &p.path)
            .any(|(r, _)| r.variable_length.is_some())
        {
            return Ok(None);
        }
        if has_joins {
            return Err(unsupported("virtual joins are not supported"));
        }
        if has_middleware {
            return Err(unsupported("source middleware is not supported"));
        }
        if parsed.has_mutations {
            return Err(unsupported("SET and DELETE clauses are not supported"));
        }
        if query
            .parts
            .iter()
            .flat_map(|p| &p.match_clauses)
            .any(|m| m.optional)
        {
            return Err(unsupported(
                "OPTIONAL MATCH cannot be combined with repetition",
            ));
        }
        if query
            .parts
            .iter()
            .skip(1)
            .any(|p| !p.match_clauses.is_empty())
        {
            return Err(unsupported(
                "MATCH in later WITH/NEXT query parts is not supported",
            ));
        }
        let scopes = parsed.match_scopes.as_ref().ok_or_else(|| {
            unsupported("parser must provide MATCH scope metadata through parse_scoped")
        })?;
        if scopes.len() != query.parts.len() {
            return Err(unsupported("invalid parser MATCH scope metadata"));
        }
        for (part, ranges) in query.parts.iter().zip(scopes) {
            let mut end = 0;
            for range in ranges {
                if range.start != end
                    || range.end <= range.start
                    || range.end > part.match_clauses.len()
                {
                    return Err(unsupported("invalid parser MATCH scope metadata"));
                }
                end = range.end;
            }
            if end != part.match_clauses.len() {
                return Err(unsupported("incomplete parser MATCH scope metadata"));
            }
        }
        if limits.max_work == 0
            || limits.max_states == 0
            || limits.max_matches == 0
            || limits.max_duration.is_zero()
        {
            return Err(unsupported(
                "work, state, match, and duration limits must be positive",
            ));
        }
        let mut plan = Self {
            nodes: Vec::new(),
            segments: Vec::new(),
            limits,
        };
        let mut aliases = HashMap::new();
        let mut relationship_aliases = HashSet::new();
        for (scope, range) in scopes[0].iter().enumerate() {
            for clause in &query.parts[0].match_clauses[range.clone()] {
                let mut previous = plan.add_node(&clause.start, &mut aliases)?;
                for (relation, node) in &clause.path {
                    validate_predicates(&relation.property_predicates)?;
                    let next = plan.add_node(node, &mut aliases)?;
                    if let Some(alias) = &relation.annotation.name {
                        if !relationship_aliases.insert(alias.clone()) {
                            return Err(unsupported(&format!(
                                "relationship alias '{alias}' is used more than once"
                            )));
                        }
                    }
                    let length = match &relation.variable_length {
                        None => Length::Single,
                        Some(bounds) => {
                            let (min, max) = match (bounds.min_hops, bounds.max_hops) {
                                (Some(n), None) => (n, n),
                                (min, Some(max)) => (min.unwrap_or(1), max),
                                (None, None) => {
                                    return Err(unsupported(
                                        "unbounded repetition is not supported",
                                    ))
                                }
                            };
                            if min < 0 || max < 0 || max < min {
                                return Err(unsupported(
                                    "repetition bounds must be nonnegative and ordered",
                                ));
                            }
                            let min = usize::try_from(min)
                                .map_err(|_| unsupported("repetition bound is too large"))?;
                            let max = usize::try_from(max)
                                .map_err(|_| unsupported("repetition bound is too large"))?;
                            if max > plan.limits.max_hops {
                                return Err(unsupported(&format!(
                                    "upper repetition bound {max} exceeds max_hops {}",
                                    plan.limits.max_hops
                                )));
                            }
                            Length::Repeated { min, max }
                        }
                    };
                    plan.segments.push(Segment {
                        from: previous,
                        to: next,
                        scope,
                        direction: relation.direction,
                        length,
                        spec: SlotElementSpec::from_relation_match(relation),
                    });
                    previous = next;
                }
            }
        }
        for alias in relationship_aliases {
            if aliases.contains_key(&alias) {
                return Err(unsupported(&format!(
                    "alias '{alias}' names both a node and a relationship"
                )));
            }
        }
        let mut connected = HashSet::from([NodeId(0)]);
        loop {
            let count = connected.len();
            for segment in &plan.segments {
                if connected.contains(&segment.from) || connected.contains(&segment.to) {
                    connected.insert(segment.from);
                    connected.insert(segment.to);
                }
            }
            if count == connected.len() {
                break;
            }
        }
        if connected.len() != plan.nodes.len() {
            return Err(unsupported("disconnected MATCH patterns are not supported; connect them with shared node aliases"));
        }
        Ok(Some(plan))
    }

    fn add_node(
        &mut self,
        node: &ast::NodeMatch,
        aliases: &mut HashMap<Arc<str>, NodeId>,
    ) -> Result<NodeId, QueryBuilderError> {
        validate_predicates(&node.property_predicates)?;
        if let Some(id) = node
            .annotation
            .name
            .as_ref()
            .and_then(|name| aliases.get(name))
        {
            self.nodes[id.0]
                .occurrences
                .push(SlotElementSpec::from_node_match(node));
            return Ok(*id);
        }
        let id = NodeId(self.nodes.len());
        if let Some(name) = &node.annotation.name {
            aliases.insert(name.clone(), id);
        }
        self.nodes.push(NodeSpec {
            alias: node.annotation.name.clone(),
            occurrences: vec![SlotElementSpec::from_node_match(node)],
        });
        Ok(id)
    }

    pub fn affinity(&self, element: &Element) -> Vec<usize> {
        match element {
            Element::Node { .. } => Vec::new(),
            Element::Relation { .. } => self
                .segments
                .iter()
                .enumerate()
                .filter(|(_, segment)| {
                    segment.length.bounds().1 > 0 && labels_match(&segment.spec, element)
                })
                .map(|(slot, _)| slot)
                .collect(),
        }
    }
}

pub(super) fn labels_match(spec: &SlotElementSpec, element: &Element) -> bool {
    spec.labels.is_empty()
        || spec
            .labels
            .iter()
            .any(|label| element.get_metadata().labels.contains(label))
}

fn unsupported(message: &str) -> QueryBuilderError {
    QueryBuilderError::EvaluationError(crate::evaluation::EvaluationError::from(
        QueryExecutionError::InvalidVariableLengthMatch(message.to_owned()),
    ))
}

fn validate_predicates(predicates: &[Expression]) -> Result<(), QueryBuilderError> {
    for predicate in predicates {
        let supported = match predicate {
            Expression::BinaryExpression(BinaryExpression::Eq(left, right)) => {
                matches!(left.as_ref(), Expression::UnaryExpression(UnaryExpression::Property { name, .. }) if name.is_empty())
                    && constant(right)
            }
            _ => false,
        };
        if !supported {
            return Err(unsupported("inline predicates must be property maps with literal values; use WHERE for other predicates"));
        }
    }
    Ok(())
}

fn constant(expression: &Expression) -> bool {
    match expression {
        Expression::UnaryExpression(UnaryExpression::Literal(literal)) => literal_constant(literal),
        Expression::ListExpression(items) => items.elements.iter().all(constant),
        Expression::ObjectExpression(items) => items.elements.values().all(constant),
        _ => false,
    }
}

fn literal_constant(literal: &Literal) -> bool {
    match literal {
        Literal::Expression(expression) => constant(expression),
        Literal::Object(items) => items.iter().all(|(_, value)| literal_constant(value)),
        _ => true,
    }
}
