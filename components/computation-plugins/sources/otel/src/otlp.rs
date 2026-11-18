// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Generated OTLP protobuf types (opentelemetry-proto v1.5.0), plus the
//! `google.rpc.Status` message OTLP/HTTP returns with error responses.

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    dead_code,
    unused_imports,
    missing_docs
)]
pub mod proto {
    pub mod common {
        pub mod v1 {
            tonic::include_proto!("opentelemetry.proto.common.v1");
        }
    }
    pub mod resource {
        pub mod v1 {
            tonic::include_proto!("opentelemetry.proto.resource.v1");
        }
    }
    pub mod trace {
        pub mod v1 {
            tonic::include_proto!("opentelemetry.proto.trace.v1");
        }
    }
    pub mod metrics {
        pub mod v1 {
            tonic::include_proto!("opentelemetry.proto.metrics.v1");
        }
    }
    pub mod logs {
        pub mod v1 {
            tonic::include_proto!("opentelemetry.proto.logs.v1");
        }
    }
    pub mod collector {
        pub mod trace {
            pub mod v1 {
                tonic::include_proto!("opentelemetry.proto.collector.trace.v1");
            }
        }
        pub mod metrics {
            pub mod v1 {
                tonic::include_proto!("opentelemetry.proto.collector.metrics.v1");
            }
        }
        pub mod logs {
            pub mod v1 {
                tonic::include_proto!("opentelemetry.proto.collector.logs.v1");
            }
        }
    }
}

/// The `google.rpc.Status` message. OTLP/HTTP requires it, protobuf-encoded,
/// as the body of every 4xx and 5xx response. Details are never populated.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RpcStatus {
    /// A `google.rpc.Code` value.
    #[prost(int32, tag = "1")]
    pub code: i32,
    /// A client-safe description of the error.
    #[prost(string, tag = "2")]
    pub message: String,
}
