// Copyright 2025 The Drasi Authors.
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
use drasi_query_cypher::{parse, CypherConfiguration};

struct TestConfig {}

impl GQLConfiguration for TestConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("count".into());
        set
    }
}

static TEST_CONFIG: TestConfig = TestConfig {};

// GROUP BY tests
#[test]
fn implicit_grouping_with_one_key() {
    // Implicit Grouping with One Key
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone {type:'Parking Lot'})
        RETURN z.type AS zone_type, count(v) AS vehicle_count",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![UnaryExpression::alias(
                UnaryExpression::expression_property(UnaryExpression::ident("z"), "type".into()),
                "zone_type".into()
            )],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("v")], 99),
                "vehicle_count".into()
            )]
        }
    );
}

#[test]
fn implicit_grouping_with_two_keys() {
    // Implicit Grouping with Two Keys
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone {type:'Parking Lot'})
         RETURN z.type AS zone_type, v.color AS vehicle_color, count(v) AS vehicle_count",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![
                UnaryExpression::alias(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("z"),
                        "type".into()
                    ),
                    "zone_type".into()
                ),
                UnaryExpression::alias(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into()
                    ),
                    "vehicle_color".into()
                )
            ],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function(
                    "count".into(),
                    vec![UnaryExpression::ident("v")],
                    126
                ),
                "vehicle_count".into()
            )]
        }
    );
}

#[test]
fn explicit_group_by_all_keys_projected() {
    // Explicit GROUP BY: All Keys Projected
    // Ensures explicit GROUP BY behaves identically to implicit grouping.
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         RETURN z.type AS zone_type, v.color AS vehicle_color, count(v) AS vehicle_count
         GROUP BY zone_type, vehicle_color",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![
                UnaryExpression::alias(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("z"),
                        "type".into()
                    ),
                    "zone_type".into()
                ),
                UnaryExpression::alias(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into()
                    ),
                    "vehicle_color".into()
                )
            ],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function(
                    "count".into(),
                    vec![UnaryExpression::ident("v")],
                    105
                ),
                "vehicle_count".into()
            )]
        }
    );
}

#[test]
fn explicit_group_by_subset_of_keys_projected() {
    // Explicit GROUP BY: Subset of Keys Projected
    // Creates a multi-part query
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         RETURN z.type AS zone_type, count(v) AS vehicle_count
         GROUP BY zone_type, v.color",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query,
        Query {
            parts: vec![
                // First part: Group by all specified keys
                QueryPart {
                    match_clauses: vec![MatchClause {
                        start: NodeMatch::with_annotation(
                            Annotation::new("v".into()),
                            "Vehicle".into()
                        ),
                        path: vec![(
                            RelationMatch::right(
                                Annotation::empty(),
                                vec!["LOCATED_IN".into()],
                                vec![],
                                None
                            ),
                            NodeMatch::with_annotation(Annotation::new("z".into()), "Zone".into())
                        )],
                        optional: false,
                    }],
                    where_clauses: vec![],
                    return_clause: ProjectionClause::GroupBy {
                        grouping: vec![
                            UnaryExpression::alias(
                                UnaryExpression::expression_property(
                                    UnaryExpression::ident("z"),
                                    "type".into()
                                ),
                                "zone_type".into()
                            ),
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into()
                            )
                        ],
                        aggregates: vec![UnaryExpression::alias(
                            FunctionExpression::function(
                                "count".into(),
                                vec![UnaryExpression::ident("v")],
                                79
                            ),
                            "vehicle_count".into()
                        )]
                    }
                },
                // Second part: Final projection with only subset of keys
                QueryPart {
                    match_clauses: vec![],
                    where_clauses: vec![],
                    return_clause: ProjectionClause::Item(vec![
                        UnaryExpression::ident("zone_type"),
                        UnaryExpression::ident("vehicle_count")
                    ])
                }
            ]
        }
    );
}

#[test]
fn group_by_with_function_expression() {
    // GROUP BY with Function Expression
    let query = gql::query(
        "MATCH (a)-[t:Transfers]->(b)
         RETURN FLOOR(t.amount) AS amount_group, count(t) AS number_of_transfers
         GROUP BY amount_group",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![UnaryExpression::alias(
                FunctionExpression::function(
                    "FLOOR".into(),
                    vec![UnaryExpression::expression_property(
                        UnaryExpression::ident("t"),
                        "amount".into()
                    )],
                    45
                ),
                "amount_group".into()
            )],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("t")], 78),
                "number_of_transfers".into()
            )]
        }
    );
}

