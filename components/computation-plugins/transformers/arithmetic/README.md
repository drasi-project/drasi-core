# Native arithmetic transformer

`drasi-computation-transformer-arithmetic` provides only
`drasi.standard/arithmetic` (implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-transformer-arithmetic --features dynamic-plugin
```

Both `in` and `out` use `GraphChangeCodec`. Configuration requires `stream`;
defaults are `field="value"`, `add=0`, `multiply=1`,
`counter_property="batch_count"`. Arithmetic is `(value + add) * multiply`, with
checked integer overflow and explicit type errors.

Standalone execution is volatile. Opted-in transaction participation borrows the
host's existing transaction capability: the host owns state/output commit,
rollback and replay. See the
[transaction contract](../../STANDARD.md#transactional-arithmetic).
No counter, middleware or capture component is loaded.
