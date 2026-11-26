# Native log sink

`drasi-computation-sink-log` provides `drasi.standard/log` (implementation `1`,
configuration version `1`, native ABI 0.2). It writes each standard graph
change as one line, rendered by a per-output Handlebars template or formatted
as JSON or the legacy compact form, to the host's logger (also when loaded as a
plugin library, through the SDK's logging bridge), stdout or stderr. Each input
completes (`Handled`) after its lines are emitted.

```sh
cargo build -p drasi-computation-sink-log --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: rendering and validation, routing, render-failure
  policy, delivery profile, lifecycle, management and legacy differences.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults and update
  boundaries, the template reference (context, helpers, routing), examples and
  the legacy mapping.

Minimal configuration: `{}` logs every change as JSON at `info` on target
`drasi.log`.