#[test]
fn group_by_with_binary_expression() {
    // GROUP BY with Binary Expression
    let query = gql::query(
        "MATCH (a)-[t:Transfers]->(b)
         RETURN t.amount + 100, count(t) AS number_of_transfers
         GROUP BY t.amount + 100",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![BinaryExpression::add(
                UnaryExpression::expression_property(UnaryExpression::ident("t"), "amount".into()),
                UnaryExpression::literal(Literal::Integer(100))
            )],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("t")], 61),
                "number_of_transfers".into()
            )]
        }
    );
}

#[test]
fn group_by_with_aliased_column() {
    // GROUP BY with Aliased Column
    let query = gql::query(
        "MATCH (a)-[t:Transfers]->(b)
         RETURN t.account_id AS account, count(t) AS number_of_transfers
         GROUP BY account",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![UnaryExpression::alias(
                UnaryExpression::expression_property(
                    UnaryExpression::ident("t"),
                    "account_id".into()
                ),
                "account".into()
            )],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("t")], 70),
                "number_of_transfers".into()
            )]
        }
    );
}

#[test]
fn group_by_with_account_id_and_count() {
    // GROUP BY with Account ID and Count
    let query = gql::query(
        "MATCH (a)-[t:Transfers]->(b)
            RETURN t.account_id, count(t) AS number_of_transfers
            GROUP BY t.account_id",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query,
        Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch::new(Annotation::new("a".into()), vec![], vec![]),
                    path: vec![(
                        RelationMatch::right(
                            Annotation::new("t".into()),
                            vec!["Transfers".into()],
                            vec![],
                            None
                        ),
                        NodeMatch::new(Annotation::new("b".into()), vec![], vec![])
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![UnaryExpression::expression_property(
                        UnaryExpression::ident("t"),
                        "account_id".into()
                    )],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("t")],
                            62
                        ),
                        "number_of_transfers".into()
                    )]
                }
            }]
        }
    );
}

#[test]
fn group_by_empty() {
    // GROUP BY ()
    // Tests the special case where GROUP BY () groups all rows into a single group.
    let query = gql::query(
        "MATCH (v:Vehicle) RETURN count(v) AS total_rows GROUP BY ()",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("v")], 25),
                "total_rows".into()
            )]
        }
    );
}

#[test]
fn implicit_grouping_with_only_aggregates() {
    // Implicit Grouping with Only Aggregates
    // Tests that when RETURN contains only aggregate functions with no explicit GROUP BY,
    let query = gql::query(
        "MATCH (v:Vehicle) 
         RETURN count(v) AS total",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("v")], 35),
                "total".into()
            )]
        }
    );
}

#[test]
fn grouping_on_raw_identifiers() {
    // Grouping on Raw Identifiers (No Alias)
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone) 
         RETURN z.type, count(v) AS vehicle_count 
         GROUP BY z.type",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![UnaryExpression::expression_property(
                UnaryExpression::ident("z"),
                "type".into()
            )],
            aggregates: vec![UnaryExpression::alias(
                FunctionExpression::function("count".into(), vec![UnaryExpression::ident("v")], 67),
                "vehicle_count".into()
            )]
        }
    );
}

