// Copyright 2026 The Drasi Authors.
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

use std::sync::Arc;

use azure_core::{ClientOptions, Context, Policy, PolicyResult, Request, TransportOptions};

/// Legacy Azure SDK request/retry debug output contains live authorization
/// headers (RUSTSEC-2026-0275). Replace only the logging transport policy and
/// redact the signed request before control returns to retry logging. The SDK
/// still owns authentication, timeouts, retries and the HTTP client.
pub(crate) fn client_options() -> ClientOptions {
    client_options_with_transport(TransportOptions::default())
}

fn client_options_with_transport(transport: TransportOptions) -> ClientOptions {
    ClientOptions::new(transport.clone()).per_call_policies(vec![
        Arc::new(CredentialLoggingPolicy {
            transport: Arc::new(RedactingTransport(transport)),
        }) as Arc<dyn Policy>,
    ])
}

#[derive(Debug)]
struct CredentialLoggingPolicy {
    transport: Arc<dyn Policy>,
}

#[async_trait::async_trait]
impl Policy for CredentialLoggingPolicy {
    async fn send(
        &self,
        context: &Context,
        request: &mut Request,
        next: &[Arc<dyn Policy>],
    ) -> PolicyResult {
        let mut next = next.to_vec();
        let last = next.last_mut().ok_or_else(|| {
            azure_core::Error::message(
                azure_core::error::ErrorKind::Other,
                "Azure request pipeline has no transport",
            )
        })?;
        *last = self.transport.clone();
        next[0].send(context, request, &next[1..]).await
    }
}

#[derive(Debug)]
struct RedactingTransport(TransportOptions);

#[async_trait::async_trait]
impl Policy for RedactingTransport {
    async fn send(
        &self,
        context: &Context,
        request: &mut Request,
        _next: &[Arc<dyn Policy>],
    ) -> PolicyResult {
        let request = SignedRequest(request);
        let response = self.0.send(context, request.0).await?;
        let xml_error = !response.status().is_success()
            && response
                .headers()
                .get_optional_str(&azure_core::headers::CONTENT_TYPE)
                == Some("application/xml");
        let queue_reply =
            response.status().is_success() && request.0.url().path().ends_with("/messages");
        if xml_error || queue_reply {
            let (status, headers, body) = response.deconstruct();
            Ok(azure_core::Response::new(
                status,
                headers,
                Box::pin(futures::stream::once(async move {
                    let body = body.collect().await?;
                    validate_service_xml(&body)?;
                    Ok(body)
                })),
            ))
        } else {
            Ok(response)
        }
    }
}

fn validate_service_xml(body: &[u8]) -> azure_core::Result<()> {
    use quick_xml::events::Event;

    // Queue replies and storage errors have fixed service schemas, not
    // arbitrary user XML. Bound attributes before the legacy serde decoder's
    // quadratic duplicate checks (RUSTSEC-2026-0194).
    let mut reader = quick_xml::Reader::from_reader(body);
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element)) => {
                for (index, attribute) in element.attributes().enumerate() {
                    if index >= 256 {
                        return Err(azure_core::Error::message(
                            azure_core::error::ErrorKind::DataConversion,
                            "Azure service XML exceeds 256 attributes per element",
                        ));
                    }
                    attribute.map_err(|error| {
                        azure_core::Error::new(azure_core::error::ErrorKind::DataConversion, error)
                    })?;
                }
            }
            Ok(Event::Eof) => return Ok(()),
            Ok(_) => {}
            Err(error) => {
                return Err(azure_core::Error::new(
                    azure_core::error::ErrorKind::DataConversion,
                    error,
                ));
            }
        }
    }
}

struct SignedRequest<'a>(&'a mut Request);

