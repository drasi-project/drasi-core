// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{
    config::{
        DEFAULT_COMMAND_TIMEOUT_MS, DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_MAX_STREAMS,
        DEFAULT_PROGRESS_TABLE, DEFAULT_RETRY_BUDGET_MS,
    },
    Encryption, MsSqlStoredProcConfig, MsSqlStoredProcFactory, ProgressTable,
};
use drasi_computation_plugin_sdk::{Factory, NativeComponent};

#[test]
fn defaults_are_published_and_applied() -> Result<()> {
    let config = MsSqlStoredProcConfig::parse(config())?;
    assert_eq!(config.port, 1433);
    assert_eq!(config.encryption, Encryption::Required);
    assert!(!config.trust_server_certificate);
    assert_eq!(config.progress_table, DEFAULT_PROGRESS_TABLE);
    assert_eq!(config.max_streams, DEFAULT_MAX_STREAMS);
    assert_eq!(config.command_timeout_ms, DEFAULT_COMMAND_TIMEOUT_MS);
    assert_eq!(config.connect_timeout_ms, DEFAULT_CONNECT_TIMEOUT_MS);
    assert_eq!(config.retry_budget_ms, DEFAULT_RETRY_BUDGET_MS);

    let details = MsSqlStoredProcFactory
        .configuration_details()
        .expect("published details");
    let schema = MsSqlStoredProcFactory.metadata().configuration;
    details.validate_schema(&schema)?;
    for (field, expected) in [
        ("port", json!(1433)),
        ("encryption", json!("required")),
        ("trustServerCertificate", json!(false)),
        ("progressTable", json!(DEFAULT_PROGRESS_TABLE)),
        ("maxStreams", json!(DEFAULT_MAX_STREAMS)),
        ("commandTimeoutMs", json!(DEFAULT_COMMAND_TIMEOUT_MS)),
        ("connectTimeoutMs", json!(DEFAULT_CONNECT_TIMEOUT_MS)),
        ("retryBudgetMs", json!(DEFAULT_RETRY_BUDGET_MS)),
    ] {
        assert_eq!(details.fields[field].default, Some(expected), "{field}");
    }
    // Only the password is secret; templates and routes are inspectable.
    let secret: Vec<_> = schema
        .fields
        .iter()
        .filter(|(_, field)| field.secret)
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(secret, ["password"]);
    assert!(schema.fields["host"].required && schema.fields["database"].required);
    Ok(())
}

#[test]
fn configuration_round_trips_through_construction() -> Result<()> {
    let mut full = config();
    full["port"] = json!(1444);
    full["encryption"] = json!("loginOnly");
    full["trustedCaPem"] = json!("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n");
    full["hostNameInCertificate"] = json!("sql.example");
    full["progressTable"] = json!("drasi.progress");
    full["maxStreams"] = json!(8);
    full["routes"] = json!({"q": {"deleted": {"template": "EXEC dbo.d {{param before.id}}"}}});
    full["commandTimeoutMs"] = json!(1000);
    full["connectTimeoutMs"] = json!(2000);
    full["retryBudgetMs"] = json!(0);
    let details = MsSqlStoredProcFactory
        .configuration_details()
        .expect("details");
    let schema = MsSqlStoredProcFactory.metadata().configuration;
    for configuration in [config(), full] {
        schema.validate(&configuration)?;
        let exposed = sink(configuration.clone())?.configuration()?;
        assert!(
            details.equivalent_with_defaults(&schema, &configuration, &exposed)?,
            "{configuration} vs {exposed}"
        );
        assert_eq!(
            exposed["password"], "p",
            "configuration() is the persistence hook"
        );
        assert_eq!(sink(exposed.clone())?.configuration()?, exposed);
    }
    Ok(())
}

#[test]
fn debug_output_redacts_the_password() -> Result<()> {
    let mut configuration = config();
    configuration["password"] = json!("hunter2-secret");
    let config = MsSqlStoredProcConfig::parse(configuration)?;
    let debug = format!("{config:?}");
    assert!(!debug.contains("hunter2-secret"), "{debug}");
    assert!(debug.contains("[REDACTED]"));
    let authentication = crate::Authentication::Sql {
        user: "u".into(),
        password: "hunter2-secret".into(),
    };
    assert!(!format!("{authentication:?}").contains("hunter2"));
    let token = crate::Authentication::AccessToken {
        token: "eyJ-token".into(),
    };
    assert!(!format!("{token:?}").contains("eyJ"));
    Ok(())
}