#[test]
fn grouping_on_non_aliased_function() {
    // Grouping on Non-aliased Function
    let query = gql::query(
        "MATCH (a)-[t:Transfers]->(b) 
         RETURN floor(t.amount), count(t) 
         GROUP BY floor(t.amount)",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query.parts[0].return_clause,
        ProjectionClause::GroupBy {
            grouping: vec![FunctionExpression::function(
                "floor".into(),
                vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("t"),
                    "amount".into()
                )],
                46
            )],
            aggregates: vec![FunctionExpression::function(
                "count".into(),
                vec![UnaryExpression::ident("t")],
                63
            )]
        }
    );
}
#[test]
fn group_by_and_where_on_vehicles() {
    // This test checks GROUP BY and WHERE together
    let query = gql::query(
        "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         WHERE v.color = 'Red'
         RETURN z.type, count(v) AS vehicle_count
         GROUP BY z.type
        ",
        &TEST_CONFIG,
    )
    .unwrap();

    assert_eq!(
        query,
        Query {
            parts: vec![QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch {
                            direction: Direction::Right,
                            annotation: Annotation { name: None },
                            variable_length: None,
                            labels: vec!["LOCATED_IN".into()],
                            property_predicates: vec![],
                        },
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    ),],
                    optional: false,
                },],
                where_clauses: vec![BinaryExpression::eq(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into()
                    ),
                    UnaryExpression::literal(Literal::Text("Red".into()))
                ),],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![UnaryExpression::expression_property(
                        UnaryExpression::ident("z"),
                        "type".into()
                    ),],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            97
                        ),
                        "vehicle_count".into()
                    ),],
                },
            },],
        }
    );
}

// LET and YIELD Tests

// Shared Cypher test config for AST comparison
struct TestCypherConfig {}
impl CypherConfiguration for TestCypherConfig {
    fn get_aggregating_function_names(&self) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        set.insert("count".into());
        set
    }
}

