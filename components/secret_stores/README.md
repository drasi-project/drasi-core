# Secret Store Plugins

Secret store plugins provide pluggable secret resolution for Drasi. When a plugin's configuration uses `ConfigValue::Secret { name }`, the configured secret store provider resolves the named secret during plugin initialization (when the consuming plugin's `create_source`, `create_reaction`, or `create_bootstrap` method runs).

## Available Providers

| Provider | Crate | Use Case |
|----------|-------|----------|
| **File** | `drasi-secret-store-file` | Development and testing — reads secrets from a JSON file on disk |
| **Keyring** | `drasi-secret-store-keyring` | Local development — uses the OS-native credential store (macOS Keychain, Linux Secret Service, Windows Credential Manager) |
| **Azure Key Vault** | `drasi-secret-store-azure-keyvault` | Production on Azure — resolves secrets from Azure Key Vault using Azure Identity credentials |

## Creating a Secret Store Plugin

A secret store plugin implements two traits from `drasi-plugin-sdk`:

1. **`SecretStoreProvider`** (from `drasi-lib`) — the runtime trait:
   ```rust
   #[async_trait]
   pub trait SecretStoreProvider: Send + Sync {
       async fn get_secret(&self, name: &str) -> anyhow::Result<String>;
   }
   ```

2. **`SecretStorePluginDescriptor`** (from `drasi-plugin-sdk`) — the factory trait:
   ```rust
   #[async_trait]
   pub trait SecretStorePluginDescriptor: Send + Sync {
       fn kind(&self) -> &str;
       fn config_version(&self) -> &str;
       fn config_schema_json(&self) -> String;
       fn config_schema_name(&self) -> &str;
       async fn create_secret_store(
           &self,
           config_json: &serde_json::Value,
       ) -> anyhow::Result<Box<dyn SecretStoreProvider>>;
   }
   ```

Register your descriptors using the `export_plugin!` macro:

```rust
drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-secret-store",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    secret_store_descriptors = [MySecretStoreDescriptor],
);
```

## Bootstrap Constraint

A secret store's own configuration **cannot** use `ConfigValue::Secret` references (circular dependency). Use `ConfigValue::Static` or `ConfigValue::EnvironmentVariable` for the secret store's own config properties.
