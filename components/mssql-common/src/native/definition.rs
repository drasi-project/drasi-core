// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published options of [`NativeSettings`](super::NativeSettings): the
//! one description, default and bound of every flat setting, used by the
//! native source and by the bootstrap providers so their schemas cannot
//! drift from each other or from the settings type.

use super::settings::*;
use crate::client::Encryption;
use drasi_computation_plugin_sdk::{ConfigOption, ConfigType};
use serde_json::json;

fn string(name: &str, required: bool, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, required, description)
}

fn integer(
    name: &str,
    description: &str,
    default: u64,
    minimum: i64,
    maximum: u64,
) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Integer, false, description)
        .default(default)
        .minimum(minimum)
        .maximum(maximum)
}

/// Every settings option, in [`SETTINGS_FIELDS`] order.
pub fn options() -> Vec<ConfigOption> {
    vec![
        string("host", false, "SQL Server host name or address. Also the endpoint passed to an activation identity provider.").default(DEFAULT_HOST),
        integer("port", "SQL Server TCP port.", u64::from(DEFAULT_PORT), 1, u64::from(u16::MAX)),
        string("database", true, "Database whose CDC capture instances are read."),
        string("user", false, "SQL authentication login. Required unless an activation identity is bound, which supplies the login or a Microsoft Entra ID access token at each start.").default_description("None; required without an identity binding."),
        string("password", false, "SQL authentication password. A secret reference; required unless an activation identity is bound.").secret(),
        string("encryption", false, "required encrypts the session and verifies the server certificate. loginOnly encrypts only the login packet and disabled sends everything in plaintext; both log a warning.")
            .default(json!(Encryption::Required))
            .allowed([json!(Encryption::Required), json!(Encryption::LoginOnly), json!(Encryption::Disabled)]),
        ConfigOption::new("trustServerCertificate", ConfigType::Boolean, false, "Skip server certificate validation (testing only; logs a warning). Mutually exclusive with trustedCaPem.").default(false),
        string("trustedCaPem", false, "PEM CA certificates trusted in addition to the system roots when verifying the server certificate."),
        string("hostNameInCertificate", false, "Host name expected in the server certificate when it differs from host.").default_description("The value of host."),
        integer("connectTimeoutMs", "Deadline for TCP, TLS and login of one connection attempt.", DEFAULT_CONNECT_TIMEOUT_MS, 1, MAX_TIMEOUT_MS),
        integer("commandTimeoutMs", "Deadline for each server round trip (catalog, key and range reads).", DEFAULT_COMMAND_TIMEOUT_MS, 1, MAX_TIMEOUT_MS),
        string("serverTimeZone", false, "The server's time zone as a sys.time_zone_info name (for example UTC or Pacific Standard Time), used to convert CDC commit times to UTC. Required on servers older than SQL Server 2022.").default_description("Detected with CURRENT_TIMEZONE_ID() (SQL Server 2022 and Azure SQL)."),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to read, as table or schema.table; bracket-quote names with other characters ([Sales Ops].[Order Lines]). For the source, empty streams every CDC-enabled table except barrierTable, and a capture instance added later then stops the source with a schema-change error. The read-only snapshot requires an explicit list.").default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"schema.table\", \"keyColumns\": [..]}, for tables without a primary key. The capture instance must include the key columns.").default(json!([])),
        string("startPosition", false, "Where a source with no committed cursor starts: current (the CDC maximum LSN at first start) or beginning (each capture instance's oldest retained change).")
            .default(json!(StartPosition::Current))
            .allowed([json!(StartPosition::Current), json!(StartPosition::Beginning)]),
        string("barrierTable", false, "Administrator-provisioned CDC table with a uniqueidentifier token column, used by the coordinated bootstrap provider. Erasing removal deletes this source's leftover markers from it."),
        integer("pollIntervalMs", "Delay before polling again after a page with no changes.", DEFAULT_POLL_INTERVAL_MS, 1, MAX_POLL_INTERVAL_MS),
        integer("maxRows", "Page size: the most change groups read and changes emitted per poll.", DEFAULT_MAX_ROWS as u64, 1, MAX_MAX_ROWS as u64),
        integer("maxBytes", "Most record payload bytes per page; one change must fit.", DEFAULT_MAX_BYTES as u64, MIN_MAX_BYTES as i64, MAX_MAX_BYTES as u64),
        integer("reconnectBudgetMs", "Total time to keep reconnecting, with jittered exponential backoff, after a transport failure. 0 disables reconnection.", DEFAULT_RECONNECT_BUDGET_MS, 0, MAX_RECONNECT_BUDGET_MS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ConfigDefinition;

    #[test]
    fn options_describe_exactly_the_settings_and_their_defaults() {
        let definition = ConfigDefinition::try_new(options()).unwrap();
        let schema = definition.schema();
        assert!(schema.fields.keys().map(String::as_str).eq({
            let mut names = SETTINGS_FIELDS.to_vec();
            names.sort();
            names
        }));
        let parsed = NativeSettings::from_value(json!({"database": "app"})).unwrap();
        let exposed = serde_json::to_value(&parsed).unwrap();
        let details = definition.details();
        for (name, field) in &details.fields {
            if let Some(default) = &field.default {
                assert_eq!(Some(default), exposed.get(name), "{name}");
            }
        }
        assert!(details
            .equivalent_with_defaults(&schema, &json!({"database": "app"}), &exposed)
            .unwrap());
    }
}