#[test]
fn simple_let_assignment() {
    let gql_query = "MATCH (v:Vehicle)
         LET isRed = v.color = 'Red'
         RETURN v.color, isRed";
    let cypher_query = "MATCH (v:Vehicle)
        WITH v, v.color = 'Red' AS isRed
        RETURN v.color, isRed";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn multiple_let_variables_in_one_clause() {
    let gql_query = "MATCH (a:Account)
         LET active = a.is_blocked = false, nameLength = LENGTH(a.nick_name)
         RETURN a.nick_name, active, nameLength";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("a".into()),
                        },
                        labels: vec!["Account".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("a"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("a"),
                                "is_blocked".into(),
                            ),
                            UnaryExpression::literal(Literal::Boolean(false)),
                        ),
                        "active".into(),
                    ),
                    UnaryExpression::alias(
                        FunctionExpression::function(
                            "LENGTH".into(),
                            vec![UnaryExpression::expression_property(
                                UnaryExpression::ident("a"),
                                "nick_name".into(),
                            )],
                            75,
                        ),
                        "nameLength".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("a"),
                        "nick_name".into(),
                    ),
                    UnaryExpression::ident("active"),
                    UnaryExpression::ident("nameLength"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn chained_let_clauses_preserving_all_variables() {
    let gql_query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         LET inGarage = z.type = 'Garage'
         RETURN v.color, z.type, isRed, inGarage";
    let cypher_query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
        WITH v, z, v.color = 'Red' AS isRed
        WITH v, z, isRed, z.type = 'Garage' AS inGarage
        RETURN v.color, z.type, isRed, inGarage";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn test_let_with_where_clause() {
    let gql_query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    WHERE z.type = 'Garage'
    LET color = v.color
    RETURN color";
    let cypher_query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    WHERE z.type = 'Garage'
    WITH v, z, v.color as color
    RETURN color";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn let_with_conditionals() {
    let gql_query = "MATCH (a:Account)
         LET status = CASE WHEN a.is_blocked THEN 'Blocked' ELSE 'Active' END
         RETURN a.nick_name, status";
    let cypher_query = "MATCH (a:Account)
        WITH a, CASE WHEN a.is_blocked THEN 'Blocked' ELSE 'Active' END AS status
        RETURN a.nick_name, status";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn chained_lets_with_multiple_new_variables() {
    let gql_query = "MATCH (p:Person)
         LET nameLength = LENGTH(p.name)
         LET isShortName = nameLength < 5, isLongName = nameLength > 7
         RETURN p.name, isShortName, isLongName";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("p".into()),
                        },
                        labels: vec!["Person".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("p"),
                    UnaryExpression::alias(
                        FunctionExpression::function(
                            "LENGTH".into(),
                            vec![UnaryExpression::expression_property(
                                UnaryExpression::ident("p"),
                                "name".into(),
                            )],
                            43,
                        ),
                        "nameLength".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("p"),
                    UnaryExpression::ident("nameLength"),
                    UnaryExpression::alias(
                        BinaryExpression::lt(
                            UnaryExpression::ident("nameLength"),
                            UnaryExpression::literal(Literal::Integer(5)),
                        ),
                        "isShortName".into(),
                    ),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::ident("nameLength"),
                            UnaryExpression::literal(Literal::Integer(7)),
                        ),
                        "isLongName".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("p"),
                        "name".into(),
                    ),
                    UnaryExpression::ident("isShortName"),
                    UnaryExpression::ident("isLongName"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

// GROUP BY with LET tests

#[test]
fn group_by_let_defined_variable() {
    // Example 1: Group by LET-Defined Variable
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // RETURN z.type AS zone_type, isRed, count(v) AS vehicle_count

    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         RETURN z.type AS zone_type, isRed, count(v) AS vehicle_count
         GROUP BY zone_type, isRed";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isRed"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            123,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn multiple_let_variables_in_group_by() {
    // Example 2: Multiple LET Variables in GROUP BY
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // WITH v, z, isRed, v.color = 'Blue' AS isBlue
    // RETURN zone_type, isRed, isBlue, count(v) AS vehicle_count

    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         LET isBlue = v.color = 'Blue'
         RETURN z.type AS zone_type, isRed, isBlue, count(v) AS vehicle_count
         GROUP BY zone_type, isRed, isBlue";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::ident("isRed"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Blue".into())),
                        ),
                        "isBlue".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isRed"),
                        UnaryExpression::ident("isBlue"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            170,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn group_by_let_defined_variable_with_less_projected_columns() {
    // Example 3: Group by LET-Defined Variable with less Projected Columns
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // WITH z.type AS zone_type, isRed, count(v) AS vehicle_count
    // RETURN zone_type, vehicle_count
    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         RETURN z.type AS zone_type, count(v) AS vehicle_count
         GROUP BY zone_type, isRed";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();
    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isRed"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            116,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("zone_type"),
                    UnaryExpression::ident("vehicle_count"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn implicit_grouping_with_let() {
    // Implicit grouping with LET
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // RETURN z.type AS zone_type, isRed, count(v) AS vehicle_count
    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         RETURN z.type AS zone_type, isRed, count(v) AS vehicle_count";
    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();
    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isRed"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            123,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };
    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn implicit_grouping_with_multiple_let() {
    // Implicit grouping with multiple LET
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // WITH v, z, isRed, v.color = 'Blue' AS isBlue
    // RETURN z.type AS zone_type, isRed, isBlue, count(v) AS vehicle_count
    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         LET isBlue = v.color = 'Blue'
         RETURN z.type AS zone_type, isRed, isBlue, count(v) AS vehicle_count";
    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();
    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::ident("isRed"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Blue".into())),
                        ),
                        "isBlue".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isRed"),
                        UnaryExpression::ident("isBlue"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            170,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };
    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn let_variable_not_used_in_group_by_or_return() {
    // LET Variable Not Used in GROUP BY or RETURN
    // MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
    // WITH v, z, v.color = 'Red' AS isRed
    // RETURN z.type AS zone_type, count(v) AS vehicle_count

    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isRed = v.color = 'Red'
         RETURN z.type AS zone_type, count(v) AS vehicle_count
         GROUP BY zone_type";
    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();
    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("z"),
                            "type".into(),
                        ),
                        "zone_type".into(),
                    )],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            116,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };
    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

// YIELD tests
#[test]
fn simple_yield() {
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD v.color AS vehicleColor, z.type AS location
         RETURN vehicleColor, location";
    let cypher_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
        WITH v.color AS vehicleColor, z.type AS location
        RETURN vehicleColor, location";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn simple_yield_on_property() {
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD v.color
         RETURN v.color";
    let cypher_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
        WITH v.color
        RETURN v.color";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn yield_single_identifier() {
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD v
         RETURN v.color";
    let cypher_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
        WITH v
        RETURN v.color";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn yield_with_let_and_chained_yield() {
    let gql_query = "MATCH (p:Product)
         LET productName = p.name, cost = p.price
         YIELD productName, cost
         LET total = cost * 1.2
         YIELD total AS finalPrice
         RETURN finalPrice";
    let cypher_query = "MATCH (p:Product)
        WITH p, p.name AS productName, p.price AS cost
        WITH productName, cost
        WITH productName, cost, cost * 1.2 AS total
        WITH total AS finalPrice
        RETURN finalPrice";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn yield_with_where() {
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         WHERE v.color = 'Red'
         YIELD v.color AS vehicleColor, z.type AS location
         RETURN vehicleColor, location";
    let cypher_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
        WHERE v.color = 'Red'
        WITH v.color AS vehicleColor, z.type AS location
        RETURN vehicleColor, location";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();
    let cypher_ast = parse(cypher_query, &TestCypherConfig {}).unwrap();

    assert_eq!(gql_ast, cypher_ast, "GQL and Cypher ASTs should be equal");
}

#[test]
fn yield_with_group_by() {
    // MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
    // WITH z.type AS zone_type, v.color AS vehicle_color
    // RETURN zone_type, vehicle_color, count(1) AS vehicle_count
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD z.type AS zone_type, v.color AS vehicle_color
         RETURN zone_type, vehicle_color, count(1) AS vehicle_count
         GROUP BY zone_type, vehicle_color";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch {
                            direction: Direction::Right,
                            annotation: Annotation {
                                name: Some("e".into()),
                            },
                            variable_length: None,
                            labels: vec!["LOCATED_IN".into()],
                            property_predicates: vec![],
                        },
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("z"),
                            "type".into(),
                        ),
                        "zone_type".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "vehicle_color".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::ident("zone_type"),
                        UnaryExpression::ident("vehicle_color"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::literal(Literal::Integer(1))],
                            146,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn yield_with_group_by_fewer_columns_projected() {
    // MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
    // WITH z.type AS zone_type, v.color AS vehicle_color
    // WITH zone_type, vehicle_color, count(1) AS vehicle_count
    // RETURN zone_type, vehicle_count

    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD z.type AS zone_type, v.color AS vehicle_color
         RETURN zone_type, count(1) AS vehicle_count
         GROUP BY zone_type, vehicle_color";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch {
                            direction: Direction::Right,
                            annotation: Annotation {
                                name: Some("e".into()),
                            },
                            variable_length: None,
                            labels: vec!["LOCATED_IN".into()],
                            property_predicates: vec![],
                        },
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("z"),
                            "type".into(),
                        ),
                        "zone_type".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "vehicle_color".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::ident("zone_type"),
                        UnaryExpression::ident("vehicle_color"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::literal(Literal::Integer(1))],
                            131,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("zone_type"),
                    UnaryExpression::ident("vehicle_count"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure"
    );
}

