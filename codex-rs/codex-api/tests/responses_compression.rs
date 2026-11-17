#![allow(clippy::expect_used)]

//! Fault injection for a receiver that rejects valid compressed conversation replay.
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use codex_api::AuthError;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponsesClient;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

#[derive(Clone)]
struct RejectCompressed {
    requests: Arc<Mutex<Vec<Request>>>,
    status: StatusCode,
    error_body: &'static str,
    reject_identity: bool,
    disconnect: bool,
    fail_stream: bool,
}

impl Default for RejectCompressed {
    fn default() -> Self {
        Self {
            requests: Arc::default(),
            status: StatusCode::BAD_REQUEST,
            error_body: r#"{"detail":"Bad Request"}"#,
            reject_identity: false,
            disconnect: false,
            fail_stream: false,
        }
    }
}

impl HttpTransport for RejectCompressed {
    async fn execute(&self, _: Request) -> Result<Response, TransportError> {
        panic!("only streaming requests are expected");
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let compressed = request.headers.contains_key(http::header::CONTENT_ENCODING);
        self.requests
            .lock()
            .expect("test mutex should not be poisoned")
            .push(request);
        if self.disconnect {
            return Err(TransportError::Network("connection lost".into()));
        }
        if !self.fail_stream && (compressed || self.reject_identity) {
            return Err(TransportError::Http {
                status: self.status,
                url: None,
                headers: Some(HeaderMap::from_iter([(
                    http::HeaderName::from_static("x-request-id"),
                    http::HeaderValue::from_static("rejection-id"),
                )])),
                body: Some(self.error_body.into()),
                retry_after: None,
            });
        }
        if self.fail_stream {
            return Ok(StreamResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                bytes: Box::pin(futures::stream::iter([Err(TransportError::Network(
                    "stream interrupted after acceptance".into(),
                ))])),
            });
        }
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(futures::stream::iter([Ok(Bytes::from_static(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"recovered\",\"output\":[]}}\n\n",
            ))])),
        })
    }
}

struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) -> Result<(), AuthError> {
        Ok(())
    }
}

