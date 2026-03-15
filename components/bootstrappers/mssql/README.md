# MS SQL Bootstrap Provider for Drasi

A Microsoft SQL Server bootstrap provider for the Drasi platform. This provider snapshots table data at query startup to provide initial state before CDC processing begins.

## Quick Start

```rust
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("localhost")
    .with_port(1433)
    .with_database("MyDatabase")
    .with_user("drasi_user")
    .with_password("password")
    .with_tables(vec!["dbo.orders".to_string(), "dbo.customers".to_string()])
    .build()?;
```

## Authentication

The MSSQL bootstrap provider supports multiple authentication methods:

### 1. SQL Server Authentication (Username/Password)

```rust
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("localhost")
    .with_port(1433)
    .with_database("MyDatabase")
    .with_user("drasi_user")
    .with_password("secure_password")
    .with_tables(vec!["dbo.orders".to_string()])
    .build()?;
```

### 2. Azure AD with Developer Tools (az login)

```rust
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;
use drasi_identity_azure::AzureIdentityProvider;
use drasi_mssql_common::EncryptionMode;

let identity = AzureIdentityProvider::with_default_credentials(
    "user@company.onmicrosoft.com"
)?;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("myserver.database.windows.net")
    .with_port(1433)
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity)
    .with_encryption(EncryptionMode::NotSupported)
    .with_trust_server_certificate(false)
    .build()?;
```

**Setup:**
```bash
# Login with Azure CLI
az login

# Create Azure AD user in database (connect as AD admin)
sqlcmd -S myserver.database.windows.net -d production -G
```

```sql
CREATE USER [user@company.onmicrosoft.com] FROM EXTERNAL PROVIDER;
ALTER ROLE db_owner ADD MEMBER [user@company.onmicrosoft.com];
GO
```

### 3. Azure AD with System-Assigned Managed Identity

```rust
use drasi_identity_azure::AzureIdentityProvider;

let identity = AzureIdentityProvider::new("my-vm-identity")?;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("myserver.database.windows.net")
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity)
    .with_encryption(EncryptionMode::NotSupported)
    .build()?;
```

**Setup:**
```bash
# Enable system-assigned managed identity on VM
az vm identity assign --name myVM --resource-group myRG
```

```sql
-- Create managed identity user in database
CREATE USER [my-vm-identity] FROM EXTERNAL PROVIDER;
ALTER ROLE db_owner ADD MEMBER [my-vm-identity];
GO
```

### 4. Azure AD with User-Assigned Managed Identity

```rust
let identity = AzureIdentityProvider::with_managed_identity(
    "drasi-identity",
    "12345678-1234-1234-1234-123456789abc"  // Client ID
)?;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("myserver.database.windows.net")
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity)
    .build()?;
```

### 5. Azure AD with Workload Identity (AKS)

```rust
let identity = AzureIdentityProvider::with_workload_identity(
    "drasi-workload-id"
)?;

let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("myserver.database.windows.net")
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity)
    .build()?;
```

**Requires environment variables:**
- `AZURE_CLIENT_ID`
- `AZURE_TENANT_ID`

For complete authentication documentation, see [Authentication Guide](../../docs/AUTHENTICATION.md).

## Configuration

```yaml
host: localhost           # MS SQL Server hostname
port: 1433               # Port (default: 1433)
database: MyDatabase     # Database name
user: drasi_user         # Username (not needed with identity provider)
password: secret         # Password (not needed with identity provider)
tables:                  # Tables to bootstrap (must include schema prefix)
  - dbo.orders
  - dbo.customers
encryption: not_supported # Options: 'off', 'on', 'not_supported' (default)
trustServerCertificate: false # Trust self-signed certs (default: false)
```

**Important:** Table names must include schema prefix (e.g., `dbo.orders` not just `orders`).

## How It Works

### Bootstrap Process

1. **Start Transaction** with snapshot isolation:
```sql
SET TRANSACTION ISOLATION LEVEL SNAPSHOT;
BEGIN TRANSACTION;
```

2. **Discover Primary Keys** for each table

