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

use std::collections::HashSet;

use super::*;
use ast::*;

struct TestCypherConfig {}

impl CypherConfiguration for TestCypherConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("count".into());
        set.insert("sum".into());
        set.insert("min".into());
        set.insert("max".into());
        set.insert("avg".into());
        set.insert("drasi.linearGradient".into());
        set.insert("drasi.last".into());
        set
    }
}

static TEST_CONFIG: TestCypherConfig = TestCypherConfig {};

#[test]
fn return_clause_non_aggregating() {
    let query = cypher::query(
        "MATCH (a) WHERE a.Field1 = 42 RETURN a.name, a.Field2 as F2, $param",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![
            UnaryExpression::expression_property(UnaryExpression::ident("a"), "name".into()),
            UnaryExpression::alias(
                UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field2".into()),
                "F2".into()
            ),
            UnaryExpression::parameter("param".into()),
        ])
    );
}

#[test]
fn return_clause_aggregating() {
    let query = cypher::query(
        "MATCH (a) WHERE a.Field1 = 42 RETURN a.name, sum(a.Field2)",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![UnaryExpression::expression_property(
                UnaryExpression::ident("a"),
                "name".into()
            )],
            aggregates: vec![FunctionExpression::function(
                "sum".into(),
                vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("a"),
                    "Field2".into()
                )],
                45
            )]
        }
    );
}

#[test]
fn namespaced_function() {
    let query = cypher::query(
        "MATCH (a) WHERE a.Field1 = 42 RETURN namespace.function(a.Field2)",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![FunctionExpression::function(
            "namespace.function".into(),
            vec![UnaryExpression::expression_property(
                UnaryExpression::ident("a"),
                "Field2".into()
            )],
            37
        ),])
    );
}

#[test]
fn multiline_function() {
    let query = cypher::query(
        "MATCH (a) WHERE a.Field1 = 42 RETURN 
            function(
                a.Field2,
                a.Field3
            )",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![FunctionExpression::function(
            "function".into(),
            vec![
                UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field2".into()),
                UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field3".into())
            ],
            50
        ),])
    );
}

#[test]
fn match_clause() {
    assert_eq!(
        cypher::query(
            "MATCH (t:Thing)-[:AT]->(wh:Warehouse) RETURN t.name",
            &TEST_CONFIG
        ),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("t".into()), "Thing".into()),
                    path: vec![(
                        RelationMatch::right(Annotation::empty(), vec!["AT".into()], vec![], None),
                        NodeMatch::with_annotation(
                            Annotation::new("wh".into()),
                            "Warehouse".into()
                        )
                    )],
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("t"),
                    "name".into()
                )]),
            }]
        })
    );

    assert_eq!(
        cypher::query(
            "MATCH (t:Thing {category: 1})-[:AT*1..5]->(wh:Warehouse) RETURN t.name",
            &TEST_CONFIG
        ),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::new(
                        Annotation::new("t".into()),
                        vec!["Thing".into()],
                        vec![BinaryExpression::eq(
                            UnaryExpression::property("".into(), "category".into()),
                            UnaryExpression::literal(Literal::Integer(1))
                        )]
                    ),
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["AT".into()],
                            vec![],
                            Some(VariableLengthMatch {
                                min_hops: Some(1),
                                max_hops: Some(5),
                            })
                        ),
                        NodeMatch::with_annotation(
                            Annotation::new("wh".into()),
                            "Warehouse".into()
                        )
                    )],
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("t"),
                    "name".into()
                )]),
            }]
        })
    );

    assert_eq!(
        cypher::query(" MATCH () -> (:LABEL_ONLY) RETURN a.test", &TEST_CONFIG),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::empty(),
                    path: vec![(
                        RelationMatch::right(Annotation::empty(), vec![], vec![], None),
                        NodeMatch::new(Annotation::empty(), vec!["LABEL_ONLY".into()], vec![]),
                    )],
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("a"),
                    "test".into()
                )]),
            }]
        })
    );

    assert_eq!(
        cypher::query(
            "MATCH (a:Person) <-[e:KNOWS]- (b:Person WHERE b.Age > 10) RETURN e.since, b.name",
            &TEST_CONFIG
        ),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("a".into()), "Person".into()),
                    path: vec![(
                        RelationMatch::left(
                            Annotation::new("e".into()),
                            vec!["KNOWS".into()],
                            vec![],
                            None
                        ),
                        NodeMatch::new(
                            Annotation::new("b".into()),
                            vec!["Person".into()],
                            vec![BinaryExpression::gt(
                                UnaryExpression::expression_property(
                                    UnaryExpression::ident("b"),
                                    "Age".into()
                                ),
                                UnaryExpression::literal(Literal::Integer(10))
                            )]
                        )
                    )],
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("e"),
                        "since".into()
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("b"),
                        "name".into()
                    ),
                ]),
            }]
        })
    );
}

