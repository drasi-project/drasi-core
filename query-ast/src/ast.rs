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

use std::collections::BTreeMap;
use std::hash::Hasher;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub parts: Vec<QueryPart>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryPart {
    pub match_clauses: Vec<MatchClause>,
    pub where_clauses: Vec<Expression>,
    pub return_clause: ProjectionClause,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub start: NodeMatch,
    pub path: Vec<(RelationMatch, NodeMatch)>,
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionClause {
    Item(Vec<Expression>),
    GroupBy {
        grouping: Vec<Expression>,
        aggregates: Vec<Expression>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub name: Arc<str>,
    pub key: Arc<str>,
    pub value: Expression,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CreateClause {
    CreateNode {
        name: Option<Arc<str>>,
        label: Arc<str>,
        properties: Vec<(Arc<str>, Expression)>,
    },
    CreateEdge {
        name: Option<Arc<str>>,
        label: Arc<str>,
        origin: Arc<str>,
        target: Arc<str>,
        properties: Vec<(Arc<str>, Expression)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub name: Option<Arc<str>>,
}

impl Annotation {
    #[allow(dead_code)]
    pub fn new(name: Arc<str>) -> Self {
        Self { name: Some(name) }
    }

    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self { name: None }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeMatch {
    pub annotation: Annotation,
    pub labels: Vec<Arc<str>>,
    pub property_predicates: Vec<Expression>,
}

impl NodeMatch {
    #[allow(dead_code)]
    pub fn new(
        annotation: Annotation,
        labels: Vec<Arc<str>>,
        property_predicates: Vec<Expression>,
    ) -> Self {
        Self {
            annotation,
            labels,
            property_predicates,
        }
    }

    #[allow(dead_code)]
    pub fn empty() -> Self {
        Self {
            annotation: Annotation::empty(),
            labels: vec![],
            property_predicates: vec![],
        }
    }

    pub fn with_annotation(annotation: Annotation, label: Arc<str>) -> Self {
        Self {
            annotation,
            labels: vec![label],
            property_predicates: vec![],
        }
    }

    pub fn without_label(annotation: Annotation) -> Self {
        Self {
            annotation,
            labels: vec![],
            property_predicates: vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Either,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VariableLengthMatch {
    pub min_hops: Option<i64>,
    pub max_hops: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelationMatch {
    pub direction: Direction,
    pub annotation: Annotation,
    pub variable_length: Option<VariableLengthMatch>,
    pub labels: Vec<Arc<str>>,
    pub property_predicates: Vec<Expression>,
}

impl RelationMatch {
    pub fn either(
        annotation: Annotation,
        labels: Vec<Arc<str>>,
        property_predicates: Vec<Expression>,
        variable_length: Option<VariableLengthMatch>,
    ) -> Self {
        Self {
            direction: Direction::Either,
            annotation,
            labels,
            property_predicates,
            variable_length,
        }
    }

    pub fn left(
        annotation: Annotation,
        labels: Vec<Arc<str>>,
        property_predicates: Vec<Expression>,
        variable_length: Option<VariableLengthMatch>,
    ) -> Self {
        Self {
            direction: Direction::Left,
            annotation,
            labels,
            property_predicates,
            variable_length,
        }
    }

    pub fn right(
        annotation: Annotation,
        labels: Vec<Arc<str>>,
        property_predicates: Vec<Expression>,
        variable_length: Option<VariableLengthMatch>,
    ) -> Self {
        Self {
            direction: Direction::Right,
            annotation,
            labels,
            property_predicates,
            variable_length,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Real(f64),
    Boolean(bool),
    Text(Arc<str>),
    Date(Arc<str>),
    LocalTime(Arc<str>),
    ZonedTime(Arc<str>),
    LocalDateTime(Arc<str>),
    ZonedDateTime(Arc<str>),
    Duration(Arc<str>),
    Object(Vec<(Arc<str>, Literal)>),
    Expression(Box<Expression>),
    Null,
}

impl std::hash::Hash for Literal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Literal::Integer(v) => v.hash(state),
            Literal::Real(v) => v.to_bits().hash(state),
            Literal::Boolean(v) => v.hash(state),
            Literal::Text(v) => v.hash(state),
            Literal::Date(v) => v.hash(state),
            Literal::LocalTime(v) => v.hash(state),
            Literal::ZonedTime(v) => v.hash(state),
            Literal::LocalDateTime(v) => v.hash(state),
            Literal::ZonedDateTime(v) => v.hash(state),
            Literal::Duration(v) => v.hash(state),
            Literal::Object(v) => v.hash(state),
            Literal::Expression(v) => v.hash(state),
            Literal::Null => state.write_u8(0),
        }
    }
}

impl Eq for Literal {}

pub trait ParentExpression {
    fn get_children(&self) -> Vec<&Expression>;
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub enum Expression {
    UnaryExpression(UnaryExpression),
    BinaryExpression(BinaryExpression),
    FunctionExpression(FunctionExpression),
    CaseExpression(CaseExpression),
    ListExpression(ListExpression),
    ObjectExpression(ObjectExpression), //do we really need this?
    IteratorExpression(IteratorExpression),
}

impl ParentExpression for Expression {
    fn get_children(&self) -> Vec<&Expression> {
        match self {
            Expression::UnaryExpression(expr) => expr.get_children(),
            Expression::BinaryExpression(expr) => expr.get_children(),
            Expression::FunctionExpression(expr) => expr.get_children(),
            Expression::CaseExpression(expr) => expr.get_children(),
            Expression::ListExpression(expr) => expr.get_children(),
            Expression::ObjectExpression(expr) => expr.get_children(),
            Expression::IteratorExpression(expr) => expr.get_children(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub enum UnaryExpression {
    Not(Box<Expression>),
    Exists(Box<Expression>),
    IsNull(Box<Expression>),
    IsNotNull(Box<Expression>),
    Literal(Literal),
    Property {
        name: Arc<str>,
        key: Arc<str>,
    },
    ExpressionProperty {
        exp: Box<Expression>,
        key: Arc<str>,
    },
    Parameter(Arc<str>),
    Identifier(Arc<str>),
    Variable {
        name: Arc<str>,
        value: Box<Expression>,
    },
    Alias {
        source: Box<Expression>,
        alias: Arc<str>,
    },
    ListRange {
        //i64 instead of Expression?
        start_bound: Option<Box<Expression>>,
        end_bound: Option<Box<Expression>>,
    },
}

impl UnaryExpression {
    pub fn literal(value: Literal) -> Expression {
        Expression::UnaryExpression(UnaryExpression::Literal(value))
    }

    pub fn parameter(name: Arc<str>) -> Expression {
        Expression::UnaryExpression(UnaryExpression::Parameter(name))
    }

    pub fn property(name: Arc<str>, key: Arc<str>) -> Expression {
        Expression::UnaryExpression(UnaryExpression::Property { name, key })
    }

    pub fn expression_property(exp: Expression, key: Arc<str>) -> Expression {
        Expression::UnaryExpression(UnaryExpression::ExpressionProperty {
            exp: Box::new(exp),
            key,
        })
    }

    pub fn alias(source: Expression, alias: Arc<str>) -> Expression {
        Expression::UnaryExpression(Self::Alias {
            source: Box::new(source),
            alias,
        })
    }

    pub fn not(cond: Expression) -> Expression {
        Expression::UnaryExpression(Self::Not(Box::new(cond)))
    }

    pub fn ident(ident: &str) -> Expression {
        Expression::UnaryExpression(Self::Identifier(ident.into()))
    }

    pub fn is_null(expr: Expression) -> Expression {
        Expression::UnaryExpression(Self::IsNull(Box::new(expr)))
    }

    pub fn variable(name: Arc<str>, value: Expression) -> Expression {
        Expression::UnaryExpression(Self::Variable {
            name,
            value: Box::new(value),
        })
    }

    pub fn is_not_null(expr: Expression) -> Expression {
        Expression::UnaryExpression(Self::IsNotNull(Box::new(expr)))
    }
    pub fn list_range(
        start_bound: Option<Expression>,
        end_bound: Option<Expression>,
    ) -> Expression {
        Expression::UnaryExpression(Self::ListRange {
            start_bound: start_bound.map(Box::new),
            end_bound: end_bound.map(Box::new),
        })
    }
}

impl ParentExpression for UnaryExpression {
    fn get_children(&self) -> Vec<&Expression> {
        match self {
            UnaryExpression::Not(expr) => vec![expr],
            UnaryExpression::Exists(expr) => vec![expr],
            UnaryExpression::IsNull(expr) => vec![expr],
            UnaryExpression::IsNotNull(expr) => vec![expr],
            UnaryExpression::Literal(_) => Vec::new(),
            UnaryExpression::Property { .. } => Vec::new(),
            UnaryExpression::Parameter(_) => Vec::new(),
            UnaryExpression::Identifier(_) => Vec::new(),
            UnaryExpression::Variable { name: _, value } => vec![value],
            UnaryExpression::Alias { source, .. } => vec![source],
            UnaryExpression::ExpressionProperty { .. } => Vec::new(),
            UnaryExpression::ListRange { .. } => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub enum BinaryExpression {
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),

    Eq(Box<Expression>, Box<Expression>),
    Ne(Box<Expression>, Box<Expression>),
    Lt(Box<Expression>, Box<Expression>),
    Le(Box<Expression>, Box<Expression>),
    Gt(Box<Expression>, Box<Expression>),
    Ge(Box<Expression>, Box<Expression>),
    In(Box<Expression>, Box<Expression>),

    Add(Box<Expression>, Box<Expression>),
    Subtract(Box<Expression>, Box<Expression>),
    Multiply(Box<Expression>, Box<Expression>),
    Divide(Box<Expression>, Box<Expression>),
    Modulo(Box<Expression>, Box<Expression>),
    Exponent(Box<Expression>, Box<Expression>),
    HasLabel(Box<Expression>, Box<Expression>),
    Index(Box<Expression>, Box<Expression>),
}

impl BinaryExpression {
    pub fn and(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::And(Box::new(a), Box::new(b)))
    }

    pub fn or(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Or(Box::new(a), Box::new(b)))
    }

    pub fn eq(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Eq(Box::new(a), Box::new(b)))
    }

    pub fn ne(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Ne(Box::new(a), Box::new(b)))
    }

    pub fn lt(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Lt(Box::new(a), Box::new(b)))
    }

    pub fn le(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Le(Box::new(a), Box::new(b)))
    }

    pub fn gt(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Gt(Box::new(a), Box::new(b)))
    }

    pub fn in_(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::In(Box::new(a), Box::new(b)))
    }

    pub fn ge(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Ge(Box::new(a), Box::new(b)))
    }

    pub fn add(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Add(Box::new(a), Box::new(b)))
    }

    pub fn subtract(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Subtract(Box::new(a), Box::new(b)))
    }

    pub fn multiply(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Multiply(Box::new(a), Box::new(b)))
    }

    pub fn divide(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Divide(Box::new(a), Box::new(b)))
    }

    pub fn modulo(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Modulo(Box::new(a), Box::new(b)))
    }

    pub fn exponent(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Exponent(Box::new(a), Box::new(b)))
    }

    pub fn has_label(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::HasLabel(Box::new(a), Box::new(b)))
    }

    pub fn index(a: Expression, b: Expression) -> Expression {
        Expression::BinaryExpression(Self::Index(Box::new(a), Box::new(b)))
    }
}

