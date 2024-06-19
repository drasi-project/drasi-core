use drasi_query_ast::ast;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::evaluation::variable_value::VariableValue;
use crate::interface::QueryClock;
use crate::models::{Element, ElementTimestamp};
use crate::path_solver::solution::SolutionSignature;

pub type QueryVariables = BTreeMap<Box<str>, VariableValue>;

#[derive(Debug, Clone)]
pub enum SideEffects {
    Apply,
    RevertForUpdate,
    RevertForDelete,
    Snapshot,
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryPartEvaluationContext {
    Adding {
        after: QueryVariables,
    },
    Updating {
        before: QueryVariables,
        after: QueryVariables,
    },
    Removing {
        before: QueryVariables,
    },
    Aggregation {
        before: Option<QueryVariables>,
        after: QueryVariables,
        grouping_keys: Vec<String>,
        default_before: bool,
        default_after: bool,
    },
    Noop,
}

impl QueryPartEvaluationContext {}

#[derive(Debug, Clone)]
pub struct ExpressionEvaluationContext<'a> {
    variables: &'a QueryVariables,
    side_effects: SideEffects,
    output_grouping_key: Option<&'a Vec<ast::Expression>>,
    input_grouping_hash: u64,
    clock: Arc<dyn QueryClock>,
    solution_signature: Option<SolutionSignature>,
    anchor_element: Option<Arc<Element>>,
}

impl<'a> ExpressionEvaluationContext<'a> {
    pub fn new(
        variables: &'a QueryVariables,
        clock: Arc<dyn QueryClock>,
    ) -> ExpressionEvaluationContext<'a> {
        ExpressionEvaluationContext {
            variables,
            side_effects: SideEffects::Apply,
            output_grouping_key: None,
            input_grouping_hash: u64::default(),
            clock,
            solution_signature: None,
            anchor_element: None,
        }
    }

    pub fn from_before_change(
        variables: &'a QueryVariables,
        side_effect_directive: SideEffects,
        change_context: &ChangeContext,
    ) -> ExpressionEvaluationContext<'a> {
        ExpressionEvaluationContext {
            variables,
            side_effects: side_effect_directive,
            output_grouping_key: None,
            input_grouping_hash: change_context.before_grouping_hash,
            clock: change_context.before_clock.clone(),
            solution_signature: Some(change_context.solution_signature),
            anchor_element: change_context.before_anchor_element.clone(),
        }
    }

    pub fn from_after_change(
        variables: &'a QueryVariables,
        change_context: &ChangeContext,
    ) -> ExpressionEvaluationContext<'a> {
        ExpressionEvaluationContext {
            variables,
            side_effects: SideEffects::Apply,
            output_grouping_key: None,
            input_grouping_hash: change_context.after_grouping_hash,
            clock: change_context.after_clock.clone(),
            solution_signature: Some(change_context.solution_signature),
            anchor_element: change_context.after_anchor_element.clone(),
        }
    }

    pub fn replace_variables(&mut self, new_data: &'a QueryVariables) {
        self.variables = new_data;
    }

    pub fn get_variable(&self, name: Arc<str>) -> Option<&VariableValue> {
        self.variables.get(&name.to_string().into_boxed_str())
    }

    pub fn clone_variables(&self) -> QueryVariables {
        self.variables.clone()
    }

    pub fn set_side_effects(&mut self, directive: SideEffects) {
        self.side_effects = directive;
    }

    pub fn get_side_effects(&self) -> &SideEffects {
        &self.side_effects
    }

    pub fn set_output_grouping_key(&mut self, grouping_key: &'a Vec<ast::Expression>) {
        self.output_grouping_key = Some(grouping_key);
    }

    pub fn get_output_grouping_key(&self) -> Option<&Vec<ast::Expression>> {
        self.output_grouping_key
    }

    pub fn get_transaction_time(&self) -> ElementTimestamp {
        self.clock.get_transaction_time()
    }

    pub fn get_realtime(&self) -> ElementTimestamp {
        self.clock.get_realtime()
    }

    pub fn get_clock(&self) -> Arc<dyn QueryClock> {
        self.clock.clone()
    }

    pub fn get_solution_signature(&self) -> Option<SolutionSignature> {
        self.solution_signature
    }

    pub fn get_anchor_element(&self) -> Option<Arc<Element>> {
        self.anchor_element.clone()
    }

    pub fn get_input_grouping_hash(&self) -> u64 {
        self.input_grouping_hash
    }
}

#[derive(Debug, Clone)]
pub struct ChangeContext {
    pub solution_signature: SolutionSignature,
    pub before_anchor_element: Option<Arc<Element>>,
    pub after_anchor_element: Option<Arc<Element>>,
    pub before_clock: Arc<dyn QueryClock>,
    pub after_clock: Arc<dyn QueryClock>,
    pub is_future_reprocess: bool,
    pub before_grouping_hash: u64,
    pub after_grouping_hash: u64,
}