#[test]
fn multiple_match_clauses() {
    assert_eq!(
        cypher::query("MATCH (t:Thing)-[:AT]->(wh:Warehouse), (i:Item)-[:CONTAINS]->(c:Component) RETURN t.name", &TEST_CONFIG),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![
                  MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("t".into()), "Thing".into()),
                    path: vec![(
                        RelationMatch::right(Annotation::empty(), vec!["AT".into()], vec![], None),
                        NodeMatch::with_annotation(Annotation::new("wh".into()), "Warehouse".into())
                    )],
                  },
                  MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("i".into()), "Item".into()),
                    path: vec![(
                        RelationMatch::right(Annotation::empty(), vec!["CONTAINS".into()], vec![], None),
                        NodeMatch::with_annotation(Annotation::new("c".into()), "Component".into())
                    )],
                  }
                ],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(UnaryExpression::ident("t"), "name".into())]),
            }]
        })
    );
}

#[test]
fn where_clauses() {
    assert_eq!(
      cypher::query("MATCH (a:Person) -[e:KNOWS]-> (b:Person) WHERE a.age > 42 AND b.name = 'Peter Parker' OR NOT e.fake RETURN e.since", &TEST_CONFIG),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![
                  MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("a".into()), "Person".into()),
                    path: vec![(
                      RelationMatch::right(Annotation::new("e".into()), vec!["KNOWS".into()], vec![], None),
                        NodeMatch::with_annotation(Annotation::new("b".into()), "Person".into())
                    )],
                  }
                ],
                where_clauses: vec![BinaryExpression::or(BinaryExpression::and(
                  BinaryExpression::gt(UnaryExpression::expression_property(UnaryExpression::ident("a"), "age".into()),UnaryExpression::literal(Literal::Integer(42))),
                                                    BinaryExpression::eq(
                                                      UnaryExpression::expression_property(UnaryExpression::ident("b"), "name".into()),
                                                        UnaryExpression::literal(Literal::Text("Peter Parker".into())),
                                                    ),
                                                ),
                                                UnaryExpression::not(
                                                  UnaryExpression::expression_property(UnaryExpression::ident("e"), "fake".into()),
                                                ))],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(UnaryExpression::ident("e"), "since".into())]),
            }]
        })
    );
}

#[test]
fn with_clauses_non_aggregating() {
    assert_eq!(
        cypher::query("MATCH (t:Thing)-[:AT]->(wh:Warehouse) WHERE t.category = 1 WITH wh.name, t RETURN t.name", &TEST_CONFIG),
        Ok(Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::with_annotation(Annotation::new("t".into()), "Thing".into()),
                    path: vec![(
                        RelationMatch::right(Annotation::empty(), vec!["AT".into()], vec![], None),
                        NodeMatch::with_annotation(Annotation::new("wh".into()), "Warehouse".into())
                    )],
                }],
                where_clauses: vec![BinaryExpression::eq(
                  UnaryExpression::expression_property(UnaryExpression::ident("t"), "category".into()),
                  UnaryExpression::literal(Literal::Integer(1))
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(UnaryExpression::ident("wh"), "name".into()), UnaryExpression::ident("t")]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(UnaryExpression::ident("t"), "name".into())]),
            }]
        })
    );
}

#[test]
fn where_clause_non_aggregating_with_date_yyyy_mm_dd() {
    let query =
        cypher::query("MATCH (a) WHERE a.Field1 = date('2023-09-05') RETURN a.name, a.Field2 as F2, a.Field1, $param", &TEST_CONFIG)
            .unwrap();

    assert_eq!(
        query.parts[0].where_clauses,
        vec![BinaryExpression::eq(
            UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field1".into()),
            FunctionExpression::function(
                "date".into(),
                vec![UnaryExpression::literal(Literal::Text("2023-09-05".into()))],
                27
            ),
        )]
    );
}

#[test]
fn where_clause_non_aggregating_with_date_weeks() {
    let query =
        cypher::query("MATCH (a) WHERE a.Field1 = date('2015-W30-2') RETURN a.name, a.Field2 as F2, a.Field1, $param", &TEST_CONFIG)
            .unwrap();

    assert_eq!(
        query.parts[0].where_clauses,
        vec![BinaryExpression::eq(
            UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field1".into()),
            FunctionExpression::function(
                "date".into(),
                vec![UnaryExpression::literal(Literal::Text("2015-W30-2".into()))],
                27
            ),
        )]
    );
}