#[test]
fn yield_let_and_group_by_together() {
    // Equivalent Cypher:
    // MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
    // WHERE v.color = 'Red'
    // WITH v, z, v.color = 'Red' AS isRed
    // WITH v, z, isRed, v.price > 50000 AS isExpensive
    // WITH z.type AS zone_type, v.color AS vehicle_color, isRed, isExpensive
    // WITH zone_type, isRed, isExpensive, count(1) AS vehicle_count
    // RETURN zone_type, isRed, vehicle_count
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         WHERE v.color = 'Red'
         LET isRed = v.color = 'Red'
         LET isExpensive = v.price > 50000
         YIELD z.type AS zone_type, v.color AS vehicle_color, isRed, isExpensive
         RETURN zone_type, isRed, count(1) AS vehicle_count
         GROUP BY zone_type, isRed, isExpensive";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch {
                            direction: Direction::Right,
                            annotation: Annotation {
                                name: Some("e".into()),
                            },
                            variable_length: None,
                            labels: vec!["LOCATED_IN".into()],
                            property_predicates: vec![],
                        },
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![BinaryExpression::eq(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::literal(Literal::Text("Red".into())),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "color".into(),
                            ),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::ident("isRed"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "price".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(50000)),
                        ),
                        "isExpensive".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("z"),
                            "type".into(),
                        ),
                        "zone_type".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "vehicle_color".into(),
                    ),
                    UnaryExpression::ident("isRed"),
                    UnaryExpression::ident("isExpensive"),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::ident("zone_type"),
                        UnaryExpression::ident("isRed"),
                        UnaryExpression::ident("isExpensive"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::literal(Literal::Integer(1))],
                            269,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("zone_type"),
                    UnaryExpression::ident("isRed"),
                    UnaryExpression::ident("vehicle_count"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure with YIELD, LET, and GROUP BY combined"
    );
}

