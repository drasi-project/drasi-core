# Identity Providers

This module provides identity provider abstractions for database authentication in Drasi reactions. Identity providers enable secure, token-based authentication with cloud databases without embedding credentials in your code.

## Overview

The identity provider pattern allows reactions to authenticate with databases using cloud-native identity solutions:

- **Azure AD Authentication** - Use Azure Managed Identity or Workload Identity for Azure Database for PostgreSQL/MySQL
- **AWS IAM Authentication** - Use AWS IAM users or roles for Amazon RDS and Aurora databases
- **Password Authentication** - Traditional username/password authentication


## Available Providers

### Password Provider

Basic username/password authentication:

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let provider = PasswordIdentityProvider::new("myuser", "mypassword");
```

### Azure AD Provider

Azure AD authentication with Managed Identity or Workload Identity:

```rust
use drasi_lib::identity::AzureIdentityProvider;

// For Azure Kubernetes Service (AKS) with Workload Identity
let provider = AzureIdentityProvider::with_workload_identity("myuser@myserver")?;

// For Azure VMs with Managed Identity or local development with Azure CLI
let provider = AzureIdentityProvider::with_default_credentials("myuser@myserver")?;
```

See [azure/README.md](azure/README.md) for detailed Azure setup.

### AWS IAM Provider

AWS IAM authentication for RDS and Aurora:

```rust
use drasi_lib::identity::AwsIdentityProvider;

// Using IAM user credentials
let provider = AwsIdentityProvider::new(
    "myuser",
    "mydb.rds.amazonaws.com",
    5432
).await?;

// Assuming an IAM role
let provider = AwsIdentityProvider::with_assumed_role(
    "myuser",
    "mydb.rds.amazonaws.com",
    5432,
    "arn:aws:iam::123456789012:role/RDSAccessRole",
    None
).await?;
```

See the AWS provider documentation in this repository for detailed AWS setup.

## Usage with Reactions

Identity providers integrate seamlessly with database reactions:

```rust
use drasi_lib::identity::AzureIdentityProvider;
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;

// Create identity provider
let identity_provider = AzureIdentityProvider::with_workload_identity(
    "myuser@myserver"
)?;

// Use with reaction
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.postgres.database.azure.com")
    .with_port(5432)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)
    .build()
    .await?;
```

## Feature Flags

Identity providers are optional dependencies controlled by feature flags:

```toml
[dependencies]
drasi-lib = { version = "0.3", features = ["azure-identity"] }
drasi-lib = { version = "0.3", features = ["aws-identity"] }
drasi-lib = { version = "0.3", features = ["all-identity"] }
```

- `azure-identity` - Enables Azure AD authentication
- `aws-identity` - Enables AWS IAM authentication
- `all-identity` - Enables all cloud identity providers

Password authentication is always available (no feature flag required).

## Security Considerations

### Token Lifecycle

Cloud identity tokens are typically short-lived (15 minutes for AWS, 1 hour for Azure). The identity provider automatically:
- Generates fresh tokens on each `get_credentials()` call
- Handles token refresh transparently
- Requires no application-level token management

### Credential Storage

Identity providers **never store or cache credentials**. Each authentication request:
1. Obtains fresh credentials from the cloud provider
2. Returns them to the reaction
3. Discards the credentials after use

This ensures:
- No credential leakage through memory dumps
- No stale tokens
- Automatic credential rotation

### SSL/TLS and Certificate Validation

Always use SSL/TLS connections with cloud databases:
- Azure Database for PostgreSQL/MySQL requires SSL by default
- AWS RDS should have SSL enabled
- Use `.with_ssl(true)` when building reactions

**Certificate Validation:**

The implementation validates SSL/TLS certificates against your system's trust store. This ensures secure, authenticated connections.

**For AWS RDS:**

AWS RDS certificates must be installed in your system trust store:

```bash
# Download AWS RDS CA certificate bundle
curl -o ~/rds-ca-bundle.pem https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem

# macOS: Install to system keychain
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain ~/rds-ca-bundle.pem

# Linux: Install to system CA store (Ubuntu/Debian)
sudo cp ~/rds-ca-bundle.pem /usr/local/share/ca-certificates/rds-ca-bundle.crt
sudo update-ca-certificates

# Linux: Install to system CA store (RHEL/CentOS)
sudo cp ~/rds-ca-bundle.pem /etc/pki/ca-trust/source/anchors/
sudo update-ca-trust
```

**For Azure Database for PostgreSQL/MySQL:**

Azure uses DigiCert Global Root G2, which is typically already in system trust stores. No additional certificate installation is usually required.

**Certificate Verification Details:**

- ✅ Validates certificate chain against system trust store
- ✅ Validates certificate hostname matches connection hostname
- ✅ Enforces TLS 1.2 minimum
- ❌ Does NOT accept self-signed or invalid certificates

## Examples

See the example directories for complete working examples:

- [examples/postgres-azure-identity](../../../examples/postgres-azure-identity) - Azure AD with PostgreSQL
- [examples/postgres-aws-identity](../../../examples/postgres-aws-identity) - AWS IAM with PostgreSQL

## Testing

Unit tests are always run:
```bash
cargo test -p drasi-lib
```

Integration tests require cloud credentials and are ignored by default:
```bash
# Run Azure integration tests
cargo test -p drasi-lib --features azure-identity -- --ignored test_azure

# Run AWS integration tests
cargo test -p drasi-lib --features aws-identity -- --ignored test_aws
```

## Troubleshooting

### Azure

- **Error: "identity not assigned"** - Ensure Managed Identity is enabled and federated credentials are configured
- **Error: "NoopClient"** - Add `reqwest` feature to `azure_core` dependency
- **SSL errors** - Azure uses DigiCert Global Root G2, which should be in system trust store

### AWS

- **Error: "InvalidClientTokenId"** - Run `aws configure` to set up valid credentials
- **Error: "Access Denied"** - Attach `rds-db:connect` policy to IAM user/role
- **SSL errors** - Install [AWS RDS CA certificate bundle](https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem)

### General

- **Error: "PAM authentication failed"** - Database user doesn't exist or isn't granted cloud auth role (`rds_iam` for AWS, Azure AD role for Azure)
- **Connection timeout** - Check security groups, firewalls, and network connectivity

## Further Reading

- [Azure Database for PostgreSQL - Azure AD Authentication](https://learn.microsoft.com/en-us/azure/postgresql/flexible-server/concepts-azure-ad-authentication)
- [AWS RDS - IAM Database Authentication](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/UsingWithRDS.IAMDBAuth.html)
- [Drasi Reactions Documentation](https://drasi.io/reference/reactions/)
