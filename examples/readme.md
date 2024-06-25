# Examples

## Temperature Fluctuations

This example simulates a set of components that track temperature that can be connected to a temperature limit definition.  The example first creates 2 components connected to the same limit of 20.  Then every 3 seconds the temperatures are randomly updated, if one of the components temperature moves from below 20 to above 20, an `adding` result will be emitted by the query.  If one falls from above 20 to below 20, then a `removing` result will be emited by the query.  If the temperature of a component was already above 20 and changed to another value above 20, then an `updating` result will be emitted.

Note: Changing the value of the `Limit` node would also produce results on the query.

The cypher query is a follows:

```cypher
MATCH 
    (c:Component)-[:HAS_LIMIT]->(l:Limit) 
WHERE c.temperature > l.max_temperature 
RETURN 
    c.name AS component_name, 
    c.temperature AS component_temperature, 
    l.max_temperature AS limit_temperature
```


To run the example us the following command:

```
cargo run --example temperature
```

## Process Monitor

```cypher
MATCH 
    (p:Process)
WHERE p.cpu_usage > 10
RETURN 
    p.pid AS process_pid,
    p.name AS process_name, 
    p.cpu_usage AS process_cpu_usage
```


To run the example us the following command:

```
cargo run --example process_monitor
```
