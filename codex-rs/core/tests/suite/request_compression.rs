#![cfg(not(target_os = "windows"))]

use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_body_is_zstd_compressed_for_codex_backend_when_enabled() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request_log = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let base_url = format!("{}/backend-api/codex/v1", server.uri());
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config
                .features
                .enable(Feature::EnableRequestCompression)
                .expect("test config should allow feature update");
            config.model_provider.base_url = Some(base_url);
        });
    let codex = builder.build(&server).await?.codex;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "compress me".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    // Wait until the task completes so the request definitely hit the server.
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = request_log.single_request();
    assert_eq!(request.header("content-encoding").as_deref(), Some("zstd"));

    let decompressed = zstd::stream::decode_all(std::io::Cursor::new(request.body_bytes()))?;
    let json: serde_json::Value = serde_json::from_slice(&decompressed)?;
    assert!(
        json.get("input").is_some(),
        "expected request body to decode as Responses API JSON"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_body_is_not_compressed_for_api_key_auth_even_when_enabled() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request_log = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let base_url = format!("{}/backend-api/codex/v1", server.uri());
    let mut builder = test_codex().with_config(move |config| {
        config
            .features
            .enable(Feature::EnableRequestCompression)
            .expect("test config should allow feature update");
        config.model_provider.base_url = Some(base_url);
    });
    let codex = builder.build(&server).await?.codex;

    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "do not compress".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = request_log.single_request();
    assert!(
        request.header("content-encoding").is_none(),
        "did not expect request compression for API-key auth"
    );

    let json: serde_json::Value = serde_json::from_slice(&request.body_bytes())?;
    assert!(
        json.get("input").is_some(),
        "expected request body to be plain Responses API JSON"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_image_continuation_survives_failure_and_resume() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use core_test_support::responses::ev_assistant_message;
    use core_test_support::responses::ev_function_call;
    use core_test_support::responses::received_responses_requests;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path_regex;

    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let image_dir = tempfile::tempdir()?;
    let image_path = image_dir.path().join("reference.png");
    image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 30])).save(&image_path)?;
    let arguments = json!({"path": image_path}).to_string();
    let requests = Arc::new(AtomicUsize::new(0));
    let reject_identity = Arc::new(AtomicBool::new(true));
    let request_count = Arc::clone(&requests);
    let reject = Arc::clone(&reject_identity);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(move |request: &wiremock::Request| {
            if request_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse(vec![
                        ev_response_created("image-call"),
                        ev_function_call("view-once", "view_image", &arguments),
                        ev_completed("image-call"),
                    ]));
            }
            if request.headers.contains_key("content-encoding") || reject.load(Ordering::SeqCst) {
                return ResponseTemplate::new(400).set_body_json(json!({"detail":"Bad Request"}));
            }
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_response_created("recovered"),
                    ev_assistant_message("recovered-message", "Preserved the image and decision"),
                    ev_completed("recovered"),
                ]))
        })
        .mount(&server)
        .await;

    let base_url = format!("{}/backend-api/codex/v1", server.uri());
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config
                .features
                .enable(Feature::EnableRequestCompression)
                .expect("enable request compression");
            config.model_provider.base_url = Some(base_url);
            config.model_provider.supports_websockets = false;
        });
    let test = builder.build(&server).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![
            UserInput::Text {
                text: "Remember decision ALPHA; inspect this image".into(),
                text_elements: vec![],
            },
            UserInput::LocalImage {
                path: image_path,
                detail: None,
            },
        ]))
        .await?;
    let mut views = 0;
    let completed = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            views += usize::from(matches!(event, EventMsg::ViewImageToolCall(_)));
            matches!(event, EventMsg::TurnComplete(_))
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(matches!(completed, EventMsg::TurnComplete(turn) if turn.error.is_some()));
    assert_eq!(views, 1);

    // Recover the original persisted thread, including the completed tool result.
    // The second process must not need to execute view_image again.
    reject_identity.store(false, Ordering::SeqCst);
    let resumed = builder.restart(&server, &test).await?;
    assert_eq!(
        resumed.session_configured.thread_id,
        test.session_configured.thread_id
    );
    resumed
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Continue with decision ALPHA".into(),
            text_elements: vec![],
        }]))
        .await?;
    let completed = wait_for_event_with_timeout(
        &resumed.codex,
        |event| {
            views += usize::from(matches!(event, EventMsg::ViewImageToolCall(_)));
            matches!(event, EventMsg::TurnComplete(_))
        },
        Duration::from_secs(30),
    )
    .await;
    assert!(matches!(completed, EventMsg::TurnComplete(turn) if turn.error.is_none()));
    assert_eq!(views, 1);

    let sent = received_responses_requests(&server).await;
    assert_eq!(sent.len(), 5);
    for pair in [&sent[1..3], &sent[3..5]] {
        assert_eq!(pair[0].header("content-encoding").as_deref(), Some("zstd"));
        assert!(pair[1].header("content-encoding").is_none());
        let decoded = zstd::stream::decode_all(pair[0].body_bytes().as_slice())?;
        assert_eq!(decoded, pair[1].body_bytes());
    }
    let before = sent[2].body_json();
    let after = sent[4].body_json();
    for body in [&before, &after] {
        let input = body["input"]
            .as_array()
            .expect("request input should be an array");
        assert!(body.to_string().contains("Remember decision ALPHA"));
        assert_eq!(
            input
                .iter()
                .filter(
                    |item| item["type"] == "function_call_output" && item["call_id"] == "view-once"
                )
                .count(),
            1
        );
        assert_eq!(
            body.to_string().matches("data:image/png;base64,").count(),
            2
        );
    }
    let original_input = before["input"].as_array().expect("original input");
    let resumed_input = after["input"].as_array().expect("resumed input");
    let original_user = original_input
        .iter()
        .find(|item| {
            item["role"] == "user"
                && item["content"]
                    .to_string()
                    .contains("Remember decision ALPHA")
        })
        .expect("original user message");
    let resumed_user = resumed_input
        .iter()
        .find(|item| {
            item["role"] == "user"
                && item["content"]
                    .to_string()
                    .contains("Remember decision ALPHA")
        })
        .expect("resumed original user message");
    assert_eq!(original_user["content"], resumed_user["content"]);
    let original_output = original_input
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "view-once")
        .expect("original completed tool output");
    let resumed_output = resumed_input
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "view-once")
        .expect("resumed completed tool output");
    assert_eq!(original_output["output"], resumed_output["output"]);
    resumed.codex.shutdown_and_wait().await?;
    Ok(())
}