#[test]
fn invalid_and_legacy_configuration_is_rejected() {
    let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    for (field, value) in [
        ("connection", json!({"host": "h"})),
        ("settings", json!({})),
        ("command_timeout_ms", json!(1000)),
        ("default_template", json!({})),
        ("retry_attempts", json!(3)),
        ("identity_provider", json!("fake")),
        ("unknown", json!(true)),
        ("host", json!("")),
        ("port", json!(0)),
        ("database", json!("")),
        ("encryption", json!("on")),
        ("progressTable", json!("dbo.drasi;DROP TABLE x")),
        ("progressTable", json!("[dbo].[t]")),
        ("progressTable", json!("a.b.c")),
        ("progressTable", json!("1table")),
        ("maxStreams", json!(0)),
        ("maxStreams", json!(1_000_001)),
        ("commandTimeoutMs", json!(0)),
        ("commandTimeoutMs", json!(3_600_001)),
        ("connectTimeoutMs", json!(0)),
        ("retryBudgetMs", json!(3_600_001)),
        ("trustedCaPem", json!("not a certificate")),
        (
            "outputs",
            json!({"bad id": {"component": OUTPUT_ID, "port": "out"}}),
        ),
        (
            "defaultTemplate",
            json!({"inserted": {"template": "EXEC p"}}),
        ),
        (
            "defaultTemplate",
            json!({"added": {"template": "EXEC p", "extra": 1}}),
        ),
        ("routes", json!({"q": {"added": "EXEC p"}})),
    ] {
        let mut configuration = config();
        configuration[field] = value.clone();
        assert!(
            MsSqlStoredProcConfig::parse(configuration).is_err(),
            "{field}={value}"
        );
    }
    let mut both = config();
    both["trustServerCertificate"] = json!(true);
    both["trustedCaPem"] = json!(pem);
    assert!(
        MsSqlStoredProcConfig::parse(both).is_err(),
        "exclusive trust"
    );
    let mut plaintext_trust = config();
    plaintext_trust["encryption"] = json!("disabled");
    plaintext_trust["trustServerCertificate"] = json!(true);
    assert!(MsSqlStoredProcConfig::parse(plaintext_trust).is_err());
}

#[test]
fn at_least_one_template_is_required() {
    let mut none = config();
    none.as_object_mut()
        .expect("object")
        .remove("defaultTemplate");
    assert!(MsSqlStoredProcConfig::parse(none.clone()).is_err());
    none["defaultTemplate"] = json!({"added": {"template": "  "}});
    assert!(MsSqlStoredProcConfig::parse(none.clone()).is_err());
    none["routes"] = json!({"q": {"updated": {"template": "EXEC dbo.u {{param after.id}}"}}});
    assert!(MsSqlStoredProcConfig::parse(none).is_ok());
}

#[test]
fn literal_credentials_are_required_without_an_identity_binding() -> Result<()> {
    for (user, password) in [("", "p"), ("u", ""), ("", "")] {
        let mut configuration = config();
        configuration["user"] = json!(user);
        configuration["password"] = json!(password);
        let parsed = MsSqlStoredProcConfig::parse(configuration.clone())?;
        assert!(
            parsed.validate_credentials(false).is_err(),
            "{user}/{password}"
        );
        parsed.validate_credentials(true)?;
        assert!(sink(configuration).is_err());
    }
    let mut missing = config();
    missing.as_object_mut().expect("object").remove("password");
    assert!(sink(missing).is_err());
    Ok(())
}

#[test]
fn routes_must_name_a_bound_output() {
    let mut configuration = config();
    configuration["routes"] = json!({"other": {"added": {"template": "EXEC dbo.p"}}});
    assert!(sink(configuration).is_err());
    let mut short = config();
    short["outputs"] = json!({"source.q": {"component": OUTPUT_ID, "port": "out"}});
    short["routes"] = json!({"q": {"added": {"template": "EXEC dbo.p"}}});
    assert!(sink(short).is_ok(), "the last dotted segment matches");
    assert!(crate::MsSqlStoredProcSink::new(
        ComponentId::try_new(SINK_ID).expect("id"),
        GRAPH,
        MsSqlStoredProcConfig::parse(config()).expect("config"),
        &[],
        false,
    )
    .is_err());
}

#[test]
fn progress_tables_are_validated_and_bracket_quoted() -> Result<()> {
    assert_eq!(
        ProgressTable::parse("dbo.drasi_delivery_progress")?.quoted(),
        "[dbo].[drasi_delivery_progress]"
    );
    assert_eq!(ProgressTable::parse("_t1")?.quoted(), "[_t1]");
    for invalid in ["", ".t", "s.", "s t", "s.t;", "s.t]", "ü", &"a".repeat(129)] {
        assert!(ProgressTable::parse(invalid).is_err(), "{invalid}");
    }
    let ddl = crate::create_table_sql("[dbo].[drasi_delivery_progress]");
    for column in [
        "owner NVARCHAR(64)",
        "stream_key NVARCHAR(128)",
        "CHECK (version = 2)",
        "sequence NVARCHAR(20)",
        "digest VARBINARY(32)",
        "operations BIGINT",
        "completed BIGINT",
        "PRIMARY KEY (owner, stream_key)",
    ] {
        assert!(ddl.contains(column), "{column}");
    }
    Ok(())
}

#[test]
fn encryption_maps_to_tiberius_levels() {
    assert_eq!(
        Encryption::Required.level(),
        tiberius::EncryptionLevel::Required
    );
    assert_eq!(
        Encryption::LoginOnly.level(),
        tiberius::EncryptionLevel::Off
    );
    assert_eq!(
        Encryption::Disabled.level(),
        tiberius::EncryptionLevel::NotSupported
    );
}
