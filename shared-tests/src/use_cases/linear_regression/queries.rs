use drasi_query_ast::ast;

pub fn gradient_query() -> ast::Query {
    drasi_query_cypher::parse(
        "
  MATCH (p:Point)
  RETURN
    drasi.linearGradient(p.x, p.y) as Gradient
    ",
    )
    .unwrap()
}
