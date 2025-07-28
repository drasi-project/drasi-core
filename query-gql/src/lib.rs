#![allow(clippy::redundant_closure_call)]
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

use drasi_query_ast::{
    api::{QueryParseError, QueryParser},
    ast::{
        self, Expression, MatchClause, ParentExpression, ProjectionClause, QueryPart,
        UnaryExpression,
    },
};
use peg::{error::ParseError, str::LineCol};
use std::{collections::HashSet, sync::Arc};

#[cfg(test)]
mod tests;

peg::parser! {
    grammar gql() for str {
        use drasi_query_ast::ast::*;

        rule kw_match()     = ("MATCH" / "match")
        rule kw_optional()  = ("OPTIONAL" / "optional")
        rule kw_where()     = ("WHERE" / "where")
        rule kw_return()    = ("RETURN" / "return")
        rule kw_true()      = ("TRUE" / "true")
        rule kw_false()     = ("FALSE" / "false")
        rule kw_null()      = ("NULL" / "null")
        rule kw_and()       = ("AND" / "and")
        rule kw_or()        = ("OR" / "or")
        rule kw_not()       = ("NOT" / "not")
        rule kw_is()        = ("IS" / "is")
        rule kw_as()        = ("AS" / "as")
        rule kw_case()      = ("CASE" / "case")
        rule kw_when()      = ("WHEN" / "when")
        rule kw_then()      = ("THEN" / "then")
        rule kw_else()      = ("ELSE" / "else")
        rule kw_end()       = ("END" / "end")
        rule kw_in()        = ("IN" / "in")
        rule kw_exists()    = ("EXISTS" / "exists")
        rule kw_group()     = ("GROUP" / "group")
        rule kw_by()        = ("BY" / "by")
        rule kw_let()       = ("LET" / "let")
        rule kw_yield()     = ("YIELD" / "yield")
        rule kw_filter()    = ("FILTER" / "filter")

        rule _()
            = quiet!{[' ']}

        rule __()
            = quiet!{[' ' | '\n' | '\t']}
            / comment()

        rule comment()
            = quiet!{ "//" (!"\n" [_])* ("\n" / ![_])}

        rule alpha()
            = ['a'..='z' | 'A'..='Z']

        rule num()
            = quiet! {
                ['0'..='9']
            }
            / expected!("a number")

        rule alpha_num()
            = ['a'..='z' | 'A'..='Z' | '0'..='9' | '_']


        // e.g. '42', '-1'
        rule integer() -> i64
            = integer:$("-"?num()+) {? integer.parse().or(Err("invalid integer")) }

        // e.g. '-0.53', '34346.245', '236.0'
        rule real() -> f64
            = real:$("-"? num()+ "." num()+) {? real.parse().or(Err("invalid real"))}

        // e.g. 'TRUE', 'FALSE'
        rule boolean() -> bool
            = kw_true() { true } / kw_false() { false }

        // e.g. 'hello world'
        rule text() -> Arc<str>
            = quiet! {
                "'" text:$((date_for_date_time() "T" time_format() timezone())) "'"{ Arc::from(text) }
            }
            / quiet! {
                "'" text:$((date_for_date_time() "T" time_format())) "'"{ Arc::from(text) }
            }
            / quiet! {
                "'" text:$(date_format()) "'" { Arc::from(text) }
            }
            / quiet! {
                "'" text:$(time_format() timezone()) "'" { Arc::from(text) }
            }
            / quiet! {
                "'" text:$(time_format()) "'" { Arc::from(text) }
            }
            /quiet!{
                "'" text:$(duration()) "'" { Arc::from(text) }
            }
            /quiet! {
                "'" text:$([^ '\'' | '\n' | '\r']*) "'" { Arc::from(text) }
            }
            / expected!("a quoted string")

        // e.g. 'TRUE', '42', 'hello world'
        rule literal() -> Literal
            = r:real() { Literal::Real(r) }
            / i:integer() { Literal::Integer(i) }
            / b:boolean() { Literal::Boolean(b) }
            / t:text() { Literal::Text(t) }
            / kw_null() { Literal::Null }

        rule year() -> Arc<str>
            = year:$(['0'..='9']*<4>) { Arc::from(year) }

        rule month() -> Arc<str>
            = month:$(['0'..='1']['0'..='9']) { Arc::from(month) }

        rule day() -> Arc<str>
            = day:$(['0'..='9']*<2>) { Arc::from(day) }

        rule week() -> Arc<str>
            = week:$("W" ['0'..='9']*<0,2>) { Arc::from(week) }

        rule quarter() -> Arc<str>
            = quarter:$("Q" ['1'..='4']) { Arc::from(quarter)}

        rule date_format() -> Arc<str>
            = date_format:$(year() "-"? month() "-"? day()) { Arc::from(date_format) }
            / date_format:$(year() "-"? week() "-"? ['0'..='7']) { Arc::from(date_format) }
            / date_format:$(year() "-"? week()) { Arc::from(date_format) }
            / date_format:$(year() "-"? quarter() "-"? day()) { Arc::from(date_format) }
            / date_format:$(year() "-"? month()) { Arc::from(date_format) }
            / date_format:$(year() "-"? ['0'..='9']*<3>) { Arc::from(date_format) }
            / date_format:$(year()) { Arc::from(date_format) }


        rule hour() -> Arc<str>
            = hour:$(['0'..='2']['0'..='9']) { Arc::from(hour) }

        rule minute() -> Arc<str>
            = minute:$(['0'..='5']['0'..='9']) { Arc::from(minute) }

        rule second() -> Arc<str>
            = second:$(['0'..='5']['0'..='9']) { Arc::from(second) }

        rule time_fraction() -> Arc<str>
            = time_fraction:$("." ['0'..='9']*<0,9>) { Arc::from(time_fraction) }

        rule time_format() ->  Arc<str>
            = time_format:$(hour() ":"? minute() ":"? second()? time_fraction()?) { Arc::from(time_format) }
            / time_format:$(hour() ":"? minute() ":"? second()) { Arc::from(time_format) }
            / time_format:$(hour() ":"? minute()) { Arc::from(time_format) }
            / time_format:$(hour()) { Arc::from(time_format) }

        rule date_for_date_time() -> Arc<str>
            = date_for_date_time:$(year() "-"? month() "-"? day()) { Arc::from(date_for_date_time) }
            / date_for_date_time:$(year() "-"? week() "-"? ['0'..='7']) { Arc::from(date_for_date_time) }
            / date_for_date_time:$(year() "-"? quarter() "-"? day() ) { Arc::from(date_for_date_time) }
            / date_for_date_time:$(year() "-"? ['0'..='9']*<3>) { Arc::from(date_for_date_time) }



        rule timezone() -> Arc<str>
            = timezone:$("Z") { Arc::from(timezone) }
            / timezone:$("[" ['a'..='z' | 'A'..='Z' | '_']+ "/" ['a'..='z' | 'A'..='Z' | '_']+  "]") { Arc::from(timezone) }
            / timezone:$("+" hour() ":"? minute()?) { Arc::from(timezone) }
            / timezone:$("-" hour() ":"? minute()?) { Arc::from(timezone) }
            / timezone:$("+" hour() "[" ['a'..='z' | 'A'..='Z' | '_']+ "/" ['a'..='z' | 'A'..='Z' | '_']+ "]" ) { Arc::from(timezone) }
            / timezone:$("-" hour() "[" ['a'..='z' | 'A'..='Z' | '_']+ "/" ['a'..='z' | 'A'..='Z' | '_']+  "]" ) { Arc::from(timezone) }
            //specific timezone (IANA timezone database)


        rule space() = [' ']*

        rule duration() -> Arc<str>
            = duration:$("P" date_for_date_time()? "T" time_format()?) "\"" ")" { Arc::from(duration) } //"P2012-02-02T14:37:21.545"
            / duration:$("P" (['0'..='9']*<0,19> "Y")? ( ['0'..='9']*<0,19> "M")? ( ['0'..='9']*<0,19> "W")? ( ['0'..='9']*<0,19>  time_fraction()?"D")? ("T" ( ['0'..='9']*<0,19> "H")? ( ['0'..='9']*<0,19> "M")? ( ['0'..='9']*<0,19> time_fraction()?  "S")?)?) { Arc::from(duration) }
            / duration:$("P" (['0'..='9']*<0,19> "Y")? ( ['0'..='9']*<0,19> "M")? ( ['0'..='9']*<0,19> "W")? ( ['0'..='9']*<0,19>  time_fraction()?"D")? ){ Arc::from(duration) }
            / duration:$("P" ("T" ( ['0'..='9']*<0,19> "H")? ( ['0'..='9']*<0,19>"M")? ( ['0'..='9']*<0,19>  time_fraction()? "S")?)?){ Arc::from(duration) }



        rule projection_expression() -> Expression
            = z:expression() _* kw_as() _* a:ident() { UnaryExpression::alias(z, a) }
            / expression()

        rule when_expression() -> (Expression, Expression)
            = kw_when() __+ when:expression() __+ kw_then() __+ then:expression() __+ { (when, then) }

        rule else_expression() -> Expression
            = kw_else() __+ else_:expression() __+ { else_ }

        rule let_assign() -> (Arc<str>, Expression)
            = name:ident() __* "=" __* expr:expression() { (name, expr) }

        rule let_statement() -> Vec<(Arc<str>,Expression)>
            = __* kw_let() __+
            assigns:(let_assign() ** (__* "," __*))
            __*
            { assigns }

        rule yield_assign() -> (Expression, Option<Arc<str>>)
            = expr:expression() __* kw_as() __* alias:ident() { (expr, Some(alias)) }
            / expr:expression() { (expr, None) }

        rule yield_statement() -> Vec<(Expression, Option<Arc<str>>)>
            = __* kw_yield() __+
              assigns:(yield_assign() ** (__* "," __*))
              __* { assigns }

        rule filter_statement() -> Expression
            = __* kw_filter() __+ expr:expression() { expr }

        rule statement_clause() -> StatementClause
            = l:let_statement()    { StatementClause::Let(l) }
            / y:yield_statement()  { StatementClause::Yield(y) }
            / f:filter_statement() { StatementClause::Filter(f) }

            #[cache_left_rec]
        pub rule expression() -> Expression
            = precedence!{
                a:(@) __* kw_and() __* b:@ { BinaryExpression::and(a, b) }
                a:(@) __* kw_or() __* b:@ { BinaryExpression::or(a, b) }
                --
                kw_not() __* c:(@) { UnaryExpression::not(c) }
                --
                it:iterator() { it }
                --
                a:(@) __* kw_in() __* b:@ { BinaryExpression::in_(a, b) }
                a:(@) __* "="  __* b:@ { BinaryExpression::eq(a, b) }
                a:(@) __* ("<>" / "!=") __* b:@ { BinaryExpression::ne(a, b) }
                a:(@) __* "<"  __* b:@ { BinaryExpression::lt(a, b) }
                a:(@) __* "<=" __* b:@ { BinaryExpression::le(a, b) }
                a:(@) __* ">"  __* b:@ { BinaryExpression::gt(a, b) }
                a:(@) __* ">=" __* b:@ { BinaryExpression::ge(a, b) }
                --
                a:(@) __* "+" __* b:@ { BinaryExpression::add(a, b) }
                a:(@) __* "-" __* b:@ { BinaryExpression::subtract(a, b) }
                --
                a:(@) __* "*" __* b:@ { BinaryExpression::multiply(a, b) }
                a:(@) __* "/" __* b:@ { BinaryExpression::divide(a, b) }
                --
                a:(@) __* "%" __* b:@ { BinaryExpression::modulo(a, b) }
                a:(@) __* "^" __* b:@ { BinaryExpression::exponent(a, b) }
                --
                parent:@ "." key:ident() { UnaryExpression::expression_property(parent, key) }
                --
                list:expression() "[" index:expression() "]" { BinaryExpression::index(list, index)}
                e:(@) __+ kw_is() _+ kw_null() { UnaryExpression::is_null(e) }
                e:(@) __+ kw_is() _+ kw_not() _+ kw_null() { UnaryExpression::is_not_null(e) }
                kw_case() __* mtch:expression()? __* when:when_expression()+ __* else_:else_expression()? __* kw_end() { CaseExpression::case(mtch, when, else_) }
                kw_case() __* when:when_expression()+ __* else_:else_expression()? __* kw_end() { CaseExpression::case(None, when, else_) }
                pos: position!() func:function_name() _* "(" __* params:(expression() ** (__* "," __*))? __* ")" "." key:ident() {
                    let params = params.unwrap_or_else(Vec::new);
                    UnaryExpression::expression_property(FunctionExpression::function(func, params, pos ), key)
                }
                pos: position!() func:function_name() _* "(" __* params:(expression() ** (__* "," __*))? __* ")" {
                    let params = params.unwrap_or_else(Vec::new);
                    FunctionExpression::function(func, params, pos )
                }
                "$" name:ident() { UnaryExpression::parameter(name) }
                start:expression()? ".." end:expression()? { UnaryExpression::list_range(start, end) }
                l:literal() { UnaryExpression::literal(l) }
                i:ident() { UnaryExpression::ident(&i) } // UnaryExpression::ident(i)

                --
                "(" __* c:expression() __* ")" { c }
                c: component() { ObjectExpression::object_from_vec(c)  } //ObjectExpression
                "[" __* c:expression() ** (__* "," __*) __* "]" { ListExpression::list(c) }
            }

            #[cache_left_rec]
        rule iterator() -> Expression
            = "[" __* item:ident() __* kw_in() __* list:expression() __* kw_where() __* filter:expression() __* "|" __* map:expression()__* "]"
                { IteratorExpression::map_with_filter(item, list, map, filter) }

            / "[" __* item:ident() __* kw_in() __* list:expression() __*  "|" __* map:expression()__* "]"
                { IteratorExpression::map(item, list, map) }

            / "[" __* item:ident() __* kw_in() __* list:expression() __* kw_where() __* filter:expression()__* "]"
                { IteratorExpression::iterator_with_filter(item, list, filter) }

            / "[" __* item:ident() __* kw_in() __* list:expression()__* "]"
                { IteratorExpression::iterator(item, list) }

            / item:ident() __* kw_in() __* list:expression() __* kw_where() __* filter:expression() __* "|" __* map:expression()
                { IteratorExpression::map_with_filter(item, list, map, filter) }

            / item:ident() __* kw_in() __* list:expression() __*  "|" __* map:expression()
                { IteratorExpression::map(item, list, map) }

            / item:ident() __* kw_in() __* list:expression() __* kw_where() __* filter:expression()
                { IteratorExpression::iterator_with_filter(item, list, filter) }

            / item:ident() __* kw_in() __* list:expression()
                { IteratorExpression::iterator(item, list) }


        // e.g. 'hello_world', 'Rust', 'HAS_PROPERTY'
        rule ident() -> Arc<str>
            = quiet!{ident:$(alpha()alpha_num()*) { Arc::from(ident) }}
            / "`" ident:$([^ '`' | '\n' | '\r']*) "`" { Arc::from(ident) }
            / expected!("an identifier")

        // e.g. 'sign', 'duration_between'
        rule function_name()  -> Arc<str>
            = quiet!{func:$(alpha()alpha_num()* ("." alpha_num()+)?) { Arc::from(func) }}
            / expected!("function name")

        rule component() -> Vec<(Arc<str>, Expression)>
            = "{" __* entries:( (k:ident() __* ":" __* v:expression() { (k, v) }) ++ (__* "," __*) ) __* "}" { entries }

        // e.g. 'a', 'a : PERSON', ': KNOWS'
        rule annotation() -> Annotation
            = name:ident()? { Annotation { name } }


        // e.g. '{answer: 42, book: 'Hitchhikers Guide'}'
        rule property_map() -> Vec<(Arc<str>, Expression)>
            = "{" __* entries:( (k:ident() __* ":" __* v:expression() { (k, v) }) ++ (__* "," __*) ) __* "}" { entries }

        rule property_map_predicate() -> Vec<Expression>
            = "{" __* entries:( (k:ident() __* ":" __* v:expression() { BinaryExpression::eq(UnaryExpression::property("".into(), k), v) }) ++ (__* "," __*) ) __* "}" { entries }

        rule element_match() -> (Annotation, Vec<Arc<str>>, Vec<Expression>)
            = a:annotation() labels:(":" label:ident() ** "|" { label })? _* p:(pm:property_map_predicate() { pm } / ( w:(where_clause() ** (__+) )? { w.unwrap_or_else(Vec::new) } ) ) {
                (a, labels.unwrap_or_else(Vec::new), p)
            }

        // e.g. '()', '( a:PERSON )', '(b)', '(a : OTHER_THING)'
        rule node() -> NodeMatch
            = "(" _* element:element_match() _* ")" {
                NodeMatch::new(element.0, element.1, element.2)
              }
            / expected!("node match pattern, e.g. '()', '( a:PERSON )', '(b)', '(a : OTHER_THING)'")

        // e.g. '-', '<-', '-[ name:KIND ]-', '<-[name]-'
        rule relation() -> RelationMatch
            =  "-[" _* element:element_match() _* vl:variable_length()? _* "]->" {
                RelationMatch::right(element.0, element.1, element.2, vl)
            }
            /  "-[" _* element:element_match() _* vl:variable_length()? _* "]-"  {
                RelationMatch::either(element.0, element.1, element.2, vl)
            }
            / "<-[" _* element:element_match() _* vl:variable_length()? _* "]-"  {
                RelationMatch::left(element.0, element.1, element.2, vl)
            }
            / "<-" { RelationMatch::left(Annotation::empty(), Vec::new(), Vec::new(), None) }
            / "->" { RelationMatch::right(Annotation::empty(), Vec::new(), Vec::new(), None) }
            / "-" { RelationMatch::either(Annotation::empty(), Vec::new(), Vec::new(), None) }
            / expected!("relation match pattern, e.g. '-', '<-', '-[ name:KIND ]-', '<-[name]-'")


        rule variable_length() -> VariableLengthMatch
            = quiet!{
                "*" min_hops:integer()? max_hops:(".." r:integer() {r})? { VariableLengthMatch{ min_hops, max_hops } }
            }
            / expected!("variable length match pattern, e.g. '*', '*2', '*..3'")

        rule property() -> (Arc<str>, Arc<str>)
            = name:ident() "." key:ident() { (name, key) }
            / "$" name:ident() "." key:ident() { (name, key) }

            #[cache_left_rec]
        rule nested_property() -> Expression
            = parent:expression() "." key:ident() { UnaryExpression::expression_property(parent, key) }

        // e.g. 'MATCH (a)', 'MATCH (a) -> (b) <- (c)', ...
        rule match_clause() -> Vec<MatchClause>
            = kw_match() __+ items:( (start:node()
                    path:( (__* e:relation() __* n:node() { (e, n) }) ** "" ) {
                    MatchClause { start, path, optional: false }
                }) ++ (__* "," __*) ) { items }
            / kw_optional() __+ kw_match() __+ items:( (start:node()
                path:( (__* e:relation() __* n:node() { (e, n) }) ** "" ) {
                    MatchClause { start, path, optional: true }
                }) ++ (__* "," __*) ) { items }

        // e.g. 'WHERE a.name <> b.name', 'WHERE a.age > b.age AND a.age <= 42'
        rule where_clause() -> Expression
            = kw_where() __+ c:expression() { c }

        // e.g. 'RETURN a, b'
        rule return_clause() -> Vec<Expression>
            = kw_return() __+ items:( projection_expression() ++ (__* "," __*) ) { items }

        rule group_by_clause() -> Vec<Expression>
            = kw_group() __+ kw_by() __+ "(" __* ")" { Vec::new() }  // Handle GROUP BY ()
            / kw_group() __+ kw_by() __+ items:( expression() ++ (__* "," __*) ) { items }

        rule match_with_where() -> (Vec<MatchClause>, Vec<Expression>)
            = ms:(match_clause() ++ (__+)) __*
            w:where_clause()? { (ms.into_iter().flatten().collect(), w.into_iter().collect()) }

        rule part(config: &dyn GQLConfiguration) -> Vec<QueryPart>
              = __*
                match_and_where:(match_with_where() ** (__+))
                statement_clauses:( __* s:statement_clause() __* { s } )*
                __* final_return:return_clause()
                __* group_by:group_by_clause()?
                __*
                  {? build_query_parts(match_and_where, statement_clauses, final_return, group_by, config).map_err(|e| match e {
                    // Will return full expected set in addition to the error message
                      QueryParseError::MissingGroupByKey => "Non-grouped RETURN expressions must appear in GROUP BY clause",
                      QueryParseError::ParserError(_) => "Parser error",
                  }) }



        pub rule query(config: &dyn GQLConfiguration) -> Query
            = __*
                parts:(w:( part(config)+ ) { w.into_iter().flatten().collect() } )
                __* {
                Query {
                    parts,
                }
            }
    }

}

