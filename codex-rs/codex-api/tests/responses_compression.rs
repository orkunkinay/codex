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

#[derive(Clone, Default)]
struct RejectCompressed {
    requests: Arc<Mutex<Vec<Request>>>,
}

impl HttpTransport for RejectCompressed {
    async fn execute(&self, _: Request) -> Result<Response, TransportError> {
        panic!("only streaming requests are expected");
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let compressed = request.headers.contains_key(http::header::CONTENT_ENCODING);
        self.requests.lock().unwrap().push(request);
        if compressed {
            return Err(TransportError::Http {
                status: StatusCode::BAD_REQUEST,
                url: None,
                headers: None,
                body: Some(r#"{"detail":"Bad Request"}"#.into()),
                retry_after: None,
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
    let requests = transport.requests.lock().unwrap();
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
