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
    ast::{self, Expression, ParentExpression, ProjectionClause},
};
use peg::{error::ParseError, str::LineCol};
use std::{collections::HashSet, sync::Arc};

#[cfg(test)]
mod tests;

peg::parser! {
    grammar cypher() for str {
        use drasi_query_ast::ast::*;

        rule kw_match()     = ("MATCH" / "match")
        rule kw_optional()  = ("OPTIONAL" / "optional")
        rule kw_create()    = ("CREATE" / "create")
        rule kw_set()       = ("SET" / "set")
        rule kw_delete()    = ("DELETE" / "delete")
        rule kw_where()     = ("WHERE" / "where")
        rule kw_return()    = ("RETURN" / "return")
        rule kw_true()      = ("TRUE" / "true")
        rule kw_false()     = ("FALSE" / "false")
        rule kw_null()      = ("NULL" / "null")
        rule kw_and()       = ("AND" / "and")
        rule kw_or()        = ("OR" / "or")
        rule kw_not()       = ("NOT" / "not")
        rule kw_is()        = ("IS" / "is")
        rule kw_id()        = ("ID" / "id")
        rule kw_label()     = ("LABEL" / "label")
        rule kw_as()        = ("AS" / "as")
        rule kw_case()      = ("CASE" / "case")
        rule kw_when()      = ("WHEN" / "when")
        rule kw_then()      = ("THEN" / "then")
        rule kw_else()      = ("ELSE" / "else")
        rule kw_end()       = ("END" / "end")
        rule kw_with()      = ("WITH" / "with")
        rule kw_in()        = ("IN" / "in")
        rule kw_exists()    = ("EXISTS" / "exists")

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

        // e.g. 'SET a.name = 'Peter Parker''
        rule set_clause() -> SetClause
            = kw_set() __+ p:property() _* "=" _* e:expression() {
                SetClause { name: p.0, key: p.1, value: e }
            }

        // e.g. 'DELETE a'
        rule delete_clause() -> Arc<str>
            = kw_delete() __+ name:ident() { name }

        // e.g. 'RETURN a, b'
        rule return_clause() -> Vec<Expression>
            = kw_return() __+ items:( projection_expression() ++ (__* "," __*) ) { items }

        rule with_clause() -> Vec<Expression>
            = kw_with() __+ items:( projection_expression() ++ (__* "," __*) ) { items }

        rule with_or_return() -> Vec<Expression>
            = w:with_clause() { w }
            / r:return_clause() { r }

        rule part(config: &dyn CypherConfiguration) -> QueryPart
            = match_clauses:( __* m:(match_clause() ** (__+) )? { m.unwrap_or_else(Vec::new).into_iter().flatten().collect() } )
                where_clauses:( __* w:(where_clause() ** (__+) )? { w.unwrap_or_else(Vec::new) } )
                //create_clauses:( __* c:(create_clause() ** (__+) )? { c.unwrap_or_else(Vec::new) } )
                set_clauses:( __* s:(set_clause() ** (__+) )? { s.unwrap_or_else(Vec::new) } )
                delete_clauses:( __* d:(delete_clause() ** (__+) )? { d.unwrap_or_else(Vec::new) } )
                return_clause:( with_or_return() )
                {
                    QueryPart {
                        match_clauses,
                        where_clauses,
                        return_clause: return_clause.into_projection_clause(config),
                    }
                }

        pub rule query(config: &dyn CypherConfiguration) -> Query
            = __*
              parts:(w:( part(config)+ ) { w } )
              __* {
                Query {
                    parts,
                }
            }
    }
}

pub fn parse(
    input: &str,
    config: &dyn CypherConfiguration,
) -> Result<ast::Query, ParseError<LineCol>> {
    cypher::query(input, config)
}

pub fn parse_expression(input: &str) -> Result<ast::Expression, ParseError<LineCol>> {
    cypher::expression(input)
}

pub trait CypherConfiguration: Send + Sync {
    fn get_aggregating_function_names(&self) -> HashSet<String>;
}

pub trait IntoProjectionClause {
    fn into_projection_clause(self, config: &dyn CypherConfiguration) -> ProjectionClause;
}

impl IntoProjectionClause for Vec<Expression> {
    fn into_projection_clause(self, config: &dyn CypherConfiguration) -> ProjectionClause {
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
    config: &dyn CypherConfiguration,
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

pub struct CypherParser {
    config: Arc<dyn CypherConfiguration>,
}

impl CypherParser {
    pub fn new(config: Arc<dyn CypherConfiguration>) -> Self {
        CypherParser { config }
    }
}

impl QueryParser for CypherParser {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError> {
        match parse(input, &*self.config) {
            Ok(query) => Ok(query),
            Err(e) => Err(QueryParseError::ParserError(Box::new(e))),
        }
    }
}
