# Native bootstrap provider standard

This guide states what a native bootstrap provider (a `BootstrapFactory`
and the providers it creates) must do beyond compiling against the
computation SDK (ABI 0.1). It applies to every crate in this directory and to
the coordinators of the sources they pair with. The SQLite crates are the
reference implementation of a paired family; the HTTP crate is the reference
for an unpaired provider.

A bootstrap factory is a declared graph component like a source or sink. The
Host registers each factory as a `BootstrapProvider` node whose single
bootstrap output (`snapshot`) is connected to a source's bootstrap input, or
directly to a transformer's or query's bootstrap input. Compatibility between
the provider and what it is connected to is declared and checked when that
edge binds, never inferred from configuration keys, plugin identities or a
first session.

## Declaration

Every factory implements the required pure
`BootstrapFactory::describe(&configuration)`. It returns the provider's
`BootstrapOutputDescription`:

| Fact | Values | Meaning |
|---|---|---|
| `completion` | `quiescent`, `durable` | Quiescent: records only. Preparation is always ready, there are no watermarks, no completion state and no progress binding. Durable: anything else (a source watermark, completion state, a query progress binding, or a non-ready preparation such as `RefreshVolatile`). |
| `pairing` | a `BootstrapProtocol` or none | The coordination protocol the provider requires of the source it serves. A paired provider is created by that source library's coordinator. |
| `namespace` | `servedSource`, `own` | `servedSource`: records use the namespace (element IDs) and output stream of the source whose bootstrap input the provider is connected to. `own`: records carry their own identities. |
| `identity` | an `ElementIdentity` or none | The element identities the provider produces, when they must equal the served source's (for example a SQLite file snapshot's tables and keys). |

The Host forwards the description unchanged as the node's
`ComponentDescription`. `BootstrapFactoryMetadata::pairing` repeats the
factory-level pairing (it selects how providers are created), and the SDK
refuses a description whose pairing differs, a paired or progress-bound
provider that is not durable, or a paired provider that does not serve its
source's namespace.

Sources declare the other side in their own `Factory::describe`:

- `ComponentDescription::coordination` (`BootstrapCoordination { protocol,
  required }`): the protocol the source's coordinator implements, and whether
  only a provider paired through it may be connected (the source admits no
  live data before its coordinated snapshot);
- `ComponentDescription::element_identity`: how the source identifies its
  elements.

The Host owns every bootstrap port and declares its facts: a source's
bootstrap input relays sessions to the provider there (`sourceRelay`), and its
bootstrap output completes as that provider does (`relayed`); a transformer's
bootstrap input accepts quiescent or durable initialization per its
`quiescent_bootstrap`/`durable_bootstrap` capability; a query's bootstrap
input accepts durable initialization. Plugins never declare bootstrap port
facts.

## Binding checks

When a bootstrap edge binds, the graph checks (`check_bootstrap_edge`):

1. A paired provider requires a source relay whose coordination protocol is
   the provider's; a source that requires coordination refuses an unpaired
   provider.
2. A provider serving its source's namespace requires a served source: the
   source relay it is connected to, or the single source a query-local
   consumer reads (see Namespace).
3. A provider declaring an element identity requires its served source to
   declare the same identity.
4. A quiescent input refuses any provider that is not quiescent. For an input
   fed by a source relay, the relay's completion is the completion of the
   providers on the source's own bootstrap input (quiescent with none); the
   edge from the provider to the source repeats the check against every
   quiescent consumer the source already serves, so whichever edge binds last
   finds the mismatch.

A mismatch never rejects the graph change: both nodes stay, the edge is
failed and unbound, and the consumer carries a terminal binding failure
(`GraphError::Incompatible` with a `Bootstrap*` incompatibility) until either
side changes. Undeclared facts satisfy no requirement.

## Pairing

A paired family has three parts:

- the protocol constants in the family's `*-common` crate (for example
  `drasi_postgres_common::replication::coordination`);
- the source factory: `HostServices::bootstrap_coordination` declared with one
  progress mode, `Factory::bootstrap_coordination` returning its
  `SourceBootstrapCoordination::create_bootstrap_coordinator` and a `describe` that declares the offer
  (only for configurations that can coordinate, such as `coordinatedSnapshot:
  true` or `startFrom: snapshot`);
- the bootstrap library's factory: usually the SDK's
  `PairedBootstrapFactory::new(implementation, protocol)`, which takes no
  configuration and opens the provider from the paired source.

The source's coordinator (`NativeSourceBootstrapCoordinator`) constructs the
source and opens each query's provider from the same creation request, so the
provider uses the source's identity, stream, connection settings, table
selection, keys, slot or barrier names and retention. Nothing the source owns
is repeated in the provider's configuration. The coordinator connects with the
source's current activation credentials (sessions are relayed only while the
source is active). `NativeSourceBootstrapCoordinator::protocol` must equal the
protocol the source declares; the Host refuses the source otherwise. The
paired provider receives a typed `SourceBootstrapMetadata` (source,
protocol, query, labels), never the source's configuration or
implementation name.

