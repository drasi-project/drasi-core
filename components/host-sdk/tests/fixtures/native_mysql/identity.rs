// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone)]
struct RotatingIdentity(Arc<Mutex<IdentityState>>);

struct IdentityState {
    next: Option<Credentials>,
    contexts: Vec<HashMap<String, String>>,
}

impl RotatingIdentity {
    fn state(&self) -> Result<std::sync::MutexGuard<'_, IdentityState>> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("identity fixture poisoned"))
    }
}

#[async_trait]
impl IdentityProvider for RotatingIdentity {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials> {
        let mut state = self.state()?;
        state.contexts.push(context.properties.clone());
        state
            .next
            .clone()
            .context("expected identity-provider failure")
    }
    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

pub(super) async fn rotation(
    database: &Database,
    factory: &computation::NativeFactory,
    envelope: ChangeEnvelope,
) -> Result<()> {
    database
        .sql(
            "SET SESSION sql_log_bin=0; \
        CREATE TABLE credential_audit(auth_user VARCHAR(80),query_id VARCHAR(64)); \
        CREATE USER native_identity@'%' IDENTIFIED BY 'first-auth';\n\
        DELIMITER //\n\
        CREATE PROCEDURE credential_effect(IN q VARCHAR(64)) BEGIN \
          INSERT INTO credential_audit VALUES(SUBSTRING_INDEX(USER(),'@',1),q); END //\n\
        DELIMITER ;\n\
        GRANT EXECUTE ON test.* TO native_identity@'%';",
        )
        .await?;
    let provider = RotatingIdentity(Arc::new(Mutex::new(IdentityState {
        next: Some(Credentials::UsernamePassword {
            username: "native_identity".into(),
            password: "first-auth".into(),
        }),
        contexts: Vec::new(),
    })));
    let identity = Arc::new(computation::NativeIdentityResource::new(Arc::new(
        provider.clone(),
    )));
    let mut settings = database.sink_settings();
    for event in ["added", "updated", "deleted"] {
        settings["default_template"][event]["template"] =
            json!("CALL credential_effect({{param query_id}})");
    }
    let literal_settings = settings.clone();
    let object = settings.as_object_mut().context("MySQL settings object")?;
    object.remove("user");
    object.remove("password");
    let mut sink = factory.create_component_with_identity(
        id("identity-procedures"),
        json!({"settings":settings}),
        identity.clone(),
    )?;
    assert!(provider.state()?.contexts.is_empty());
    let declared = sink.configuration()?;
    assert_eq!(declared["settings"]["user"], "");
    assert_eq!(declared["settings"]["password"], "");
    let input = || InputEnvelope {
        port: PortId::try_new("in").expect("input"),
        envelope: envelope.clone(),
    };
    let proof =
        async {
            sink.start().await?;
            assert_eq!(provider.state()?.contexts.len(), 1);
            sink.handle(input()).await?;
            sink.stop().await?;
            assert_eq!(sink.configuration()?, declared);
            database
                .sql(
                    "SET SESSION sql_log_bin=0; \
            ALTER USER native_identity@'%' IDENTIFIED BY 'second-auth'",
                )
                .await?;
            provider.state()?.next = Some(Credentials::Token {
                username: "native_identity".into(),
                token: "second-auth".into(),
            });
            sink.start().await?;
            assert_eq!(provider.state()?.contexts.len(), 2);
            sink.handle(input()).await?;
            sink.stop().await?;
            assert_eq!(sink.configuration()?, declared);

            for rejected in [
                None,
                Some(Credentials::Certificate {
                    cert_pem: "not-a-password".into(),
                    key_pem: "not-a-password".into(),
                    username: Some("native_identity".into()),
                }),
                Some(Credentials::Token {
                    username: "native_identity".into(),
                    token: "x".repeat(64 * 1024 + 1),
                }),
                Some(Credentials::Token {
                    username: "native_identity".into(),
                    token: "expired-auth".into(),
                }),
            ] {
                provider.state()?.next = rejected;
                assert!(
                    sink.start().await.is_err(),
                    "failed identity must not reuse old auth"
                );
                assert!(sink.handle(input()).await.is_err());
                sink.stop().await?;
                assert_eq!(sink.configuration()?, declared);

                let reads = provider.state()?.contexts.len();
                let mut fallback = factory.create_component_with_identity(
                    id("literal-fallback-probe"),
                    json!({"settings":literal_settings}),
                    identity.clone(),
                )?;
                assert_eq!(provider.state()?.contexts.len(), reads);
                let original = fallback.configuration()?;
                let start = fallback.start().await;
                let stopped = fallback.stop().await;
                assert!(
                    start.is_err(),
                    "bound identity must override valid root literals"
                );
                stopped?;
                assert_eq!(fallback.configuration()?, original);
            }

            provider.state()?.next = Some(Credentials::UsernamePassword {
                username: "native_identity".into(),
                password: "second-auth".into(),
            });
            sink.start().await?;
            sink.handle(input()).await?;
            sink.stop().await?;
            assert_eq!(sink.configuration()?, declared);
            assert_eq!(provider.state()?.contexts.len(), 11);
            let expected = HashMap::from([
                ("hostname".to_owned(), "127.0.0.1".to_owned()),
                ("port".to_owned(), database.port.to_string()),
            ]);
            for context in &provider.state()?.contexts {
                assert_eq!(context, &expected);
            }
            assert_eq!(
                database
                    .sql("SELECT COUNT(*) FROM credential_audit")
                    .await?,
                "3"
            );
            assert_eq!(
                database
                    .sql("SELECT DISTINCT auth_user,query_id FROM credential_audit")
                    .await?,
                "native_identity\tleft"
            );
            database.expect_count(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='native_identity'",
            "0",
        ).await?;
            Ok(())
        }
        .await;
    let stopped = sink.stop().await;
    combine("native MySQL activation credentials", [proof, stopped])
}
