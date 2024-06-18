pub fn gradient_query() -> &'static str {
    "MATCH (p:Point)
  RETURN
    drasi.linearGradient(p.x, p.y) as Gradient
    "
}
