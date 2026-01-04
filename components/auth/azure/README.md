# Azure Identity Authentication for Drasi

Azure Identity authentication library for Drasi components, providing Azure Active Directory credential support for sources, reactions, and other plugins.

## Overview

This crate provides authentication using Azure Identity, supporting various credential types:

- **DefaultAzureCredential**: Automatically tries multiple credential types in order
- **Managed Identity**: System-assigned or user-assigned managed identities
- **Service Principal**: Client credentials flow with tenant ID, client ID, and secret

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
drasi-auth-azure = { path = "path/to/components/auth/azure" }
```

## Usage Examples

### DefaultAzureCredential

The easiest way to get started - automatically tries multiple credential sources:

```rust
use drasi_auth_azure::AzureIdentityAuth;

let auth = AzureIdentityAuth::default();
let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
```

### Managed Identity (System-Assigned)

Use when running on Azure services like VM, App Service, or AKS:

```rust
use drasi_auth_azure::AzureIdentityAuth;

let auth = AzureIdentityAuth::managed_identity();
let credential = auth.build_credential().await?;
```

### Managed Identity (User-Assigned)

Specify a client ID for user-assigned managed identity:

```rust
use drasi_auth_azure::AzureIdentityAuth;

let auth = AzureIdentityAuth::managed_identity_with_client("my-client-id");
let credential = auth.build_credential().await?;
```

### Service Principal

Use client credentials (tenant ID, client ID, secret):

```rust
use drasi_auth_azure::AzureIdentityAuth;

let auth = AzureIdentityAuth::service_principal(
    "tenant-id",
    "client-id",
    "client-secret"
);
let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
```

## Common Token Scopes

### Azure SQL Database / PostgreSQL
```rust
let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
```

### Azure Storage
```rust
let token = auth.get_token(&["https://storage.azure.com/.default"]).await?;
```

### Azure Key Vault
```rust
let token = auth.get_token(&["https://vault.azure.net/.default"]).await?;
```

## Environment Variables

The Azure Identity SDK respects these environment variables:

- `AZURE_TENANT_ID`: Azure AD tenant ID
- `AZURE_CLIENT_ID`: Application (client) ID
- `AZURE_CLIENT_SECRET`: Client secret
- `AZURE_AUTHORITY_HOST`: Azure AD authority host (optional)

These are automatically used by `DefaultAzureCredential`.

## Integration Example

### PostgreSQL with Azure Identity

```rust
use drasi_auth_azure::AzureIdentityAuth;

// Configure Azure auth
let auth = AzureIdentityAuth::managed_identity();

// Get access token for Azure PostgreSQL
let token = auth.get_token(&["https://ossrdbms-aad.database.windows.net/.default"]).await?;

// Use token as password in PostgreSQL connection
// Username format: <identity-name>@<server-name>
```

## Features

- **Type-safe**: Uses Rust enums for different credential types
- **Async-first**: All operations are async
- **Flexible**: Supports multiple credential flows
- **Serializable**: Can be configured via JSON/YAML

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
