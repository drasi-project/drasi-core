use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

use crate::projection::validate_identifier;

fn default_api_version() -> String {
    "68.0".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SalesforceConnectionConfig {
    pub instance_url: String,
    pub expected_org_id: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    pub auth: SalesforceAuthConfig,
}

impl SalesforceConnectionConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.instance_url)?;
        let loopback = is_loopback_host(&url);
        if loopback {
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "loopback instance_url must use HTTP or HTTPS"
            );
        } else {
            ensure!(url.scheme() == "https", "instance_url must use HTTPS");
            ensure!(
                url.host_str()
                    .is_some_and(|host| host.ends_with(".salesforce.com")),
                "instance_url must target a Salesforce hostname"
            );
        }
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "instance_url must not contain embedded credentials"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "instance_url must not include query or fragment"
        );
        ensure!(
            matches!(self.expected_org_id.len(), 15 | 18)
                && self
                    .expected_org_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()),
            "expected_org_id must be a Salesforce 15/18-character identifier"
        );
        ensure!(
            self.api_version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.'),
            "api_version must contain only digits and '.'"
        );
        self.auth.validate()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SalesforceAuthConfig {
    ClientCredentials {
        #[serde(rename = "tokenUrl", alias = "token_url")]
        token_url: String,
        #[serde(rename = "clientId", alias = "client_id")]
        client_id: String,
        #[serde(rename = "clientSecret", alias = "client_secret")]
        client_secret: String,
    },
}

impl std::fmt::Debug for SalesforceAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientCredentials {
                token_url,
                client_id,
                ..
            } => f
                .debug_struct("ClientCredentials")
                .field("token_url", token_url)
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .finish(),
        }
    }
}

impl SalesforceAuthConfig {
    pub fn validate(&self) -> Result<()> {
        match self {
            SalesforceAuthConfig::ClientCredentials {
                token_url,
                client_id,
                client_secret,
            } => {
                let token_url = reqwest::Url::parse(token_url)?;
                if is_loopback_host(&token_url) {
                    ensure!(
                        matches!(token_url.scheme(), "http" | "https"),
                        "loopback token_url must use HTTP or HTTPS"
                    );
                } else {
                    ensure!(token_url.scheme() == "https", "token_url must use HTTPS");
                }
                ensure!(!client_id.trim().is_empty(), "client_id must not be empty");
                ensure!(
                    !client_secret.trim().is_empty(),
                    "client_secret must not be empty"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectProjectionConfig {
    pub object: String,
    pub fields: Vec<String>,
}

impl ObjectProjectionConfig {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.object)?;
        ensure!(
            !self.fields.is_empty(),
            "projection.fields must contain at least one selected field"
        );
        for field in &self.fields {
            validate_identifier(field)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum InitialCursorBehavior {
    #[default]
    Latest,
    Earliest,
}

pub(crate) fn is_loopback_host(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn auth(token_url: &str) -> SalesforceAuthConfig {
        SalesforceAuthConfig::ClientCredentials {
            token_url: token_url.to_string(),
            client_id: "client-id".to_string(),
            client_secret: "client-secret".to_string(),
        }
    }

    #[test]
    fn resolved_auth_debug_redacts_client_secret() {
        let config = SalesforceConnectionConfig {
            instance_url: "https://example.salesforce.com".into(),
            expected_org_id: "00D000000000001AAA".into(),
            api_version: "68.0".into(),
            auth: auth("https://example.salesforce.com/oauth/token"),
        };
        for output in [
            format!("{:?}", config.auth),
            format!("{:#?}", config.auth),
            format!("{config:?}"),
            format!("{config:#?}"),
        ] {
            assert!(!output.contains("client-secret"), "{output}");
            assert!(output.contains("<redacted>"), "{output}");
            assert!(output.contains("client-id"), "{output}");
        }
        assert_eq!(
            serde_json::to_value(config).unwrap()["auth"]["clientSecret"],
            "client-secret",
            "Debug redaction must not alter persisted configuration"
        );
    }

    #[test]
    fn production_config_requires_https_salesforce_hostname() {
        let config = SalesforceConnectionConfig {
            instance_url: "http://example.salesforce.com".to_string(),
            expected_org_id: "00D000000000001AAA".to_string(),
            api_version: "68.0".to_string(),
            auth: auth("https://example.salesforce.com/oauth/token"),
        };
        let error = config.validate().expect_err("http should be rejected");
        assert!(
            format!("{error:#}").contains("instance_url must use HTTPS"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn loopback_config_allows_http_for_protocol_harnesses() {
        let config = SalesforceConnectionConfig {
            instance_url: "http://127.0.0.1:8443".to_string(),
            expected_org_id: "00D000000000001AAA".to_string(),
            api_version: "68.0".to_string(),
            auth: auth("http://127.0.0.1:8443/oauth/token"),
        };
        config
            .validate()
            .expect("loopback protocol harness config should be valid");
    }

    #[test]
    fn auth_deserializes_camel_case_client_credentials_fields() {
        let auth: SalesforceAuthConfig = serde_json::from_value(json!({
            "type": "clientCredentials",
            "tokenUrl": "https://example.salesforce.com/oauth/token",
            "clientId": "fixture-client-id",
            "clientSecret": "fixture-client-secret"
        }))
        .expect("camelCase auth fields should deserialize");

        match auth {
            SalesforceAuthConfig::ClientCredentials {
                token_url,
                client_id,
                client_secret,
            } => {
                assert_eq!(token_url, "https://example.salesforce.com/oauth/token");
                assert_eq!(client_id, "fixture-client-id");
                assert_eq!(client_secret, "fixture-client-secret");
            }
        }
    }

    #[test]
    fn auth_serializes_client_credentials_fields_as_camel_case() {
        let auth = SalesforceAuthConfig::ClientCredentials {
            token_url: "https://example.salesforce.com/oauth/token".to_string(),
            client_id: "fixture-client-id".to_string(),
            client_secret: "fixture-client-secret".to_string(),
        };

        let value = serde_json::to_value(auth).expect("auth serialization should succeed");
        let object = value
            .as_object()
            .expect("serialized auth should be a JSON object");

        assert!(object.contains_key("type"));
        assert!(object.contains_key("tokenUrl"));
        assert!(object.contains_key("clientId"));
        assert!(object.contains_key("clientSecret"));
        assert!(!object.contains_key("token_url"));
        assert!(!object.contains_key("client_id"));
        assert!(!object.contains_key("client_secret"));
    }
}
