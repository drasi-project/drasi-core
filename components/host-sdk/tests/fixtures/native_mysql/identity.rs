// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};

#[derive(Clone)]
struct FixedIdentity;

#[async_trait]
impl IdentityProvider for FixedIdentity {
    async fn get_credentials(&self, _context: &CredentialContext) -> Result<Credentials> {
        Ok(Credentials::UsernamePassword {
            username: "native_identity".into(),
            password: "first-auth".into(),
        })
    }
    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

/// The plugin still declares activation credentials (its Start consumes
/// them; see the crate's unit tests), but the Host does not yet bind an
/// identity to an external consumer: the consumer creation paths take no
/// identity and the ordinary identity path refuses a consumer factory.
/// Until the Host forwards activation credentials to consumers, a consumer
/// sink uses literal credentials.
pub(super) fn rotation(factory: &computation::NativeFactory) -> Result<()> {
    assert!(factory.supports_activation_credentials());
    let identity = Arc::new(computation::NativeIdentityResource::new(Arc::new(
        FixedIdentity,
    )));
    let mut settings = json!({
        "hostname": "127.0.0.1", "database": "test",
        "outputs": {"left": {"component": id("left").as_str(), "port": "out"}},
        "defaultTemplate": {"added": {"template": "CALL credential_effect({{param output.alias}})"}}
    });
    settings["port"] = json!(3306);
    assert!(
        crate::sink_inputs::create_with_identity(
            factory,
            id("identity-procedures"),
            settings,
            identity,
        )
        .is_err(),
        "the Host consumer path does not bind activation identities yet"
    );
    Ok(())
}