#[test]
fn yield_then_let() {
    // Equivalent Cypher:
    // MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
    // WITH v.color AS vehicleColor, z.type AS location
    // WITH vehicleColor, location, vehicleColor = 'Red' AS isRed
    // RETURN vehicleColor, location, isRed
    let gql_query = "MATCH (v:Vehicle)-[e:LOCATED_IN]->(z:Zone)
         YIELD v.color AS vehicleColor, z.type AS location
         LET isRed = vehicleColor = 'Red'
         RETURN vehicleColor, location, isRed";

    let gql_ast = gql::query(gql_query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch {
                            direction: Direction::Right,
                            annotation: Annotation {
                                name: Some("e".into()),
                            },
                            variable_length: None,
                            labels: vec!["LOCATED_IN".into()],
                            property_predicates: vec![],
                        },
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "vehicleColor".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("z"),
                            "type".into(),
                        ),
                        "location".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("vehicleColor"),
                    UnaryExpression::ident("location"),
                    UnaryExpression::alias(
                        BinaryExpression::eq(
                            UnaryExpression::ident("vehicleColor"),
                            UnaryExpression::literal(Literal::Text("Red".into())),
                        ),
                        "isRed".into(),
                    ),
                ]),
            },
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("vehicleColor"),
                    UnaryExpression::ident("location"),
                    UnaryExpression::ident("isRed"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for YIELD then LET"
    );
}

// FILTER Tests
#[test]
fn simple_filter() {
    // MATCH (v:Vehicle)
    // FILTER v.miles > 60000
    // RETURN v.color, v.miles
    let query = "MATCH (v:Vehicle)
         FILTER v.miles > 60000
         RETURN v.color, v.miles";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for simple FILTER"
    );
}

#[test]
fn multiple_filters() {
    // MATCH (v:Vehicle)
    // FILTER v.color = 'Red'
    // FILTER v.miles > 60000
    // RETURN v.color, v.miles
    let query = "MATCH (v:Vehicle)
         FILTER v.color = 'Red'
         FILTER v.miles > 60000
         RETURN v.color, v.miles";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by color
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::eq(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::literal(Literal::Text("Red".into())),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for multiple FILTERs"
    );
}

#[test]
fn filter_with_let() {
    // MATCH (v:Vehicle)
    // LET isHighMileage = v.miles >= 60000
    // FILTER isHighMileage
    // RETURN v.color
    let query = "MATCH (v:Vehicle)
         LET isHighMileage = v.miles >= 60000
         FILTER isHighMileage
         RETURN v.color";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and define LET variable
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::ge(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "isHighMileage".into(),
                    ),
                ]),
            },
            // Second query part: Filter by LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("isHighMileage")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("isHighMileage"),
                ]),
            },
            // Third query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::expression_property(
                    UnaryExpression::ident("v"),
                    "color".into(),
                )]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER with LET"
    );
}

#[test]
fn filter_let_return_vehicle() {
    let query = "MATCH (v:Vehicle)
         FILTER v.miles > 60000
         LET highMileage = v.miles > 60000
         RETURN v.color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Define LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER, LET, and RETURN"
    );
}

#[test]
fn filter_yield_return_color() {
    let query = "MATCH (v:Vehicle)
         FILTER v.miles > 60000
         YIELD v.color AS color
         RETURN color";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Yield v.color as color
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::alias(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    "color".into(),
                )]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("color")]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER, YIELD, and RETURN"
    );
}

#[test]
fn yield_filter_return_color() {
    let query = "MATCH (v:Vehicle)
         YIELD v.color AS color, v.miles AS miles
         FILTER miles > 60000
         RETURN color";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and yield color/miles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        "miles".into(),
                    ),
                ]),
            },
            // Second query part: Filter miles > 60000
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::ident("miles"),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                ]),
            },
            // Third query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("color")]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for YIELD, FILTER, and RETURN"
    );
}

#[test]
fn filter_let_yield_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         FILTER v.miles > 60000
         LET highMileage = v.miles > 60000
         YIELD v.color AS color, highMileage
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Define LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Fourth query part: Yield v.color as color, highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Fifth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER, LET, YIELD, and RETURN"
    );
}