enum StatementClause {
    Let(Vec<(Arc<str>, Expression)>),
    Yield(Vec<(Expression, Option<Arc<str>>)>),
    Filter(Expression),
}

pub fn parse(
    input: &str,
    config: &dyn GQLConfiguration,
) -> Result<ast::Query, ParseError<LineCol>> {
    gql::query(input, config)
}

pub fn parse_expression(input: &str) -> Result<ast::Expression, ParseError<LineCol>> {
    gql::expression(input)
}

pub trait GQLConfiguration: Send + Sync {
    fn get_aggregating_function_names(&self) -> HashSet<String>;
}

fn build_query_parts(
    match_and_where: Vec<(Vec<MatchClause>, Vec<Expression>)>,
    statement_clauses: Vec<StatementClause>,
    final_return_expressions: Vec<Expression>,
    group_by_expressions: Option<Vec<Expression>>,
    config: &dyn GQLConfiguration,
) -> Result<Vec<QueryPart>, QueryParseError> {
    let mut match_clauses = Vec::new();
    let mut where_clauses = Vec::new();
    for (m, ws) in match_and_where {
        match_clauses.extend(m);
        where_clauses.extend(ws);
    }
    let mut query_parts = Vec::new();

    if !statement_clauses.is_empty() {
        let starting_scope = get_starting_scope(&match_clauses);
        query_parts.extend(build_parts_for_statements(
            std::mem::take(&mut match_clauses),
            std::mem::take(&mut where_clauses),
            statement_clauses,
            starting_scope,
            config,
        ));
    }

    let final_projection_parts = if let Some(group_keys) = group_by_expressions {
        handle_explicit_group_by(
            match_clauses,
            where_clauses,
            final_return_expressions,
            group_keys,
            config,
        )?
    } else {
        vec![QueryPart {
            match_clauses,
            where_clauses,
            return_clause: final_return_expressions.into_projection_clause(config),
        }]
    };

    query_parts.extend(final_projection_parts);
    Ok(query_parts)
}