#[test]
fn where_clause_non_aggregating_with_date_yyyy_quarters() {
    let query =
        cypher::query("MATCH (a) WHERE a.Field1 = date('2015-Q2-02') RETURN a.name, a.Field2 as F2, a.Field1, $param", &TEST_CONFIG)
            .unwrap();

    assert_eq!(
        query.parts[0].where_clauses,
        vec![BinaryExpression::eq(
            UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field1".into()),
            FunctionExpression::function(
                "date".into(),
                vec![UnaryExpression::literal(Literal::Text("2015-Q2-02".into()))],
                27
            ),
        )]
    );
}

#[test]
fn where_clause_non_aggregating_with_local_time() {
    let query =
        cypher::query("MATCH (a) WHERE a.Field1 = localtime('12:58:04') RETURN a.name, a.Field2 as F2, a.Field1, $param", &TEST_CONFIG)
            .unwrap();

    assert_eq!(
        query.parts[0].where_clauses,
        vec![BinaryExpression::eq(
            UnaryExpression::expression_property(UnaryExpression::ident("a"), "Field1".into()),
            FunctionExpression::function(
                "localtime".into(),
                vec![UnaryExpression::literal(Literal::Text("12:58:04".into()))],
                27
            ),
        )]
    );
}

#[test]
fn test_reduce_function() {
    let query = cypher::query(
        "MATCH (a) RETURN reduce(s = 0, x IN [1,2,3] | s + x) AS sum",
        &TEST_CONFIG,
    )
    .unwrap();
    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![UnaryExpression::alias(
            FunctionExpression::function(
                "reduce".into(),
                vec![
                    BinaryExpression::eq(
                        UnaryExpression::ident("s"),
                        UnaryExpression::literal(Literal::Integer(0))
                    ),
                    IteratorExpression::map(
                        "x".into(),
                        ListExpression::list(vec![
                            UnaryExpression::literal(Literal::Integer(1)),
                            UnaryExpression::literal(Literal::Integer(2)),
                            UnaryExpression::literal(Literal::Integer(3)),
                        ]),
                        BinaryExpression::add(
                            UnaryExpression::ident("s"),
                            UnaryExpression::ident("x")
                        )
                    )
                ],
                17,
            ),
            "sum".into()
        )])
    );
}

#[test]
fn test_list_construction() {
    let query = cypher::query(
        "MATCH
    (foo:FOO)
  WITH 
    foo,
    [5, 7, 9, foo.bah] as list
  WHERE
    foo.bah IN list
   RETURN foo, list",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        //  testing WITH
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![
            UnaryExpression::ident("foo"),
            UnaryExpression::alias(
                ListExpression::list(vec![
                    UnaryExpression::literal(Literal::Integer(5)),
                    UnaryExpression::literal(Literal::Integer(7)),
                    UnaryExpression::literal(Literal::Integer(9)),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("foo"),
                        "bah".into()
                    )
                ]),
                "list".into()
            )
        ])
    );
}

#[test]
fn test_list_construction_with_comments_in_between_parts() {
    let query = cypher::query(
        "// This is comment #1
        MATCH
    (foo:FOO)
    // This is comment #2
  WITH 
    foo,
    [5, 7, 9, foo.bah] as list
  WHERE
    foo.bah IN list
    // This is comment #3
    // This is comment #4
   RETURN foo, list",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        //  testing WITH
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![
            UnaryExpression::ident("foo"),
            UnaryExpression::alias(
                ListExpression::list(vec![
                    UnaryExpression::literal(Literal::Integer(5)),
                    UnaryExpression::literal(Literal::Integer(7)),
                    UnaryExpression::literal(Literal::Integer(9)),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("foo"),
                        "bah".into()
                    )
                ]),
                "list".into()
            )
        ])
    );
}

#[test]
fn test_list_construction_with_comments() {
    let query = cypher::query(
        "
        MATCH
        (foo:FOO)
        // A comment in between parts
    WITH 
        foo,
        // A comment within a part
        [5, 7, 9, foo.bah] as list // A comment at the end of a part
    RETURN foo, list // A comment at the end of a query",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        //  testing WITH
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![
            UnaryExpression::ident("foo"),
            UnaryExpression::alias(
                ListExpression::list(vec![
                    UnaryExpression::literal(Literal::Integer(5)),
                    UnaryExpression::literal(Literal::Integer(7)),
                    UnaryExpression::literal(Literal::Integer(9)),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("foo"),
                        "bah".into()
                    )
                ]),
                "list".into()
            )
        ])
    );
}

