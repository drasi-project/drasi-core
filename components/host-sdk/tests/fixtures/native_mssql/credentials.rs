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
/// Database-free checks of the actual library: the sink is an external
/// consumer that exports activation credentials, refuses unbound empty
/// literals and constructs without I/O. Identity bindings are not yet offered
/// to external consumers by the Host (`"identity"` requires a non-consumer
/// factory), so provider credentials and Entra ID tokens cannot reach this
/// sink until the Host supports them; this test records that refusal.
async fn public_binding(dynamic: bool) -> Result<()> {
    let native = plugins(dynamic)?;
    let factory = &native[2].factories()[0];
    let sink_id = ComponentId::try_new("00000000-0000-0000-0000-000000000121")?;
    let output_id = ComponentId::try_new("00000000-0000-0000-0000-000000000081")?;
    ensure!(
        factory.supports_activation_credentials(),
        "actual MSSQL sink did not export activation credentials"
    );
    ensure!(
        factory.consumer_mode() == Some(drasi_computation_plugin_sdk::ConsumerMode::External),
        "actual MSSQL sink is not an external consumer"
    );
    let config = json!({
        "host":"credential-only.invalid","port":1437,"database":"d","user":"","password":"",
        "outputs":{"q":{"component":output_id,"port":"out"}},"commandTimeoutMs":100,
        "defaultTemplate":{"added":{"template":"EXEC dbo.p {{param after.id}}"}},
    });
    ensure!(
        crate::create_sink(factory, sink_id.clone(), config.clone())
            .await
            .is_err(),
        "unbound empty credentials were accepted"
    );
    let mut literal = config.clone();
    literal["user"] = json!("u");
    literal["password"] = json!("p");
    let (sink, _state) = crate::create_sink(factory, sink_id.clone(), literal.clone()).await?;
    ensure!(
        sink.configuration()?["host"] == "credential-only.invalid",
        "configuration did not round-trip"
    );
    drop(sink);
    let provider = Provider {
        calls: Arc::default(),
        certificate: Arc::default(),
        contexts: Arc::default(),
    };
    ensure!(
        crate::sink_inputs::create_with_identity(
            factory,
            sink_id,
            config,
            Arc::new(NativeIdentityResource::new(Arc::new(provider.clone()))),
        )
        .is_err(),
        "the Host now binds identities to external consumers: replace this check with \
         provider-credential and token activation tests"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
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

/// Live provider refresh is blocked: the Host does not bind identities to
/// external consumers yet. When it does, restore the real-server refresh
/// scenario here: provider SQL credentials overriding deliberately wrong
/// literals on each start, and (on Azure SQL) a `Token` credential used as a
/// Microsoft Entra ID access token.
pub(super) async fn actual_override_and_refresh(dynamic: bool) -> Result<()> {
    public_binding(dynamic).await
}