fn build_parts_for_statements(
    mut match_clauses: Vec<MatchClause>,
    mut where_clauses: Vec<Expression>,
    statements: Vec<StatementClause>,
    mut scope: Vec<Expression>,
    config: &dyn GQLConfiguration,
) -> Vec<QueryPart> {
    let mut parts = Vec::new();

    for statement in statements {
        let match_clause = std::mem::take(&mut match_clauses);
        let where_clause = std::mem::take(&mut where_clauses);

        match statement {
            StatementClause::Let(bindings) => {
                let mut projection = scope.clone();
                for (alias, expr) in bindings {
                    projection.push(UnaryExpression::alias(expr.clone(), alias.clone()));
                    scope.push(UnaryExpression::ident(alias.as_ref()));
                }
                parts.push(QueryPart {
                    match_clauses: match_clause,
                    where_clauses: where_clause,
                    return_clause: projection.into_projection_clause(config),
                });
            }

            StatementClause::Yield(yields) => {
                let mut projection = Vec::new();
                let mut new_scope = Vec::new();
                for (expr, alias_opt) in yields {
                    if let Some(alias) = alias_opt {
                        projection.push(UnaryExpression::alias(expr.clone(), alias.clone()));
                        new_scope.push(UnaryExpression::ident(alias.as_ref()));
                    } else {
                        projection.push(expr.clone());
                        if let Expression::UnaryExpression(UnaryExpression::Identifier(id)) = &expr
                        {
                            new_scope.push(UnaryExpression::ident(id));
                        }
                    }
                }
                scope = new_scope;
                parts.push(QueryPart {
                    match_clauses: match_clause,
                    where_clauses: where_clause,
                    return_clause: projection.into_projection_clause(config),
                });
            }

            StatementClause::Filter(filter) => {
                if !match_clause.is_empty() || !where_clause.is_empty() {
                    parts.push(QueryPart {
                        match_clauses: match_clause,
                        where_clauses: where_clause,
                        return_clause: scope.clone().into_projection_clause(config),
                    });
                }
                parts.push(QueryPart {
                    match_clauses: Vec::new(),
                    where_clauses: vec![filter],
                    return_clause: scope.clone().into_projection_clause(config),
                });
            }
        }
    }

    parts
}