3. **Query Table Data**:
```sql
SELECT * FROM dbo.orders;
```

4. **Emit Records** in batches to Drasi

5. **Commit Transaction**

### Element ID Generation

The bootstrap provider uses the same ID generation as the CDC source:
- **Single PK**: `{table}:{pk_value}` (e.g., `orders:12345`)
- **Composite PK**: `{table}:{pk1}_{pk2}` (e.g., `order_items:12345_67890`)
- **No PK**: `{table}:{uuid}` (fallback with warning)

## Usage with MSSQL Source

The bootstrap provider is typically used with the MSSQL CDC source to provide initial state:

```rust
use drasi_source_mssql::MsSqlSource;
use drasi_bootstrap_mssql::MsSqlBootstrapProvider;
use drasi_identity_azure::AzureIdentityProvider;
use drasi_mssql_common::EncryptionMode;

// Create identity provider
let identity = AzureIdentityProvider::with_default_credentials(
    "user@company.onmicrosoft.com"
)?;

// Create bootstrap provider
let bootstrap = MsSqlBootstrapProvider::builder()
    .with_host("myserver.database.windows.net")
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity.clone())
    .with_encryption(EncryptionMode::NotSupported)
    .build()?;

// Create MSSQL source with bootstrap
let source = MsSqlSource::builder("mssql-source")
    .with_host("myserver.database.windows.net")
    .with_database("production")
    .with_tables(vec!["dbo.orders".to_string()])
    .with_identity_provider(identity)
    .with_encryption(EncryptionMode::NotSupported)
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

**Note:** Ensure both the bootstrap provider and source use the same:
- Authentication settings (identity provider or credentials)
- Encryption settings
- Host/database configuration

## Prerequisites

### MS SQL Server Setup

1. Create user with SELECT permissions:
```sql
-- SQL Server Authentication
CREATE LOGIN drasi_user WITH PASSWORD = 'password';
CREATE USER drasi_user FOR LOGIN drasi_user;
GRANT SELECT ON dbo.orders TO drasi_user;
GRANT SELECT ON dbo.customers TO drasi_user;
```

Or for Azure AD:
```sql
-- Azure AD Authentication
CREATE USER [user@company.onmicrosoft.com] FROM EXTERNAL PROVIDER;
ALTER ROLE db_datareader ADD MEMBER [user@company.onmicrosoft.com];
```

2. Ensure snapshot isolation is available:
```sql
-- Check if snapshot isolation is enabled
SELECT name, snapshot_isolation_state_desc 
FROM sys.databases 
WHERE name = 'MyDatabase';

-- Enable if needed
ALTER DATABASE MyDatabase SET ALLOW_SNAPSHOT_ISOLATION ON;
```

## Testing

```bash
# Run unit tests
cargo test -p drasi-bootstrap-mssql

# Run with real MS SQL (requires instance)
cargo test -p drasi-bootstrap-mssql -- --ignored
```

## Modules

- `mssql.rs` - Main bootstrap provider implementation
- `descriptor.rs` - Plugin descriptor for dynamic loading

## Dependencies

- `drasi-lib` - Core Drasi types and traits
- `drasi-core` - Element types and models
- `drasi-mssql-common` - Shared MSSQL utilities
- `tiberius` - MS SQL client library
- `tokio` - Async runtime
- `serde` - Serialization

## Plugin Packaging

This bootstrap provider is compiled as a dynamic plugin (cdylib) that can be loaded by drasi-server at runtime.

**Key files:**
- `Cargo.toml` — includes `crate-type = ["lib", "cdylib"]`
- `src/descriptor.rs` — implements `BootstrapProviderPluginDescriptor` with kind `"mssql"`
- `src/mssql.rs` — invokes `drasi_plugin_sdk::export_plugin!` to export the plugin entry point

**Building:**
```bash
cargo build -p drasi-bootstrap-mssql
```

The compiled `.so` (Linux) / `.dylib` (macOS) / `.dll` (Windows) is placed in `target/debug/` and can be copied to the server's `plugins/` directory.

## License

Apache License 2.0

## Authors

Drasi Contributors
