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

//! Azure Active Directory authentication for Azure SQL Database
//!
//! This module provides helper functions and constants for authenticating to
//! Azure SQL Database using Azure Active Directory credentials.

use anyhow::Result;

/// Azure AD OAuth scope for Azure SQL Database
///
/// This scope is used when authenticating to Azure SQL Database
/// using Azure Active Directory credentials. The token obtained with this scope
/// can be used for authentication in SQL Server connection strings.
///
/// Note: Azure SQL Database uses a different OAuth scope than PostgreSQL/MySQL.
/// PostgreSQL and MySQL use the "OSS RDBMS" scope, while Azure SQL uses the
/// "Azure SQL Database" scope.
///
/// # Azure Documentation
/// For more information, see:
/// <https://learn.microsoft.com/en-us/azure/azure-sql/database/authentication-aad-overview>
pub const AZURE_AD_SCOPE: &str = "https://database.windows.net/.default";

/// Get an Azure AD token for Azure SQL Database using DefaultAzureCredential
///
/// This is a convenience function for Azure SQL Database-specific authentication.
/// It uses DefaultAzureCredential which tries multiple authentication methods
/// in order: environment variables, managed identity, Azure CLI, Azure PowerShell, etc.
///
/// The returned token can be used for authentication when connecting to Azure SQL Database
/// with Azure AD authentication enabled.
///
/// # Example
/// ```rust,ignore
/// use drasi_reaction_storedproc_mssql::azure_auth::get_mssql_aad_token;
///
/// let token = get_mssql_aad_token().await?;
/// // Use token in your MSSqlStoredProcReactionConfig
/// let config = MSSqlStoredProcReactionConfig {
///     aad_token: Some(token),
///     // ... other fields
/// };
/// ```
///
/// # Errors
/// Returns an error if the credential cannot be obtained or if the token request fails.
pub async fn get_mssql_aad_token() -> Result<String> {
    drasi_auth_azure::get_token_with_default_credential(AZURE_AD_SCOPE).await
}

/// Get an Azure AD token for Azure SQL Database using Service Principal credentials
///
/// This function uses Service Principal (client credentials) authentication
/// to obtain an Azure AD token for Azure SQL Database.
///
/// # Arguments
/// * `tenant_id` - Azure AD tenant ID
/// * `client_id` - Application (client) ID of the service principal
/// * `client_secret` - Client secret of the service principal
///
/// # Example
/// ```rust,ignore
/// use drasi_reaction_storedproc_mssql::azure_auth::get_mssql_aad_token_with_service_principal;
///
/// let token = get_mssql_aad_token_with_service_principal(
///     "00000000-0000-0000-0000-000000000000", // tenant_id
///     "11111111-1111-1111-1111-111111111111", // client_id
///     "your-client-secret"
/// ).await?;
/// ```
///
/// # Errors
/// Returns an error if the credential cannot be created or if the token request fails.
pub async fn get_mssql_aad_token_with_service_principal(
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
        // Verify the scope constant is correct for Azure SQL Database
        // This is different from PostgreSQL/MySQL which use the OSS RDBMS scope
        assert_eq!(AZURE_AD_SCOPE, "https://database.windows.net/.default");
    }
}
