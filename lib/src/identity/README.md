# Identity Providers

This module defines the `IdentityProvider` trait and includes a built-in `PasswordIdentityProvider` for static username/password authentication.

For cloud-native identity providers (Azure Entra ID, AWS IAM), see [`components/identity/`](../../../components/identity/).

## IdentityProvider Trait

```rust
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials>;
    fn clone_box(&self) -> Box<dyn IdentityProvider>;
}
```

Sources and reactions call `get_credentials()` when they need to authenticate. The `CredentialContext` provides optional metadata (e.g., hostname, port) that providers can use to generate context-specific credentials.

## Credential Types

```rust
pub enum Credentials {
    UsernamePassword { username: String, password: String },
    Token { username: String, token: String },
    Certificate { cert_pem: String, key_pem: String, username: Option<String> },
}
```

## PasswordIdentityProvider

Returns static `UsernamePassword` credentials. No external dependencies required.

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let provider = PasswordIdentityProvider::new("myuser", "mypassword");
```

### Usage with DrasiLib

```rust
use drasi_lib::DrasiLib;
use drasi_lib::identity::PasswordIdentityProvider;
use std::sync::Arc;

let provider = PasswordIdentityProvider::new("myuser", "mypassword");

let app = DrasiLib::builder()
    .with_id("my-app")
    .with_identity_provider(Arc::new(provider))
    // ... sources, reactions, queries
    .build()
    .await?;
```

### Usage with a Reaction Builder

```rust
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_identity_provider(PasswordIdentityProvider::new("myuser", "mypassword"))
    .with_ssl(false)
    .build()
    .await?;
```

## Security Note

`PasswordIdentityProvider` returns the same credentials on every call. Use it for development and testing. For production, use a cloud identity provider from [`components/identity/`](../../../components/identity/) instead.
