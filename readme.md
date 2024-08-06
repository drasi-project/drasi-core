# Drasi Core

Drasi-core is the library used by [Drasi](https://github.com/drasi-project/drasi-platform) to implement continuous queries.

Continuous Queries, as the name implies, are queries that run continuously. To understand what is unique about them, it is useful to contrast them with the kind of instantaneous queries developers are accustomed to running against databases.

When you execute an instantaneous query, you are running the query against the database at a point in time. The database calculates the results to the query and returns them. While you work with those results, you are working with a static snapshot of the data and are unaware of any changes that may have happened to the data after you ran the query. If you run the same instantaneous query periodically, the query results might be different each time due to changes made to the data by other processes. But to understand what has changed, you would need to compare the most recent result with the previous result.

Continuous Queries, once started, continue to run until they are stopped. While running, Continuous Queries will process any changes flowing from one or more data sources, compute how the query result is affected and emit the diff.

Continuous Queries are implemented as graph queries written in the Cypher Query Language. The use of a declarative graph query language means you can express rich query logic that takes into consideration both the properties of the data you are querying and the relationships between data in a single query.

Drasi-core is the internal library used by [Drasi](https://github.com/drasi-project/drasi-platform) to implement continuous queries. Drasi itself is a much broader solution with many more moving parts.  Drasi-core can be used stand-alone from Drasi for embedded scenarios, where continuous queries could run in-process inside an application.

## Example

In this scenario, we have a set of `Vehicles` and a set of `Zones` where vehicles can be.  The conceptual data model in Drasi is a labeled property graph, so we will add the vehicles and zones as nodes in the graph and we will connect them with a `LOCATED_IN` relationship.

We will create a Continuous Query to observe the `Parking Lot` Zone so that we will get notified when any Vehicle enters or exits the Zone.

```cypher
MATCH 
    (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
RETURN 
    v.color AS color, 
    v.plate AS plate
```

When the `LOCATED_IN` relationship is added or deleted, the Continuous Query will emit a diff stating that the Vehicle was added to or removed from the query result. And changing one of the Vehicle properties, such as the `color`, will cause the query to emit a diff stating the Vehicle has been updated.

Let's look at how to configure a Continuous Query using the `QueryBuilder`.

```rust
let query_str = "
    MATCH 
        (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
    RETURN 
        v.color AS color, 
        v.plate AS plate";

let query_builder = QueryBuilder::new(query_str);
let query = query_builder.build().await;
```

Let's load a Vehicle (v1) and a Zone (z1) as nodes into the query.
We can do this by processsing a `SourceChange::Insert` into the query, this in turn takes an `Element`, which can be of either a `Element::Node` or `Element::Relation`, which represent nodes and relations in the graph model that can be queried.  When constructing an `Element`, you will also need to supply `ElementMetadata` which contains it's unique identity (`ElementReference`), any labels the be applied tro it on the labeled property graph and an effective from time.

```rust
query.process_source_change(SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1"),
            labels: Arc::new([Arc::from("Vehicle")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "plate": "AAA-1234",
            "color": "Blue"
        }))
    },
}).await;

query.process_source_change(SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "z1"),
            labels: Arc::new([Arc::from("Zone")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "type": "Parking Lot"
        })),
    },
}).await;
```

We can use the `process_source_change` function on the continuous query to compute the diff a data change has on the query result.


```rust
query.process_source_change(SourceChange::Insert {
    element: Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1-location"),
            labels: Arc::new([Arc::from("LOCATED_IN")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::new(),
        out_node: ElementReference::new("", "z1"),
        in_node: ElementReference::new("", "v1"),
    },
}).await;
```

```
Result: [Adding { 
    after: {"color": String("Blue"), "plate": String("AAA-1234")} 
}]
```

```rust

query.process_source_change(SourceChange::Update {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("", "v1"),
            labels: Arc::new([Arc::from("Vehicle")]),
            effective_from: 0,
        },
        properties: ElementPropertyMap::from(json!({
            "plate": "AAA-1234",
            "color": "Green"
        }))
    },
}).await;
```

```
Result: [Updating { 
    before: {"color": String("Blue"), "plate": String("AAA-1234")}, 
    after: {"color": String("Green"), "plate": String("AAA-1234")} 
}]
```

```rust
query.process_source_change(SourceChange::Delete {
    metadata: ElementMetadata {
        reference: ElementReference::new("", "v1-location"),
        labels: Arc::new([Arc::from("LOCATED_AT")]),
        effective_from: 0,
    },
}).await;
```

```
Result: [Removing { 
    before: {"color": String("Green"), "plate": String("AAA-1234")} 
}]
```

### Additional examples

More examples can be found under the [examples](examples) folder.

## Storage implementations

Drasi maintains internal indexes that are used to compute the effect of a data change on the query result. By default these indexes are in-memory, but a continuous query can be configured to use persistent storage.  Currently there are storage implementions for Redis, Garent and RocksDB.


## Release Status

The drasi-core library is one component of an early release of Drasi which enables the community to learn about and experiment with the platform. Please let us know what you think and open Issues when you find bugs or want to request a new feature. Drasi is not yet ready for production workloads.


## Contributing

Enable the local git hooks and your development machine

```
chmod +x .githooks/pre-commit
git config core.hooksPath .githooks
```
