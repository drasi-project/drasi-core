// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_host_sdk::computation::NativeIdentityResource;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

#[derive(Clone)]
struct Provider {
    calls: Arc<AtomicUsize>,
    certificate: Arc<AtomicUsize>,
    contexts: Arc<Mutex<Vec<(String, String)>>>,
}
#[async_trait]
impl IdentityProvider for Provider {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts
            .lock()
            .map_err(|_| anyhow::anyhow!("context lock"))?
            .push((
                context
                    .properties
                    .get("hostname")
                    .context("hostname")?
                    .clone(),
                context.properties.get("port").context("port")?.clone(),
            ));
        if self.certificate.load(Ordering::SeqCst) > 0 {
            Ok(Credentials::Certificate {
                cert_pem: "must-not-leak-certificate".into(),
                key_pem: "must-not-leak-key".into(),
                username: None,
            })
        } else {
            anyhow::bail!("must-not-leak-provider-secret")
        }
    }
    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}
async fn public_binding(dynamic: bool) -> Result<()> {
    let native = plugins(dynamic)?;
    let factory = &native[2].factories()[0];
    ensure!(
        factory.supports_activation_credentials(),
        "actual MSSQL sink did not export activation credentials"
    );
    let config = json!({"settings":{
        "connection":{"host":"credential-only.invalid","port":1437,"database":"d","user":"","password":""},
        "outputs":{"q":{"component":"q","port":"out"}},"command_timeout_ms":100,
    }});
    ensure!(
        crate::sink_inputs::create(factory, id("sink"), config.clone()).is_err(),
        "unbound empty credentials were accepted"
    );
    let provider = Provider {
        calls: Arc::default(),
        certificate: Arc::default(),
        contexts: Arc::default(),
    };
    let mut sink = crate::sink_inputs::create_with_identity(
        factory,
        id("sink"),
        config,
        Arc::new(NativeIdentityResource::new(Arc::new(provider.clone()))),
    )?;
    let declared = sink.configuration()?;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    let failure = sink
        .start()
        .await
        .expect_err("provider failure must propagate before SQL");
    ensure!(
        !format!("{failure:#}").contains("must-not-leak"),
        "provider secret leaked"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.configuration()?, declared);
    sink.stop().await?;
    provider.certificate.store(1, Ordering::SeqCst);
    let failure = sink
        .start()
        .await
        .expect_err("certificate must be rejected at Host boundary");
    ensure!(
        failure.to_string().contains("certificate"),
        "wrong certificate rejection"
    );
    ensure!(
        !format!("{failure:#}").contains("must-not-leak"),
        "certificate material leaked"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(provider
        .contexts
        .lock()
        .map_err(|_| anyhow::anyhow!("context lock"))?
        .iter()
        .all(|context| context == &("credential-only.invalid".into(), "1437".into())));
    assert_eq!(sink.configuration()?, declared);
    sink.stop().await?;
    Ok(())
}
#[tokio::test]
async fn static_activation_binding_and_host_rejection_without_database() -> Result<()> {
    public_binding(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_activation_binding_and_host_rejection_without_database() -> Result<()> {
    public_binding(true).await
}

#[derive(Clone)]
struct ActualProvider {
    username: String,
    password: String,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl IdentityProvider for ActualProvider {
    async fn get_credentials(&self, _: &CredentialContext) -> Result<Credentials> {
        let activation = self.calls.fetch_add(1, Ordering::SeqCst);
        if activation == 0 {
            Ok(Credentials::UsernamePassword {
                username: self.username.clone(),
                password: self.password.clone(),
            })
        } else {
            // This deliberately tests the legacy SQL-auth pair behavior, NOT
            // Azure AD authentication or a fabricated token protocol.
            Ok(Credentials::Token {
                username: self.username.clone(),
                token: self.password.clone(),
            })
        }
    }
    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}
pub(super) async fn actual_override_and_refresh(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let plugins = plugins(dynamic)?;
    let factory = &plugins[2].factories()[0];
    let settings = db.settings();
    let provider = ActualProvider {
        username: settings["connection"]["user"]
            .as_str()
            .context("test application user")?
            .into(),
        password: settings["connection"]["password"]
            .as_str()
            .context("test application password")?
            .into(),
        calls: Arc::default(),
    };
    let mut connection = settings["connection"].clone();
    connection["user"] = json!("deliberately-wrong-literal");
    connection["password"] = json!("deliberately-wrong-literal");
    let mut sink = crate::sink_inputs::create_with_identity(
        factory,
        id("identity-sink"),
        json!({"settings":{
            "connection":connection,"outputs":{"q":{"component":"q","port":"out"}},
            "default_template":{"added":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect N'IDENTITY',1,1")}}
        }}),
        Arc::new(NativeIdentityResource::new(Arc::new(provider.clone()))),
    )?;
    let declared = sink.configuration()?;
    for _ in 0..2 {
        sink.start().await?;
        use drasi_core::evaluation::context::QueryPartEvaluationContext;
        let envelope = QueryChangeCodec::encode_evaluation(
            None,
            &id("q"),
            SystemMetadata::new(stream("q"), 1),
            &ResultNodeProjector::try_new("test/native-mssql-credentials/q", None)?,
            &[QueryPartEvaluationContext::Adding {
                after: Default::default(),
                row_signature: 1,
            }],
            drasi_computation_contracts::QueryOutputMetadata {
                query_id: "q".into(),
                source_id: None,
                timestamp: chrono::Utc::now(),
                metadata: Default::default(),
                profiling: None,
            },
        )?
        .context("fixture query envelope")?;
        sink.handle(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
        .await?;
        sink.stop().await?;
        ensure!(
            sink.configuration()? == declared,
            "provider credentials replaced declared configuration"
        );
    }
    ensure!(
        provider.calls.load(Ordering::SeqCst) == 2,
        "provider was not resolved for each activation"
    );
    ensure!(
        db.sql(
            "SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE operation=N'IDENTITY'"
        )? == "2",
        "provider-authenticated effects were not observed"
    );
    db.remove()
}
