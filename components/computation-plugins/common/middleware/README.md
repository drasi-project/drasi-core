# drasi-computation-middleware-common

The middleware implementation shared by the native
[`drasi.standard/middleware`](../../transformers/middleware) and
[`drasi.standard/view`](../../transformers/view) transformers. It exports no
factories.

- `STATELESS_KINDS`: the kinds both transformers declare (`decoder`, `map`,
  `parse_json`, `promote`, `relabel`). The list is a constant rather than
  whatever `drasi-middleware` features a build unified, so a transformer's
  declaration never depends on how it was built. Each kind is a
  `drasi_middleware::stateless::StatelessMiddleware`: it never reads stored
  elements, so the transformers need no element index.
- `Mode::options()`: the `stream`, `middleware` and `pipeline` configuration
  options. The `middleware` option carries a `ConfigurationConstraint` that
  restricts `kind` to the declared list (and, in `Mode::Images`, restricts map
  declarations to insert rules emitting complete `Insert`/`Replace` images).
  The graph checks it when the component binds.
- `Pipeline::new(mode, middleware, pipeline)`: checks each declaration against
  the same constraint, builds the steps and resolves pipeline names.
  `Pipeline::apply` runs one change event (middleware transformer);
  `Pipeline::map` runs one complete image (view).

The query transformer hosts a full middleware registry (including `unwind`)
and publishes that registry's kinds with
`ConfigurationConstraint::middleware_declarations`, the same declaration
shape.
