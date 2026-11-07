# Native computation contracts

Shared identifiers, envelopes, schemas, message codecs and component interfaces
belong here rather than in the query runtime. Both the host and native plugins
use the same definitions and encoding implementations.

There is no graph executor, query evaluator, language parser or storage backend
in this package. Sources, transformers and query outputs share the graph-change
contract; query-specific metadata is optional for generic graph consumers.
Shared graph values come from `drasi-core-types`.
`QueryJoinConfig` and `QueryJoinKeyConfig`, their validation, and semantic-order
normalization are shared by ordinary and native query factories. They convert to
the existing Core join models; join matching and indexing do not live here.
Owned-worker helpers use the caller's Tokio runtime and do not create an executor.
`OperationFailures` preserves all typed operation/cleanup causes without requiring
the runtime; `drasi_lib::error` re-exports that same type. `SourceProgressView`
exposes the actual provider's read-only publication and subscriptions, never local
transaction ownership or a copied progress registry.
`QueryResultSnapshot` is a snapshot of result rows, not query configuration.
The former `QuerySnapshot` alias has been removed.

The runtime remains responsible for graph membership, actual storage ownership,
query processing and recovery. Native plugins use explicit host capabilities,
not concrete runtime objects.

`ComponentId` is a validated UUID, serialized as canonical UUID text.
`ComponentName` is separate, case-sensitive graph-owned metadata; renaming does
not change the UUID or existing connections. Authoring `ComponentReference`
strings select names, even when the string looks like a UUID; `{"id":"UUID"}`
selects an existing identity explicitly. Plugin IDs, implementation versions,
graph/instance IDs and business-record namespaces remain their own identifiers.

JSON `EnvelopeCodec` and MessagePack `BinaryEnvelopeCodec` use format **2**.
Both reject previous formats, including envelopes without component annotations.
Use fresh persisted state and matching native libraries; there is no name-to-ID
conversion or migration reader.

Published envelopes have a maximum ancestry depth of 1,024
(`MAX_LINEAGE_DEPTH`). JSON and binary decoding, encoding and binary size
measurement enforce the same bound. `ChangeEnvelope::try_derive` checks it before
derivation; `derive` may construct a deeper in-memory value, which publication
rejects. Lineage destruction is iterative and preserves other shared branches.
