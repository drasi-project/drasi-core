pub mod auth;
pub mod catalog;
pub mod config;
pub mod mapping;
pub mod position;
pub mod projection;
pub mod pubsub;
pub mod rest;
pub mod snapshot;

pub mod proto {
    tonic::include_proto!("eventbus.v1");
}

pub use auth::{SalesforceAuthManager, SalesforceSessionToken};
pub use catalog::{
    graph_changes, Catalog, DiscoveredSchema, FieldSchema, GraphRecord, LookupRelationship,
    LookupSchema, ObjectSchema, SchemaOmission,
};
pub use config::{
    InitialCursorBehavior, ObjectProjectionConfig, SalesforceAuthConfig, SalesforceConnectionConfig,
};
pub use mapping::{
    apply_text_diff, delete_change, delete_change_with_org, node_change, node_change_with_org,
    Change as NativeChange, Decoder as NativeDecoder, Invalidation, Operation,
};
pub use position::{LocalPosition, POSITION_ENCODED_BYTES, POSITION_VERSION_V1};
pub use projection::{
    canonical_record_id, graph_element_id, merge_properties, parse_instant, properties_json,
    query_json, validate_identifier, validate_org_id, Describe, DescribeField, Field, Kind,
    ObjectPlan, Properties, Record,
};
pub use pubsub::{Feed as PubSubFeed, ReplayCursor, SalesforcePubSubClient, TopicUnavailable};
pub use rest::SalesforceRestClient;
pub use snapshot::{
    read_snapshot_record, resolve_snapshot_path, sha256_hex, write_snapshot_record,
    SnapshotManifest, SnapshotTicket, MAX_SNAPSHOT_FILE_BYTES,
    SALESFORCE_BOOTSTRAP_TICKET_CONTEXT_PROPERTY, SNAPSHOT_MANIFEST_VERSION,
};

#[cfg(test)]
mod protocol_tests;