fn provider() -> Provider {
    Provider {
        name: "test".into(),
        base_url: "https://example.invalid".into(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: codex_api::RetryConfig {
            max_attempts: 0,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(1),
    }
}

fn history() -> Value {
    // A valid one-pixel PNG is retained both as an attachment and as a tool result.
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    json!({
        "model": "test", "stream": true, "store": false,
        "input": [
            {"role":"user","content":[
                {"type":"input_text","text":"Keep decision ALPHA and this image"},
                {"type":"input_image","image_url":image}
            ]},
            {"type":"function_call","name":"view_image","call_id":"view-1","arguments":"{}"},
            {"type":"function_call_output","call_id":"view-1","output":[
                {"type":"input_text","text":"Previously completed tool evidence BETA"},
                {"type":"input_image","image_url":image}
            ]},
            {"role":"user","content":"Continue"}
        ]
    })
}

#[tokio::test]
async fn compressed_replay_rejection_preserves_exact_history() -> anyhow::Result<()> {
    let transport = RejectCompressed::default();
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
    let history = history();
    let expected = serde_json::to_vec(&history)?;
    let mut stream = client
        .stream(history, HeaderMap::new(), Compression::Zstd, None)
        .await?;
    let mut completed = 0;
    while let Some(event) = stream.next().await {
        if matches!(event?, ResponseEvent::Completed { .. }) {
            completed += 1;
        }
    }
    assert_eq!(completed, 1);
    let requests = transport
        .requests
        .lock()
        .expect("test mutex should not be poisoned");
    assert_eq!(requests.len(), 2);
    let compressed = requests[0]
        .prepare_body_for_send()
        .map_err(anyhow::Error::msg)?
        .body_bytes();
    assert_eq!(zstd::stream::decode_all(compressed.as_ref())?, expected);
    assert!(
        !requests[1]
            .headers
            .contains_key(http::header::CONTENT_ENCODING)
    );
    assert_eq!(
        requests[1]
            .prepare_body_for_send()
            .map_err(anyhow::Error::msg)?
            .body_bytes()
            .as_ref(),
        expected
    );
    Ok(())
}

#[tokio::test]
async fn encoding_rejections_retry_once_but_detailed_errors_do_not() {
    for (status, body, attempts) in [
        (StatusCode::BAD_REQUEST, r#"{ "detail": "Bad Request" }"#, 2),
        (StatusCode::PAYLOAD_TOO_LARGE, "too large", 2),
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported encoding",
            2,
        ),
        (
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"invalid_image","param":"input[0]"}}"#,
            1,
        ),
        (
            StatusCode::BAD_REQUEST,
            r#"{"detail":"Bad Request","error":{"code":"content_policy_violation"}}"#,
            1,
        ),
        (StatusCode::BAD_REQUEST, "invalid zstd or truncated JSON", 1),
        (StatusCode::UNAUTHORIZED, r#"{"detail":"Bad Request"}"#, 1),
        (StatusCode::FORBIDDEN, r#"{"detail":"Bad Request"}"#, 1),
        (StatusCode::TOO_MANY_REQUESTS, "rate limited", 1),
        (StatusCode::INTERNAL_SERVER_ERROR, "server error", 1),
    ] {
        let transport = RejectCompressed {
            status,
            error_body: body,
            reject_identity: true,
            ..Default::default()
        };
        let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
        let result = client
            .stream(history(), HeaderMap::new(), Compression::Zstd, None)
            .await;
        // Both representations are rejected. The final error must retain the
        // provider's status, diagnostic body, and correlation headers.
        let Err(codex_api::ApiError::Transport(TransportError::Http {
            status: actual_status,
            body: actual_body,
            headers,
            ..
        })) = result
        else {
            panic!("expected a terminal HTTP error for {status}");
        };
        assert_eq!(actual_status, status);
        assert_eq!(actual_body.as_deref(), Some(body));
        assert_eq!(
            headers.expect("error should retain headers")["x-request-id"],
            "rejection-id"
        );
        assert_eq!(
            transport
                .requests
                .lock()
                .expect("test mutex should not be poisoned")
                .len(),
            attempts
        );
    }
}

#[tokio::test]
async fn uncompressed_rejection_does_not_retry() {
    let transport = RejectCompressed {
        reject_identity: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
    assert!(
        client
            .stream(history(), HeaderMap::new(), Compression::None, None)
            .await
            .is_err()
    );
    assert_eq!(
        transport
            .requests
            .lock()
            .expect("test mutex should not be poisoned")
            .len(),
        1
    );
}

#[tokio::test]
async fn ambiguous_disconnect_does_not_change_encoding() {
    let transport = RejectCompressed {
        disconnect: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
    assert!(
        client
            .stream(history(), HeaderMap::new(), Compression::Zstd, None)
            .await
            .is_err()
    );
    assert_eq!(
        transport
            .requests
            .lock()
            .expect("test mutex should not be poisoned")
            .len(),
        1
    );
}

#[tokio::test]
async fn accepted_stream_failure_does_not_replay_request() -> anyhow::Result<()> {
    let transport = RejectCompressed {
        fail_stream: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
    let mut stream = client
        .stream(history(), HeaderMap::new(), Compression::Zstd, None)
        .await?;
    let mut errors = 0;
    while let Some(event) = stream.next().await {
        errors += usize::from(event.is_err());
    }
    assert_eq!(errors, 1);
    assert_eq!(
        transport
            .requests
            .lock()
            .expect("test mutex should not be poisoned")
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn compression_build_failure_does_not_dispatch_or_retry() {
    let transport = RejectCompressed::default();
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
    let headers = HeaderMap::from_iter([(
        http::header::CONTENT_ENCODING,
        http::HeaderValue::from_static("gzip"),
    )]);
    let result = client
        .stream(history(), headers, Compression::Zstd, None)
        .await;
    assert!(matches!(
        result,
        Err(codex_api::ApiError::Transport(TransportError::Build(_)))
    ));
    assert!(
        transport
            .requests
            .lock()
            .expect("test mutex should not be poisoned")
            .is_empty()
    );
}

#[tokio::test]
async fn recovery_preserves_large_repetitive_and_compacted_payloads() -> anyhow::Result<()> {
    // Vary the payload across the reported 128 KiB boundary as well as larger
    // histories. These are receiver fault-injection cases, not backend limit claims.
    for size in [0, 131_071, 131_072, 131_073, 140_000, 2_000_000] {
        let mut body = history();
        body["input"]
            .as_array_mut()
            .expect("fixture input should be an array")
            .insert(
                0,
                json!({
                    "type":"compaction", "encrypted_content":"opaque-checkpoint"
                }),
            );
        body["instructions"] = json!("a".repeat(size));
        let expected = serde_json::to_vec(&body)?;
        let transport = RejectCompressed::default();
        let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth));
        let _stream = client
            .stream(body, HeaderMap::new(), Compression::Zstd, None)
            .await?;
        let requests = transport
            .requests
            .lock()
            .expect("test mutex should not be poisoned");
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let prepared = request
                .prepare_body_for_send()
                .map_err(anyhow::Error::msg)?;
            let actual = if prepared
                .headers
                .contains_key(http::header::CONTENT_ENCODING)
            {
                zstd::stream::decode_all(prepared.body_bytes().as_ref())?
            } else {
                prepared.body_bytes().to_vec()
            };
            assert_eq!(actual, expected);
        }
    }
    Ok(())
}

#[tokio::test]
async fn recovery_crosses_the_real_http_error_boundary() -> anyhow::Result<()> {
    use codex_client::ReqwestTransport;
    use codex_http_client::HttpClientBuilder;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            if request.headers.contains_key("content-encoding") {
                ResponseTemplate::new(400).set_body_json(json!({"detail":"Bad Request"}))
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"recovered\",\"output\":[]}}\n\n")
            }
        })
        .expect(2)
        .mount(&server).await;
    let mut provider = provider();
    provider.base_url = server.uri();
    let client = ResponsesClient::new(
        ReqwestTransport::from_http_client(HttpClientBuilder::new().build_direct()?),
        provider,
        Arc::new(NoAuth),
    );
    let mut stream = client
        .stream(history(), HeaderMap::new(), Compression::Zstd, None)
        .await?;
    let mut completed = 0;
    while let Some(event) = stream.next().await {
        completed += usize::from(matches!(event?, ResponseEvent::Completed { .. }));
    }
    assert_eq!(completed, 1);
    let requests = server
        .received_requests()
        .await
        .expect("requests should be recorded");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        zstd::stream::decode_all(requests[0].body.as_slice())?,
        requests[1].body
    );
    assert_eq!(requests[0].url, requests[1].url);
    Ok(())
}

