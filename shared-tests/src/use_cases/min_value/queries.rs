use drasi_query_ast::ast;

pub fn min_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
  MATCH (t:Thing)
  RETURN
    min(t.Value) as min_value
    ",
    )
    .unwrap()
}
