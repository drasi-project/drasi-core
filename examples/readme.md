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

```
Loading initial data...
Initial data loaded.
Component1 - temperature: 23
Results affected: 1
Adding: {"component_name": String("Component1"), "component_temperature": Integer(23), "limit_temperature": Integer(20)}
Component2 - temperature: 13
Results affected: 0
Component1 - temperature: 27
Results affected: 1
Updating: {"component_name": String("Component1"), "component_temperature": Integer(23), "limit_temperature": Integer(20)} -> {"component_name": String("Component1"), "component_temperature": Integer(27), "limit_temperature": Integer(20)}
Component2 - temperature: 11
Results affected: 0
Component1 - temperature: 0
Results affected: 1
Removing: {"component_name": String("Component1"), "component_temperature": Integer(27), "limit_temperature": Integer(20)}
Component2 - temperature: 21
Results affected: 0
```

## Process Monitor

This example models all the running processes on the host machine as nodes within the queryable graph.  The query matches on CPU processes and returns all those where the current CPU usage is above 10%.  As processes jump above and below this limit, entries will be added and removed from the query result set.

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

```
Adding: {"process_cpu_usage": Float(16.280963897705078), "process_name": String("Process 1"), "process_pid": String("35260")}

Adding: {"process_cpu_usage": Float(34.785438537597656), "process_name": String("Process 3"), "process_pid": String("74292")}

Updating: {"process_cpu_usage": Float(16.280963897705078), "process_name": String("Process 2"), "process_pid": String("35260")} -> {"process_cpu_usage": Float(16.114809036254883), "process_name": String("Process 2"), "process_pid": String("35260")}

Removing: {"process_cpu_usage": Float(34.785438537597656), "process_name": String("Process 3"), "process_pid": String("74292")}
```

## Vehicle Location

In this scenario, we have a set of `vehicles` and a set of `zones` where vehicles can be.  The conceptual data model in Drasi is a labeled property graph, so we will add the vehicles and zones as nodes in the graph and we will connect them with a `LOCATED_IN` relationship.

We will create one continuous query, to monitor the vehicles in the `Parking Lot` zone.

```cypher
MATCH 
    (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
RETURN 
    v.color AS color, 
    v.plate AS plate
```

When the `LOCATED_IN` relationship is changed, we will see the vehicle removed or added from the query result.  Changing one of the vehicle properties, such as the `color` will cause the query to emit an update diff.

To run the example us the following command:

```
cargo run --example vehicles
```

```
Result: [Adding { after: {"color": String("Blue"), "plate": String("AAA-1234")} }]
Result: [Updating { before: {"color": String("Blue"), "plate": String("AAA-1234")}, after: {"color": String("Green"), "plate": String("AAA-1234")} }]
Result: [Removing { before: {"color": String("Green"), "plate": String("AAA-1234")} }]
```
