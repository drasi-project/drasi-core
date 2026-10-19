# Shared gRPC protocols

`drasi-grpc-common` owns the message definitions, client/server bindings and JSON
value conversion shared by ordinary and native gRPC components. It contains no
source, sink, listener or plugin implementation.

Select the needed protocol features:

| Feature | Rust module | Protocol files |
|---|---|---|
| `source` | `source` | [Source events and service](proto/source/drasi/v1/source.proto), [shared source messages](proto/source/drasi/v1/common.proto) |
| `reaction` | `reaction` | [Reaction results and service](proto/reaction/drasi/v1/reaction.proto) |
| `output` | `output` (also enables `reaction`) | Extracted reaction configuration, templating, metadata-compatible batches, connection/retry classification and their regression suites |

The historical source and reaction contracts both use the protobuf package
`drasi.v1`, but their `QueryResult` messages are different. They are deliberately
generated into separate Rust modules and must not be combined into one protocol.
Service paths, message fields, field numbers and conversions are unchanged.

Existing imports through `drasi_source_grpc::proto`,
`drasi_reaction_grpc::proto::drasi_v1`, and the native gRPC source's `proto::source`
module remain re-exports of these shared definitions. Raw shared protocol files
are maintained only here; generating clients requires `protoc` and the relevant
source or reaction include directory shown above. Build scripts can use
`SOURCE_PROTO_ROOT` to import source messages without maintaining another copy.
The native source retains its own admission-service protocol and conversions.

The output helpers do not own a component queue or graph. Both reaction
implementations use the same rendering/protocol logic; native delivery retains
the host's handled-input and cancellation boundary.

Run `cargo test -p drasi-grpc-common --features source,output`
from the workspace root.
