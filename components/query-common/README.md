# Shared query implementation

Query-specific behavior shared by the library's linked transformer and the
native query transformer belongs here. It is extracted from the existing query
implementation, not a second query engine.

The shared implementation provides the existing query definition and semantic
fingerprint, parser/function construction, execution-setting validation and Core
builder configuration. Its result implementation owns canonical rows, retained
history, snapshot/replay/follow readers and publication fencing.
Live and scheduled output construction, provenance/profiling, output identity,
recovery validation, pending-output reconstruction, delivery confirmation and
query input-scope resolution also use the existing shared implementation.
Non-atomic publication checkpoint encoding, reset markers and generation
calculation are shared too. Hosting adapters retain their actual transaction
boundaries; independent writers are not promoted to atomic participants.
The same typed recovery-error classification and reset-ownership checks decide
which identified states may be rebuilt. AutoReset never treats an I/O error or a
foreign producer as permission to clear state.

`QueryOutputOwner` grants mutation access to the executing component.
`QueryResults` is a read-only view over the same state; it cannot mutate rows,
publish events or grant transaction authority. There is no host-side mirror of
the native plugin's projection.
The read-only recovery contract exposes metadata-only heads and coherent retained
views. Canonical output observations also provide row/outbox gauges without
fetching or projecting all rows.

Hosting adapters provide lifecycle and storage access. This crate owns no graph,
opens no storage and cannot commit transactions; publication remains the
processing owner's responsibility. Native transport preserves the actual
producer epoch and normalizes transport re-emissions to the query's logical
sequence before entering the shared retained-history implementation.

Ordinary channel/broadcast delivery remains in the graph-owned result catalogue
and outlet. Their native read adapter accesses these same results, including a
coherent snapshot/history view for reaction recovery; it does not create another
row store or convert short follow leases into indefinite subscriptions.

Ordinary query construction receives an explicitly registered implementation.
The Host SDK binds the resolved provider, progress owner, bootstrap provider,
catalogue and scheduling resource to the native plugin; this crate does not
choose or load it. Native snapshot/history transfers page one immutable shared
observation. Paging changes transport only, not result ownership or projection.

Scheduling reuses Core's future queue and evaluation and the existing query
wakeup policy. A native scheduling observation contains only the committed next
deadline, never another queue. Providers that expose only committed queue reads
are sampled after commit, and ordinary inputs that do not mutate the queue do
not acquire an extra scheduling read.