fn get_starting_scope(match_clauses: &[MatchClause]) -> Vec<Expression> {
    let mut expressions = Vec::new();
    let mut seen: HashSet<Arc<str>> = HashSet::new();

    for clause in match_clauses {
        if let Some(name) = &clause.start.annotation.name {
            if seen.insert(name.clone()) {
                expressions.push(UnaryExpression::ident(name.as_ref()));
            }
        }
        for (_rel, node) in &clause.path {
            if let Some(name) = &node.annotation.name {
                if seen.insert(name.clone()) {
                    expressions.push(UnaryExpression::ident(name.as_ref()));
                }
            }
        }
    }

    expressions
}

fn handle_explicit_group_by(
    match_clauses: Vec<MatchClause>,
    where_clauses: Vec<Expression>,
    return_expressions: Vec<Expression>,
    group_by_keys: Vec<Expression>,
    config: &dyn GQLConfiguration,
) -> Result<Vec<QueryPart>, QueryParseError> {
    let mut return_non_aggs = Vec::new();
    let mut return_aggs = Vec::new();
    for expr in &return_expressions {
        if contains_aggregating_function(expr, config) {
            return_aggs.push(expr.clone());
        } else {
            return_non_aggs.push(expr.clone());
        }
    }

    if return_non_aggs.iter().any(|expr| {
        group_by_keys
            .iter()
            .all(|key| !matches_group_by_key(expr, key))
    }) {
        return Err(QueryParseError::MissingGroupByKey);
    }

    // All GROUP BY keys appear in the RETURN non-aggregates
    if group_by_keys
        .iter()
        .all(|k| return_non_aggs.iter().any(|e| matches_group_by_key(e, k)))
    {
        return Ok(vec![QueryPart {
            match_clauses,
            where_clauses,
            return_clause: return_expressions.into_projection_clause(config),
        }]);
    }

    // Fewer keys are projected in the RETURN than are in the GROUP BY clause.
    let full_keys: Vec<_> = group_by_keys
        .into_iter()
        .map(|key| {
            return_expressions
                .iter()
                .find(|e| matches_group_by_key(e, &key))
                .cloned()
                .unwrap_or(key)
        })
        .collect();

    Ok(group_all_keys_then_project_return(
        &match_clauses,
        &where_clauses,
        full_keys,
        return_aggs,
        &return_expressions,
    ))
}