#[test]
fn filter_yield_let_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         FILTER v.miles > 60000
         YIELD v.color AS color, v.miles AS miles
         LET highMileage = miles > 60000
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: Filter by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Yield v.color as color, v.miles as miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        "miles".into(),
                    ),
                ]),
            },
            // Fourth query part: Define LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::ident("miles"),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Fifth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER, YIELD, LET, and RETURN"
    );
}

#[test]
fn let_filter_yield_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         LET highMileage = v.miles > 60000
         FILTER highMileage
         YIELD v.color AS color, highMileage
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and define LET variable
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Second query part: Filter by highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("highMileage")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Third query part: Yield v.color as color, highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for LET, FILTER, YIELD, and RETURN"
    );
}

#[test]
fn let_yield_filter_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         LET highMileage = v.miles > 60000
         YIELD v.color AS color, highMileage
         FILTER highMileage
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and define LET variable
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Second query part: Yield v.color as color, highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Third query part: Filter by highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("highMileage")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for LET, YIELD, FILTER, and RETURN"
    );
}

#[test]
fn yield_filter_let_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         YIELD v.color AS color, v.miles AS miles
         FILTER miles > 60000
         LET highMileage = miles > 60000
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and yield color/miles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        "miles".into(),
                    ),
                ]),
            },
            // Second query part: Filter miles > 60000
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::ident("miles"),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                ]),
            },
            // Fourth query part: Define LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::ident("miles"),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Fifth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for YIELD, FILTER, LET, and RETURN"
    );
}

#[test]
fn yield_let_filter_return_color_highmileage() {
    let query = "MATCH (v:Vehicle)
         YIELD v.color AS color, v.miles AS miles
         LET highMileage = miles > 60000
         FILTER highMileage
         RETURN color, highMileage";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and yield color/miles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        "miles".into(),
                    ),
                ]),
            },
            // Second query part: Define LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::ident("miles"),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "highMileage".into(),
                    ),
                ]),
            },
            // Third query part: Filter by highMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("highMileage")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
            // Fifth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("highMileage"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for YIELD, LET, FILTER, and RETURN"
    );
}

#[test]
fn where_and_filter_together() {
    // Test combining WHERE and FILTER clauses
    // WHERE filters during the MATCH phase, FILTER filters after projection
    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         WHERE v.color = 'Red'
         FILTER v.miles > 60000
         RETURN v.color, v.miles, z.type";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles with WHERE clause
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![BinaryExpression::eq(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::literal(Literal::Text("Red".into())),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                ]),
            },
            // Second query part: FILTER by miles
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::literal(Literal::Integer(60000)),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                ]),
            },
            // Third query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("z"),
                        "type".into(),
                    ),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for WHERE and FILTER together"
    );
}

#[test]
fn let_filter_group_by_together() {
    // Test combining LET, FILTER, and GROUP BY clauses
    // This tests the interaction between variable definition, filtering, and aggregation
    let query = "MATCH (v:Vehicle)-[:LOCATED_IN]->(z:Zone)
         LET isHighMileage = v.miles > 60000
         LET isExpensive = v.price > 50000
         FILTER isHighMileage
         RETURN z.type AS zone_type, isExpensive, count(v) AS vehicle_count
         GROUP BY zone_type, isExpensive";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and define LET variables
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![(
                        RelationMatch::right(
                            Annotation::empty(),
                            vec!["LOCATED_IN".into()],
                            vec![],
                            None,
                        ),
                        NodeMatch {
                            annotation: Annotation {
                                name: Some("z".into()),
                            },
                            labels: vec!["Zone".into()],
                            property_predicates: vec![],
                        },
                    )],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "miles".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(60000)),
                        ),
                        "isHighMileage".into(),
                    ),
                ]),
            },
            // Second query part: Define second LET variable
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::ident("isHighMileage"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "price".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(50000)),
                        ),
                        "isExpensive".into(),
                    ),
                ]),
            },
            // Third query part: Filter by isHighMileage
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("isHighMileage")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("z"),
                    UnaryExpression::ident("isHighMileage"),
                    UnaryExpression::ident("isExpensive"),
                ]),
            },
            // Fourth query part: Group by zone_type and isExpensive
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::GroupBy {
                    grouping: vec![
                        UnaryExpression::alias(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("z"),
                                "type".into(),
                            ),
                            "zone_type".into(),
                        ),
                        UnaryExpression::ident("isExpensive"),
                    ],
                    aggregates: vec![UnaryExpression::alias(
                        FunctionExpression::function(
                            "count".into(),
                            vec![UnaryExpression::ident("v")],
                            210,
                        ),
                        "vehicle_count".into(),
                    )],
                },
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for LET, FILTER, and GROUP BY together"
    );
}

