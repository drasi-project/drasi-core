use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::auth::SalesforceSessionToken;
use crate::config::SalesforceConnectionConfig;
use crate::projection::{Describe, ObjectPlan, Record};

#[derive(Clone)]
pub struct SalesforceRestClient {
    connection: SalesforceConnectionConfig,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryPage {
    records: Vec<Value>,
    done: bool,
    next_records_url: Option<String>,
}

#[derive(Debug)]
pub struct InvalidField {
    source: reqwest::Error,
}

impl std::fmt::Display for InvalidField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Salesforce rejected the query projection (INVALID_FIELD)")
    }
}

impl std::error::Error for InvalidField {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryError {
    error_code: String,
}

pub(crate) async fn read_json_bounded<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T> {
    serde_json::from_slice(&read_bounded(response, limit).await?)
        .context("failed to decode Salesforce JSON response")
}

async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(
            length <= limit as u64,
            "Salesforce response exceeded byte limit ({limit})"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= limit.saturating_sub(body.len()),
            "Salesforce response exceeded byte limit ({limit})"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub fn is_unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|error| error.status() == Some(reqwest::StatusCode::UNAUTHORIZED))
}

pub fn is_retryable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
        error.is_timeout()
            || error.is_connect()
            || error.is_request()
            || error.is_body()
            || error.status().is_some_and(|status| {
                status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
            })
    })
}

impl SalesforceRestClient {
    pub fn new(connection: SalesforceConnectionConfig) -> Result<Self> {
        Ok(Self {
            connection,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("failed to construct Salesforce REST client")?,
        })
    }

    pub async fn describe_object(
        &self,
        session: &SalesforceSessionToken,
        object: &str,
    ) -> Result<Describe> {
        self.ensure_session_matches_config(session)?;
        let response = self
            .http
            .get(self.api_url(
                &format!("sobjects/{object}/describe"),
                &session.instance_url,
            ))
            .bearer_auth(&session.access_token)
            .send()
            .await?
            .error_for_status()?;

        let describe: Describe = read_json_bounded(response, 8 * 1024 * 1024).await?;
        Ok(describe)
    }

    pub async fn fetch_records_by_ids(
        &self,
        session: &SalesforceSessionToken,
        plan: &ObjectPlan,
        ids: &[String],
        max_response_bytes: usize,
    ) -> Result<Vec<Record>> {
        self.ensure_session_matches_config(session)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        const BATCH_SIZE: usize = 100;
        for chunk in ids.chunks(BATCH_SIZE) {
            for id in chunk {
                plan.validate_id(id)?;
            }
            let quoted = chunk
                .iter()
                .map(|id| format!("'{id}'"))
                .collect::<Vec<_>>()
                .join(", ");
            let soql = format!(
                "SELECT {} FROM {} WHERE Id IN ({quoted})",
                plan.projection(),
                plan.name
            );
            let mut rows = self
                .query_records(session, &soql, max_response_bytes, chunk.len())
                .await?
                .into_iter()
                .map(|row| plan.snapshot(&row))
                .collect::<Result<Vec<_>>>()?;
            results.append(&mut rows);
        }
        Ok(results)
    }

    pub async fn full_scan(
        &self,
        session: &SalesforceSessionToken,
        plan: &ObjectPlan,
        max_rows: usize,
        max_response_bytes: usize,
    ) -> Result<Vec<Record>> {
        self.ensure_session_matches_config(session)?;
        let soql = format!("SELECT {} FROM {}", plan.projection(), plan.name);
        let rows = self
            .query_records(session, &soql, max_response_bytes, max_rows)
            .await?;
        ensure!(
            rows.len() <= max_rows,
            "full scan returned {} rows, exceeds configured limit {}",
            rows.len(),
            max_rows
        );
        rows.into_iter()
            .map(|row| plan.snapshot(&row))
            .collect::<Result<Vec<_>>>()
    }

    async fn query_records(
        &self,
        session: &SalesforceSessionToken,
        soql: &str,
        max_response_bytes: usize,
        max_rows: usize,
    ) -> Result<Vec<Value>> {
        let mut total_bytes = 0usize;
        let mut records = Vec::new();
        let mut next_url = Some(self.api_url("query", &session.instance_url));
        let mut query = Some(vec![("q", soql.to_string())]);

        while let Some(url) = next_url.take() {
            let mut request = self.http.get(url).bearer_auth(&session.access_token);
            if let Some(query_params) = query.take() {
                request = request.query(&query_params);
            }

            let response = request.send().await?;
            if let Err(error) = response.error_for_status_ref() {
                if response.status() == reqwest::StatusCode::BAD_REQUEST {
                    let errors: Vec<QueryError> = read_json_bounded(response, 64 * 1024)
                        .await
                        .context("failed to read Salesforce query error response")?;
                    if errors
                        .iter()
                        .any(|error| error.error_code == "INVALID_FIELD")
                    {
                        return Err(InvalidField { source: error }.into());
                    }
                }
                return Err(error.into());
            }
            let body = read_bounded(response, max_response_bytes - total_bytes).await?;
            total_bytes = total_bytes.saturating_add(body.len());
            ensure!(
                total_bytes <= max_response_bytes,
                "Salesforce query response exceeded configured byte limit ({max_response_bytes})"
            );

            let page: QueryPage = serde_json::from_slice(&body)
                .context("failed to parse Salesforce query page JSON")?;
            ensure!(
                page.records.len() <= max_rows.saturating_sub(records.len()),
                "Salesforce query exceeded configured row limit ({max_rows})"
            );
            records.extend(page.records);

            if page.done {
                return Ok(records);
            }

            let continuation = page
                .next_records_url
                .context("Salesforce query page missing nextRecordsUrl")?;
            ensure!(
                continuation.starts_with("/services/data/") && !continuation.contains(".."),
                "Salesforce query page returned an unsafe continuation path"
            );
            next_url = Some(format!("{}{}", session.instance_url, continuation));
            query = None;
        }

        Ok(records)
    }

    fn api_url(&self, path: &str, instance_url: &str) -> String {
        format!(
            "{}/services/data/v{}/{}",
            instance_url.trim_end_matches('/'),
            self.connection.api_version.trim_start_matches('v'),
            path.trim_start_matches('/')
        )
    }

    fn ensure_session_matches_config(&self, session: &SalesforceSessionToken) -> Result<()> {
        ensure!(
            session.instance_url == self.connection.instance_url,
            "session token instance URL does not match configured Salesforce instance"
        );
        ensure!(
            session.org_id == self.connection.expected_org_id,
            "session token org_id does not match configured expected_org_id"
        );
        Ok(())
    }
}