fn matches_group_by_key(expr: &Expression, key: &Expression) -> bool {
    if expr == key {
        return true;
    }

    match (expr, key) {
        (Expression::FunctionExpression(f1), Expression::FunctionExpression(f2)) => {
            f1.eq_ignore_position_in_query(f2)
        }

        // If the RETURN expression is an alias (a.name AS person_name) and the GROUP BY references the alias name (person_name), it is a match.
        (
            Expression::UnaryExpression(UnaryExpression::Alias { alias, .. }),
            Expression::UnaryExpression(UnaryExpression::Identifier(id)),
        ) => id == alias,
        _ => false,
    }
}

fn group_all_keys_then_project_return(
    match_clauses: &[MatchClause],
    where_clauses: &[Expression],
    grouping: Vec<Expression>,
    aggregates: Vec<Expression>,
    original_return: &[Expression],
) -> Vec<QueryPart> {
    let initial_projection = if aggregates.is_empty() {
        ProjectionClause::Item(grouping.clone())
    } else {
        ProjectionClause::GroupBy {
            grouping: grouping.clone(),
            aggregates: aggregates.clone(),
        }
    };

    let perform_grouping_part = QueryPart {
        match_clauses: match_clauses.to_owned(),
        where_clauses: where_clauses.to_owned(),
        return_clause: initial_projection,
    };

    let project_return_part = QueryPart {
        match_clauses: vec![],
        where_clauses: vec![],
        return_clause: ProjectionClause::Item(
            original_return
                .iter()
                .map(expression_to_alias_ident)
                .collect(),
        ),
    };

    vec![perform_grouping_part, project_return_part]
}

