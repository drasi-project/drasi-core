
# Examples

## Temperature Fluctuations

This example simulates a set of components that track temperature that can be connected to a temperature limit definition.  The example first creates 2 components connected to the same limit of 20.  Then every 3 seconds the temperatures are randomly updated, if one of the components temperature moves from below 20 to above 20, an `adding` result will be emitted by the query.  If one falls from above 20 to below 20, then a `removing` result will be emited by the query.  If the temperature of a component was already above 20 and changed to another value above 20, then an `updating` result will be emitted.

Note: Changing the value of the `Limit` node would also produce results on the query.

```
cargo run --example temperature
```