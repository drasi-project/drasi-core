# Identity Providers

Identity providers enable Drasi sources and reactions to authenticate with databases and external services using cloud-native identity solutions instead of static credentials.

## How It Works

The `IdentityProvider` trait (defined in `drasi-lib`) provides a single method:

```rust
async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials>;
```

When a source or reaction needs to connect to a database, the framework calls `get_credentials()` to obtain fresh credentials. This happens transparently — your source/reaction code doesn't need to manage tokens or passwords directly.

## Available Providers

| Provider | Crate | Use Case |
|----------|-------|----------|
| [Azure Identity](azure/) | `drasi-identity-azure` | Azure Managed Identity, Workload Identity, developer tools (`az login`) |
| [AWS Identity](aws/) | `drasi-identity-aws` | AWS IAM, IRSA (EKS), assumed roles |
| Password (built-in) | `drasi-lib` | Static username/password — no extra dependency needed |

## Setting Up an Identity Provider

Identity providers can be configured at two levels:

### 1. Framework Level (recommended)

Set the identity provider on the `DrasiLib` builder. The framework injects it into all sources and reactions automatically:

```rust
use drasi_lib::DrasiLib;
use drasi_identity_azure::AzureIdentityProvider;
use std::sync::Arc;

let identity_provider = AzureIdentityProvider::with_default_credentials("myuser@myserver")?;

let app = DrasiLib::builder()
    .with_id("my-app")
    .with_identity_provider(Arc::new(identity_provider))
    .with_source(my_source)
    .with_reaction(my_reaction)
    .with_query(my_query)
    .build()
    .await?;
```

### 2. Component Level

Set the identity provider directly on a specific reaction or source builder. This takes precedence over the framework-level provider:

```rust
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.postgres.database.azure.com")
    .with_port(5432)
    .with_database("mydb")
    .with_identity_provider(my_provider)
    .with_ssl(true)
    .build()
    .await?;
```

### 3. Password Provider (built-in)

No extra crate needed — `PasswordIdentityProvider` is included in `drasi-lib`:

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let provider = PasswordIdentityProvider::new("username", "password");
```

## Credential Types

Identity providers return one of three credential types:

- **`Credentials::Token { username, token }`** — Used by Azure and AWS providers. The token is an OAuth/IAM access token.
- **`Credentials::UsernamePassword { username, password }`** — Used by `PasswordIdentityProvider`.
- **`Credentials::Certificate { cert_pem, key_pem, username }`** — For mTLS client certificate authentication.

## Security Notes

- Cloud tokens are short-lived (Azure: ~1 hour, AWS: ~15 minutes). Providers fetch fresh tokens on each call — no caching or refresh logic needed.
- `PasswordIdentityProvider` returns static credentials. Use it for development/testing only.
- Never hardcode credentials in source code. Use environment variables or the cloud identity providers for production.