The Host creates a coordinator only for a source that declares coordination
and has a provider connected to its bootstrap input.

## Watermarks and completion

A durable provider implements every hook of `DurableBootstrapProvider`:

| Hook | Contract |
|---|---|
| `prepare(state)` | Validate the query's progress and persisted handover state; return `Ready`, `RefreshVolatile` or `ResetRequired`. `state` is the query-owned bootstrap state when the query has one. |
| `has_pending_snapshot` | Whether a prepared snapshot is needed even after a completed bootstrap. |
| `snapshot(state)` | Persist handover intent before external initialization, then return the record stream and any watermarks known up front. |
| `complete_snapshot` | Boundaries known only after the stream closed (the source watermark the source resumes from). |
| `completion_state` | Final handover state, committed atomically with the watermarks and the completed marker. |
| `recovery_reader` | The query progress reader the provider was given, unchanged. |
| `stop` | Finish owned cleanup (see Lifecycle). |

A quiescent provider implements `QuiescentBootstrapProvider` (scope check,
`snapshot`, `stop`). The SDK reports it as always ready, never pending,
refuses watermarks it returns and completes it without state, so a quiescent
provider cannot hand over by accident. The factory returns the variant
matching its declared completion; the SDK refuses a mismatch.

## Labels

Each query declares the node and relation labels it reads (its query text and
its inputs' labels, without synthetic join relations). The query sends them in
every `BootstrapRequest` (`labels`), a source relay forwards them unchanged,
and the Host passes them to the provider in `BootstrapSession::labels` (paired
providers receive them through the coordinator's `open`). A query whose
middleware or input pipelines can rename labels, or that matches a node or
relation without a label, sends none, which means unrestricted. Transformers
send none: their own snapshot is not bounded by a downstream query's labels.

Providers select at selection time, not by filtering records: a table,
endpoint or collection is read only if a requested label can come from it. A
configured selection (`tables`, `labels`, `nodeLabels`) is an upper bound: a
requested label outside it yields no data. An empty request is a real
restriction (nothing of that kind), while no request selects everything
configured.

## Namespace

A provider serving a source's namespace never configures the source: the
graph relation supplies it (`served_source` in drasi-lib). Connected to a
source's bootstrap input, the provider serves that source and its single
output stream; connected directly to a query's or transformer's bootstrap
input, it serves the single source that consumer reads (the producer of its
data inputs). The Host passes the result in `BootstrapSession::served`;
`BootstrapSession::served()` fails if it is missing. A consumer that reads no
single source gives the provider no namespace, and the edge is refused when
it binds (`BootstrapNamespace`); a declared element identity is compared with
the served source's in either case.

## Configuration

- One flat object; keys are lowerCamelCase at every level and every nested
  type has `deny_unknown_fields`. `describe` parses the whole typed
  configuration, so a misspelled nested key is refused when the graph asks
  for the description, before any I/O.
- A provider that accepts its source's settings publishes the source's own
  options from the shared `*-common` `definition` module (SQL Server, MySQL,
  PostgreSQL), plus its own fields with their serde defaults. Never maintain
  a copy of the source's option list or a field-name allow-list.
- Secrets are top-level fields (`password`), never a secret `settings`
  object.

## Lifecycle

- Construction is configuration-only: no I/O, no clock, no secret resolution
  beyond validation.
- `stop` is required. It finishes owned asynchronous cleanup (workers, cursors,
  connections), retains unfinished work on error and returns success only when
  nothing the provider owns is still running. A provider whose stream owns all
  of its resources documents why `stop` has nothing to do.
- A provider owns no graph state to erase. State that outlives a session (a
  replication slot, a barrier marker) belongs to the paired source, which
  erases it on its own erasing disposal; this is why such state must be named
  from the source's identity, never from provider configuration.
- Each session gets its own provider for the requesting query. Cancelling a
  stream requests cancellation; the Host awaits `stop` before reuse, query
  stop or deprovisioning.

## Tests

Each provider crate covers:

- `describe` and `metadata` agree (`BootstrapFactoryMetadata::validate_description`),
  and configuration keys the graph relation or the paired source own are
  refused;
- published keys are camelCase (`testing::ensure_camel_case_configuration`),
  published defaults equal the parsed defaults, and an unknown nested key is
  refused;
- the provider variant matches the declared completion;
- the served source is required from the session for `servedSource`
  providers;
- label selection within the configured upper bound;
- for paired families, the source's coordination offer for each mode and the
  coordinator's provider (container suites).

Graph-level binding checks are covered in `lib/tests/computation_bootstrap_binding.rs`
and the contract tests in `computation-contracts` (`description.rs`).