#[derive(Default)]
struct SigningAuth {
    signed: Mutex<Vec<Vec<u8>>>,
}

impl AuthProvider for SigningAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) -> Result<(), AuthError> {
        panic!("body signing must use apply_auth");
    }

    fn apply_auth(&self, mut request: Request) -> codex_api::AuthProviderFuture<'_> {
        Box::pin(async move {
            let bytes = request
                .prepare_body_for_send()
                .map_err(AuthError::Build)?
                .body_bytes();
            self.signed
                .lock()
                .expect("test mutex should not be poisoned")
                .push(bytes.to_vec());
            // A stand-in for a body-dependent signature; authentication must see
            // each prepared representation, rather than reusing the first signature.
            request.headers.insert(
                "x-test-signed-length",
                bytes
                    .len()
                    .to_string()
                    .parse()
                    .expect("length should be a valid header"),
            );
            Ok(request)
        })
    }
}

#[tokio::test]
async fn fallback_is_reauthenticated_with_final_wire_bytes() -> anyhow::Result<()> {
    let transport = RejectCompressed::default();
    let auth = Arc::new(SigningAuth::default());
    let client = ResponsesClient::new(transport.clone(), provider(), auth.clone());
    let headers = HeaderMap::from_iter([
        (
            http::HeaderName::from_static("x-client-request-id"),
            http::HeaderValue::from_static("same-thread"),
        ),
        (
            http::HeaderName::from_static("x-codex-turn-state"),
            http::HeaderValue::from_static("same-route"),
        ),
    ]);
    let _stream = client
        .stream(history(), headers, Compression::Zstd, None)
        .await?;
    let requests = transport
        .requests
        .lock()
        .expect("test mutex should not be poisoned");
    let signed = auth
        .signed
        .lock()
        .expect("test mutex should not be poisoned");
    assert_eq!(signed.len(), 2);
    for (request, signed_bytes) in requests.iter().zip(signed.iter()) {
        let prepared = request
            .prepare_body_for_send()
            .map_err(anyhow::Error::msg)?;
        assert_eq!(prepared.body_bytes().as_ref(), signed_bytes);
        assert_eq!(
            request.headers["x-test-signed-length"],
            signed_bytes.len().to_string()
        );
        assert_eq!(request.headers["x-client-request-id"], "same-thread");
        assert_eq!(request.headers["x-codex-turn-state"], "same-route");
    }
    assert_ne!(signed[0], signed[1]);
    Ok(())
}