// Converts an expression (x + 1 AS sum) to just the alias ident (sum).
fn expression_to_alias_ident(expr: &Expression) -> Expression {
    if let Expression::UnaryExpression(UnaryExpression::Alias { alias, .. }) = expr {
        UnaryExpression::ident(alias)
    } else {
        expr.clone()
    }
}

pub trait IntoProjectionClause {
    fn into_projection_clause(self, config: &dyn GQLConfiguration) -> ProjectionClause;
}

impl IntoProjectionClause for Vec<Expression> {
    fn into_projection_clause(self, config: &dyn GQLConfiguration) -> ProjectionClause {
        let mut keys = Vec::new();
        let mut aggs = Vec::new();

        for expr in self {
            if contains_aggregating_function(&expr, config) {
                aggs.push(expr);
            } else {
                keys.push(expr);
            }
        }

        if aggs.is_empty() {
            ProjectionClause::Item(keys)
        } else {
            ProjectionClause::GroupBy {
                grouping: keys,
                aggregates: aggs,
            }
        }
    }
}

pub fn contains_aggregating_function(
    expression: &Expression,
    config: &dyn GQLConfiguration,
) -> bool {
    let stack = &mut vec![expression];
    let aggr_funcs = config.get_aggregating_function_names();

    while let Some(expr) = stack.pop() {
        if let Expression::FunctionExpression(ref function) = expr {
            if aggr_funcs.contains(&function.name.to_string()) {
                return true;
            }
        }

        for c in expr.get_children() {
            stack.push(c);
        }
    }

    false
}

pub struct GQLParser {
    config: Arc<dyn GQLConfiguration>,
}

impl GQLParser {
    pub fn new(config: Arc<dyn GQLConfiguration>) -> Self {
        GQLParser { config }
    }
}

impl QueryParser for GQLParser {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError> {
        match parse(input, &*self.config) {
            Ok(query) => Ok(query),
            Err(e) => Err(QueryParseError::ParserError(Box::new(e))),
        }
    }
}