impl Drop for SignedRequest<'_> {
    fn drop(&mut self) {
        // Also redact on transport cancellation. Authorization is regenerated
        // by the SDK before every retry; it is not part of SharedKey's input.
        if self
            .0
            .headers()
            .get_optional_str(&azure_core::headers::AUTHORIZATION)
            .is_some()
        {
            self.0
                .insert_header(azure_core::headers::AUTHORIZATION, "<redacted>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::Mutex;

    use azure_core::{
        headers::Headers, FixedRetryOptions, HttpClient, Method, Response, RetryOptions,
        StatusCode, TransportOptions,
    };
    use azure_storage::{clients::ServiceType, StorageCredentials};
    use tracing::{instrument::WithSubscriber, Level};

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct Transport {
        authorizations: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl HttpClient for Transport {
        async fn execute_request(&self, request: &Request) -> azure_core::Result<Response> {
            tokio::task::yield_now().await;
            let retry = {
                let mut authorizations = self.authorizations.lock().unwrap();
                authorizations.push(
                    request
                        .headers()
                        .get_str(&azure_core::headers::AUTHORIZATION)?
                        .to_owned(),
                );
                authorizations.len() % 2 == 1
            };
            tracing::debug!(target: "drasi_test_transport", "transport debug remains visible");
            tracing::warn!(target: "azure_core::test", "azure warning remains visible");
            Ok(Response::new(
                if retry {
                    StatusCode::ServiceUnavailable
                } else {
                    StatusCode::Ok
                },
                Headers::new(),
                Box::pin(futures::stream::empty()),
            ))
        }
    }

    #[tokio::test]
    async fn signed_requests_never_log_credentials_at_trace_level() {
        let output = Output::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let transport = Arc::new(Transport::default());
        let captured_transport = transport.clone();
        async {
            for service in [ServiceType::Blob, ServiceType::Queue, ServiceType::Table] {
                let options = client_options_with_transport(TransportOptions::new(
                    transport.clone(),
                ))
                .retry(RetryOptions::fixed(
                    FixedRetryOptions::default().delay(std::time::Duration::ZERO),
                ));
                let credentials = StorageCredentials::access_key(
                    "testaccount",
                    // Public test material, not an account credential.
                    "dGVzdC1vbmx5LWtleQ==",
                );
                let pipeline =
                    azure_storage::clients::new_pipeline_from_options(options, credentials);
                let mut request = Request::new(
                    "https://testaccount.blob.core.windows.net/items"
                        .parse()
                        .unwrap(),
                    Method::Put,
                );
                request.insert_header(
                    azure_core::headers::MS_DATE,
                    "Thu, 29 Oct 2026 00:00:00 GMT",
                );
                let mut context = Context::new();
                context.insert(service);
                pipeline.send(&context, &mut request).await.unwrap();
            }
            tracing::debug!(target: "azure_core::test", "outside request remains visible");
        }
        .with_subscriber(subscriber)
        .await;
        let logs = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let authorizations = captured_transport.authorizations.lock().unwrap();
        assert_eq!(authorizations.len(), 6);
        for authorization in authorizations.iter() {
            assert!(authorization.starts_with("SharedKey testaccount:"));
            assert!(!logs.contains(authorization));
        }
        for retry in authorizations.chunks_exact(2) {
            assert_eq!(
                retry[0], retry[1],
                "redaction must not change retry signing"
            );
        }
        assert!(!logs.contains("dGVzdC1vbmx5LWtleQ=="));
        assert!(!logs.contains("the following request"));
        assert!(logs.contains("Successful response. Request="));
        assert!(logs.contains("<redacted>"));
        assert!(logs.contains("transport debug remains visible"));
        assert!(logs.contains("azure warning remains visible"));
        assert!(logs.contains("outside request remains visible"));
    }

    #[tokio::test]
    async fn cancellation_redacts_the_request_before_unwinding_the_pipeline() {
        use futures::FutureExt;

        let transport = RedactingTransport(TransportOptions::new(Arc::new(Transport::default())));
        let mut request = Request::new("https://example.test/items".parse().unwrap(), Method::Put);
        request.insert_header(azure_core::headers::AUTHORIZATION, "test-authorization");
        assert!(transport
            .send(&Context::new(), &mut request, &[])
            .now_or_never()
            .is_none());
        assert_eq!(
            request
                .headers()
                .get_str(&azure_core::headers::AUTHORIZATION)
                .unwrap(),
            "<redacted>"
        );
    }

    #[test]
    fn legacy_xml_deserialization_has_a_bounded_attribute_preflight() {
        let xml = |count: usize| {
            format!(
                "<QueueMessagesList {} />",
                (0..count)
                    .map(|i| format!("a{i}=\"v\""))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        assert!(validate_service_xml(xml(256).as_bytes()).is_ok());
        assert!(validate_service_xml(xml(257).as_bytes()).is_err());
        assert!(validate_service_xml(b"<Error><Code>NotFound</Code></Error>").is_ok());
        assert!(validate_service_xml(b"<Error a='one' a='two'/>").is_err());
    }
}