#[test]
fn let_filter_yield_with_param() {
    // Test LET, FILTER, YIELD, and $param usage
    let query = "MATCH (v:Vehicle)
         LET threshold = $param
         FILTER v.miles > threshold
         YIELD v.color AS color, v.miles AS miles, threshold
         RETURN color, miles, threshold";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles and define LET variable
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        UnaryExpression::parameter("param".into()),
                        "threshold".into(),
                    ),
                ]),
            },
            // Second query part: Filter by v.miles > threshold
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::gt(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::ident("threshold"),
                )],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("threshold"),
                ]),
            },
            // Third query part: Yield v.color as color, v.miles as miles, threshold
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "color".into(),
                        ),
                        "color".into(),
                    ),
                    UnaryExpression::alias(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        "miles".into(),
                    ),
                    UnaryExpression::ident("threshold"),
                ]),
            },
            // Fourth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("color"),
                    UnaryExpression::ident("miles"),
                    UnaryExpression::ident("threshold"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for LET, FILTER, YIELD, and $param"
    );
}

#[test]
fn filter_filter_let_filter() {
    // Test the sequence FILTER FILTER LET FILTER

    let query = "MATCH (v:Vehicle)
         FILTER v.color = 'Red'
         FILTER v.miles > 50000 AND v.miles < 100000
         LET isExpensive = v.price > 40000
         FILTER isExpensive
         RETURN v.color, v.miles, v.price, isExpensive";

    let gql_ast = gql::query(query, &TEST_CONFIG).unwrap();

    let expected_ast = Query {
        parts: vec![
            // First query part: Match vehicles
            QueryPart {
                match_clauses: vec![MatchClause {
                    start: NodeMatch {
                        annotation: Annotation {
                            name: Some("v".into()),
                        },
                        labels: vec!["Vehicle".into()],
                        property_predicates: vec![],
                    },
                    path: vec![],
                    optional: false,
                }],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Second query part: First FILTER - v.color = 'Red'
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::eq(
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::literal(Literal::Text("Red".into())),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Third query part: Second FILTER - v.miles > 50000 AND v.miles < 100000
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![BinaryExpression::and(
                    BinaryExpression::gt(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        UnaryExpression::literal(Literal::Integer(50000)),
                    ),
                    BinaryExpression::lt(
                        UnaryExpression::expression_property(
                            UnaryExpression::ident("v"),
                            "miles".into(),
                        ),
                        UnaryExpression::literal(Literal::Integer(100000)),
                    ),
                )],
                return_clause: ProjectionClause::Item(vec![UnaryExpression::ident("v")]),
            },
            // Fourth query part: LET - define isExpensive
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::alias(
                        BinaryExpression::gt(
                            UnaryExpression::expression_property(
                                UnaryExpression::ident("v"),
                                "price".into(),
                            ),
                            UnaryExpression::literal(Literal::Integer(40000)),
                        ),
                        "isExpensive".into(),
                    ),
                ]),
            },
            // Fifth query part: Third FILTER - isExpensive
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![UnaryExpression::ident("isExpensive")],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::ident("v"),
                    UnaryExpression::ident("isExpensive"),
                ]),
            },
            // Sixth query part: Final projection
            QueryPart {
                match_clauses: vec![],
                where_clauses: vec![],
                return_clause: ProjectionClause::Item(vec![
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "color".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "miles".into(),
                    ),
                    UnaryExpression::expression_property(
                        UnaryExpression::ident("v"),
                        "price".into(),
                    ),
                    UnaryExpression::ident("isExpensive"),
                ]),
            },
        ],
    };

    assert_eq!(
        gql_ast, expected_ast,
        "GQL AST should match expected structure for FILTER FILTER LET FILTER sequence"
    );
}
