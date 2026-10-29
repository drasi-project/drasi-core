# Shared reaction behavior

This library contains the existing reaction template/configuration types and
protocol rendering reused by ordinary and native plugins. It owns no graph,
query evaluator, plugin registry, processing worker or output queue.

`TemplateSpec`, `QueryConfig`, `TemplateRouting` and `AdaptiveBatchConfig` were
extracted from `drasi-lib::reactions::common`; that module re-exports the same
types while its remaining consumers are being replaced. The optional `sse`
feature shares the SSE reaction's template selection, JSON helper, path routing
and browser payload rendering with the native sink.

Run `cargo test -p drasi-reaction-common --features sse` from the workspace.