impl ParentExpression for BinaryExpression {
    fn get_children(&self) -> Vec<&Expression> {
        match self {
            BinaryExpression::And(a, b) => vec![a, b],
            BinaryExpression::Or(a, b) => vec![a, b],
            BinaryExpression::Eq(a, b) => vec![a, b],
            BinaryExpression::Ne(a, b) => vec![a, b],
            BinaryExpression::Lt(a, b) => vec![a, b],
            BinaryExpression::Le(a, b) => vec![a, b],
            BinaryExpression::Gt(a, b) => vec![a, b],
            BinaryExpression::Ge(a, b) => vec![a, b],
            BinaryExpression::In(a, b) => vec![a, b],
            BinaryExpression::Add(a, b) => vec![a, b],
            BinaryExpression::Subtract(a, b) => vec![a, b],
            BinaryExpression::Multiply(a, b) => vec![a, b],
            BinaryExpression::Divide(a, b) => vec![a, b],
            BinaryExpression::Modulo(a, b) => vec![a, b],
            BinaryExpression::Exponent(a, b) => vec![a, b],
            BinaryExpression::HasLabel(a, b) => vec![a, b],
            BinaryExpression::Index(a, b) => vec![a, b],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct FunctionExpression {
    pub name: Arc<str>,
    pub args: Vec<Expression>,
    pub position_in_query: usize,
}

impl FunctionExpression {
    pub fn function(name: Arc<str>, args: Vec<Expression>, position_in_query: usize) -> Expression {
        Expression::FunctionExpression(FunctionExpression {
            name,
            args,
            position_in_query,
        })
    }

    pub fn eq_ignore_position_in_query(&self, other: &Self) -> bool {
        self.name == other.name && self.args == other.args
    }
}

impl ParentExpression for FunctionExpression {
    fn get_children(&self) -> Vec<&Expression> {
        self.args.iter().collect()
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct CaseExpression {
    pub match_: Option<Box<Expression>>,
    pub when: Vec<(Expression, Expression)>,
    pub else_: Option<Box<Expression>>,
}

impl CaseExpression {
    pub fn case(
        match_: Option<Expression>,
        when: Vec<(Expression, Expression)>,
        else_: Option<Expression>,
    ) -> Expression {
        Expression::CaseExpression(CaseExpression {
            match_: match_.map(Box::new),
            when,
            else_: else_.map(Box::new),
        })
    }
}

impl ParentExpression for CaseExpression {
    fn get_children(&self) -> Vec<&Expression> {
        let mut children = Vec::new();
        if let Some(match_) = &self.match_ {
            children.push(match_.as_ref());
        }
        for (when, then) in &self.when {
            children.push(when);
            children.push(then);
        }
        if let Some(else_) = &self.else_ {
            children.push(else_);
        }
        children
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct ObjectExpression {
    pub elements: BTreeMap<Arc<str>, Expression>,
}

impl ObjectExpression {
    pub fn object_from_vec(elements: Vec<(Arc<str>, Expression)>) -> Expression {
        let mut map = BTreeMap::new();
        for (key, value) in elements {
            map.insert(key, value);
        }
        Expression::ObjectExpression(ObjectExpression { elements: map })
    }

    pub fn object(elements: BTreeMap<Arc<str>, Expression>) -> Expression {
        Expression::ObjectExpression(ObjectExpression { elements })
    }
}

impl ParentExpression for ObjectExpression {
    fn get_children(&self) -> Vec<&Expression> {
        let keys: Vec<_> = self.elements.values().clone().collect();

        keys
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct ListExpression {
    pub elements: Vec<Expression>,
}

impl ListExpression {
    pub fn list(elements: Vec<Expression>) -> Expression {
        Expression::ListExpression(ListExpression { elements })
    }
}

impl ParentExpression for ListExpression {
    fn get_children(&self) -> Vec<&Expression> {
        let mut children = Vec::new();
        for element in &self.elements {
            children.push(element);
        }

        children
    }
}

#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct IteratorExpression {
    pub item_identifier: Arc<str>,
    pub list_expression: Box<Expression>,
    pub filter: Option<Box<Expression>>,
    pub map_expression: Option<Box<Expression>>,
}

impl IteratorExpression {
    pub fn map(
        item_identifier: Arc<str>,
        list_expression: Expression,
        map_expression: Expression,
    ) -> Expression {
        Expression::IteratorExpression(IteratorExpression {
            item_identifier,
            list_expression: Box::new(list_expression),
            filter: None,
            map_expression: Some(Box::new(map_expression)),
        })
    }

    pub fn map_with_filter(
        item_identifier: Arc<str>,
        list_expression: Expression,
        map_expression: Expression,
        filter: Expression,
    ) -> Expression {
        Expression::IteratorExpression(IteratorExpression {
            item_identifier,
            list_expression: Box::new(list_expression),
            filter: Some(Box::new(filter)),
            map_expression: Some(Box::new(map_expression)),
        })
    }

    pub fn iterator(item_identifier: Arc<str>, list_expression: Expression) -> Expression {
        Expression::IteratorExpression(IteratorExpression {
            item_identifier,
            list_expression: Box::new(list_expression),
            filter: None,
            map_expression: None,
        })
    }

    pub fn iterator_with_filter(
        item_identifier: Arc<str>,
        list_expression: Expression,
        filter: Expression,
    ) -> Expression {
        Expression::IteratorExpression(IteratorExpression {
            item_identifier,
            list_expression: Box::new(list_expression),
            filter: Some(Box::new(filter)),
            map_expression: None,
        })
    }
}

impl ParentExpression for IteratorExpression {
    fn get_children(&self) -> Vec<&Expression> {
        let mut children = Vec::new();
        children.push(&*self.list_expression);
        if let Some(filter) = &self.filter {
            children.push(filter);
        }
        children
    }
}
