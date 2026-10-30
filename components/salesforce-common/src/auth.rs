use anyhow::{bail, ensure, Context, Result};
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::config::{SalesforceAuthConfig, SalesforceConnectionConfig};

#[derive(Clone)]
pub struct SalesforceSessionToken {
    pub access_token: String,
    pub instance_url: String,
    pub org_id: String,
}

impl std::fmt::Debug for SalesforceSessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SalesforceSessionToken")
            .field("access_token", &"[REDACTED]")
            .field("instance_url", &self.instance_url)
            .field("org_id", &self.org_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct SalesforceAuthManager {
    connection: SalesforceConnectionConfig,
    identity_provider: Option<Arc<dyn IdentityProvider>>,
    http_client: reqwest::Client,
    cached_oauth_token: Arc<RwLock<Option<CachedOAuthToken>>>,
}

#[derive(Clone)]
struct CachedOAuthToken {
    access_token: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl SalesforceAuthManager {
    pub fn new(
        connection: SalesforceConnectionConfig,
        identity_provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Result<Self> {
        connection.validate()?;
        Ok(Self {
            connection,
            identity_provider,
            http_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .context("failed to construct Salesforce OAuth client")?,
            cached_oauth_token: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn session_token(&self) -> Result<SalesforceSessionToken> {
        let access_token = if let Some(provider) = &self.identity_provider {
            self.identity_provider_token(provider).await?
        } else {
            self.oauth_client_credentials_token().await?
        };

        validate_access_token(&access_token)?;

        Ok(SalesforceSessionToken {
            access_token,
            instance_url: self.connection.instance_url.clone(),
            org_id: self.connection.expected_org_id.clone(),
        })
    }

    pub async fn invalidate_oauth_cache(&self) {
        *self.cached_oauth_token.write().await = None;
    }

    async fn identity_provider_token(
        &self,
        provider: &Arc<dyn IdentityProvider>,
    ) -> Result<String> {
        let context = CredentialContext::new()
            .with_property("instance_url", &self.connection.instance_url)
            .with_property("org_id", &self.connection.expected_org_id);

        let credentials = provider
            .get_credentials(&context)
            .await
            .context("failed to resolve Salesforce credentials from identity provider")?;

        match credentials {
            Credentials::Token { token, .. } => Ok(token),
            _ => bail!("Salesforce source requires Token credentials from identity provider"),
        }
    }

    async fn oauth_client_credentials_token(&self) -> Result<String> {
        if let Some(cached) = self.cached_oauth_token.read().await.as_ref() {
            if cached.expires_at > Instant::now() {
                return Ok(cached.access_token.clone());
            }
        }

        let SalesforceAuthConfig::ClientCredentials {
            token_url,
            client_id,
            client_secret,
        } = &self.connection.auth;

        let response = self
            .http_client
            .post(token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
            ])
            .send()
            .await
            .context("failed to request Salesforce OAuth token")?
            .error_for_status()
            .context("Salesforce OAuth token request failed")?;

        let body: OAuthTokenResponse = crate::rest::read_json_bounded(response, 64 * 1024)
            .await
            .context("failed to parse Salesforce OAuth token response")?;

        validate_access_token(&body.access_token)?;

        let ttl = Duration::from_secs(body.expires_in.unwrap_or(300));
        ensure!(
            !ttl.is_zero(),
            "Salesforce returned an already expired OAuth token"
        );
        let refresh_before = Duration::from_secs(30).min(ttl / 2);
        let cached = CachedOAuthToken {
            access_token: body.access_token.clone(),
            expires_at: Instant::now()
                .checked_add(ttl - refresh_before)
                .context("Salesforce OAuth token expiry is out of range")?,
        };
        *self.cached_oauth_token.write().await = Some(cached);

        Ok(body.access_token)
    }
}

pub fn validate_access_token(token: &str) -> Result<()> {
    ensure!(!token.is_empty(), "OAuth access token is empty");
    ensure!(
        token
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !byte.is_ascii_whitespace()),
        "OAuth access token contains whitespace or invalid characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol_tests::auth_connection;
    use serde_json::json;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn oauth_expiry_defaults_short_ttls_and_overflow() -> Result<()> {
        let server = MockServer::start().await;
        for (expires_in, expected) in [
            (None, Ok(Duration::from_secs(270))),
            (Some(1), Ok(Duration::from_millis(500))),
            (Some(10), Ok(Duration::from_secs(5))),
            (Some(60), Ok(Duration::from_secs(30))),
            (Some(61), Ok(Duration::from_secs(31))),
            (Some(0), Err("already expired")),
            (Some(u64::MAX), Err("expiry is out of range")),
        ] {
            server.reset().await;
            let mut body = json!({"access_token": "fixture-token"});
            if let Some(expires_in) = expires_in {
                body["expires_in"] = json!(expires_in);
            }
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            let auth = SalesforceAuthManager::new(auth_connection(&server), None)?;
            let before = Instant::now();
            let result = auth.session_token().await;
            match expected {
                Ok(lease) => {
                    let session = result?;
                    let expiry = auth
                        .cached_oauth_token
                        .read()
                        .await
                        .as_ref()
                        .expect("successful OAuth response should be cached")
                        .expires_at;
                    assert!(expiry >= before + lease);
                    assert!(expiry <= Instant::now() + lease);
                    assert_eq!(server.received_requests().await.unwrap().len(), 1);
                    if expires_in.is_none() {
                        assert_eq!(
                            auth.session_token().await?.access_token,
                            session.access_token
                        );
                        assert_eq!(server.received_requests().await.unwrap().len(), 1);
                        auth.cached_oauth_token
                            .write()
                            .await
                            .as_mut()
                            .unwrap()
                            .expires_at = Instant::now();
                        auth.session_token().await?;
                        assert_eq!(server.received_requests().await.unwrap().len(), 2);
                    }
                }
                Err(message) => {
                    let error = result.unwrap_err();
                    assert!(format!("{error:#}").contains(message), "{error:#}");
                    assert!(auth.cached_oauth_token.read().await.is_none());
                }
            }
        }
        Ok(())
    }
}