#[test]
fn test_reflex_query_with_comment() {
    let query = cypher::query(
    "MATCH // A comment in the match clause
    (equip:Equipment {type:'freezer'})-[:HAS_SENSOR]->(:Sensor {type:'temperature'})-[:HAS_VALUE]->(val:SensorValue) 
    // Comment #2
  WITH
    val,
    elementId(equip) AS freezerId,
    val.timestamp - (15 * 60) AS timeRangeStart, // comment #3
    val.timestamp AS timeRangeEnd
  WITH
    freezerId,
    drasi.getVersionsByTimeRange(val, timeRangeStart, timeRangeEnd ) AS sensorValVersions
  WITH 
    freezerId,
    reduce ( minTemp = 10000.0, sensorValVersion IN sensorValVersions | CASE WHEN sensorValVersion.value < minTemp THEN sensorValVersion.value ELSE minTemp END) AS minTempInTimeRange
  WHERE 
    minTempInTimeRange > 32.0
  RETURN
    // A comment in the return clause
    freezerId, minTempInTimeRange
    ", &TEST_CONFIG).unwrap();

    assert_eq!(
        query.parts[0].match_clauses[0],
        MatchClause {
            start: NodeMatch::new(
                Annotation::new("equip".into()),
                vec!["Equipment".into()],
                vec![BinaryExpression::eq(
                    UnaryExpression::property("".into(), "type".into()),
                    UnaryExpression::literal(Literal::Text("freezer".into()))
                )]
            ),
            path: vec![
                (
                    RelationMatch::right(
                        Annotation::empty(),
                        vec!["HAS_SENSOR".into()],
                        vec![],
                        None
                    ),
                    NodeMatch {
                        annotation: Annotation::empty(),
                        labels: vec!["Sensor".into()],
                        property_predicates: vec![BinaryExpression::eq(
                            UnaryExpression::property("".into(), "type".into()),
                            UnaryExpression::literal(Literal::Text("temperature".into()))
                        )]
                    }
                ),
                (
                    RelationMatch::right(
                        Annotation::empty(),
                        vec!["HAS_VALUE".into()],
                        vec![],
                        None
                    ),
                    NodeMatch {
                        annotation: Annotation::new("val".into()),
                        labels: vec!["SensorValue".into()],
                        property_predicates: vec![]
                    }
                )
            ],
        }
    );

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![
            UnaryExpression::ident("val"),
            UnaryExpression::alias(
                FunctionExpression::function(
                    "elementId".into(),
                    vec![UnaryExpression::ident("equip")],
                    195
                ),
                "freezerId".into()
            ),
            UnaryExpression::alias(
                BinaryExpression::subtract(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("val"),
                        "timestamp".into()
                    ),
                    BinaryExpression::multiply(
                        UnaryExpression::literal(Literal::Integer(15)),
                        UnaryExpression::literal(Literal::Integer(60))
                    )
                ),
                "timeRangeStart".into()
            ),
            UnaryExpression::alias(
                UnaryExpression::expression_property(
                    UnaryExpression::ident("val"),
                    "timestamp".into()
                ),
                "timeRangeEnd".into()
            )
        ])
    );
}

#[test]
fn test_function_empty_args() {
    let query = cypher::query("MATCH (a) RETURN datetime()", &TEST_CONFIG).unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![FunctionExpression::function(
            "datetime".into(),
            vec![],
            17
        ),])
    );
}

#[test]
fn where_follows_with_no_alias() {
    let query =
        cypher::query("MATCH (a) WITH a WHERE 1 = 1 RETURN a.Field1", &TEST_CONFIG).unwrap();

    println!("{:#?}", query.parts);

    assert_eq!(2, query.parts.len());

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::Item(vec![UnaryExpression::ident("a")])
    );

    assert_eq!(
        query.parts[1].where_clauses,
        vec![BinaryExpression::eq(
            UnaryExpression::literal(Literal::Integer(1)),
            UnaryExpression::literal(Literal::Integer(1))
        )]
    );

    assert_eq!(
        query.parts[1].return_clause,
        ProjectionClause::Item(vec![UnaryExpression::expression_property(
            UnaryExpression::ident("a"),
            "Field1".into()
        )])
    );
}
