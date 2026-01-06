// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Azure Active Directory authentication for MySQL
//!
//! This module provides helper functions and constants for authenticating to
//! Azure Database for MySQL using Azure Active Directory credentials.

use anyhow::Result;

/// Azure AD OAuth scope for Azure Database for MySQL
///
/// This scope is used when authenticating to Azure Database for MySQL
/// using Azure Active Directory credentials. The token obtained with this scope
/// can be used as the password field in MySQL connection strings.
///
/// Note: Azure Database for MySQL uses the same OAuth scope as PostgreSQL
/// (the "Azure Database for OSS RDBMS" scope).
///
/// # Azure Documentation
/// For more information, see:
/// <https://learn.microsoft.com/en-us/azure/mysql/flexible-server/how-to-azure-ad>
pub const AZURE_AD_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

/// Get an Azure AD token for MySQL using DefaultAzureCredential
///
/// This is a convenience function for MySQL-specific authentication.
/// It uses DefaultAzureCredential which tries multiple authentication methods
/// in order: environment variables, managed identity, Azure CLI, Azure PowerShell, etc.
///
/// The returned token can be used as the password field in MySQL connection strings
/// when connecting to Azure Database for MySQL with Azure AD authentication enabled.
///
/// # Important
/// When using Azure AD authentication with MySQL, you must also enable the cleartext
/// password plugin on the client side. Set `enable_cleartext_plugin: true` in your
/// MySqlStoredProcReactionConfig.
///
/// # Example
/// ```rust,ignore
/// use drasi_reaction_storedproc_mysql::azure_auth::get_mysql_aad_token;
///
/// let token = get_mysql_aad_token().await?;
/// // Use token in your MySqlStoredProcReactionConfig
/// let config = MySqlStoredProcReactionConfig {
///     aad_token: Some(token),
///     enable_cleartext_plugin: true,  // Required for AAD auth with MySQL
///     // ... other fields
/// };
/// ```
///
/// # Errors
/// Returns an error if the credential cannot be obtained or if the token request fails.
pub async fn get_mysql_aad_token() -> Result<String> {
    drasi_auth_azure::get_token_with_default_credential(AZURE_AD_SCOPE).await
}

/// Get an Azure AD token for MySQL using Service Principal credentials
///
/// This function uses Service Principal (client credentials) authentication
/// to obtain an Azure AD token for MySQL.
///
/// # Arguments
/// * `tenant_id` - Azure AD tenant ID
/// * `client_id` - Application (client) ID of the service principal
/// * `client_secret` - Client secret of the service principal
///
/// # Example
/// ```rust,ignore
/// use drasi_reaction_storedproc_mysql::azure_auth::get_mysql_aad_token_with_service_principal;
///
/// let token = get_mysql_aad_token_with_service_principal(
///     "00000000-0000-0000-0000-000000000000", // tenant_id
///     "11111111-1111-1111-1111-111111111111", // client_id
///     "your-client-secret"
/// ).await?;
/// ```
///
/// # Errors
/// Returns an error if the credential cannot be created or if the token request fails.
pub async fn get_mysql_aad_token_with_service_principal(
    tenant_id: impl Into<String>,
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
) -> Result<String> {
    drasi_auth_azure::get_token_with_service_principal(
        AZURE_AD_SCOPE,
        tenant_id,
        client_id,
        client_secret,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_azure_ad_scope_constant() {
        // Verify the scope constant is correct for Azure Database for MySQL
        // MySQL uses the same scope as PostgreSQL (OSS RDBMS)
        assert_eq!(
            AZURE_AD_SCOPE,
            "https://ossrdbms-aad.database.windows.net/.default"
        );
    }
}
