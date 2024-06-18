pub fn min_query() -> &'static str  {
            "
  MATCH (t:Thing)
  RETURN
    min(t.Value) as min_value
    "
}
