# Native computation contracts

Shared identifiers, envelopes, schemas, message codecs and component interfaces
belong here rather than in the query runtime. Both the host and native plugins
use the same definitions and encoding implementations.

There is no graph executor, query evaluator, language parser or storage backend
in this package. Query-result and graph-change data retain their existing wire
formats. The query AST dependency represents values; it does not execute them.
Owned-worker helpers use the caller's Tokio runtime and do not create an executor.
`QueryResultSnapshot` is a snapshot of result rows, not query configuration.
Its former `QuerySnapshot` name remains a compatibility alias.

The runtime remains responsible for graph membership, actual storage ownership,
query processing and recovery. Native plugins use explicit host capabilities,
not concrete runtime objects.
