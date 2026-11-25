//! Images in requests to an OpenAI-compatible server. Ports `ToolImagesChatClientTests`,
//! `VisionRefusalTests`, `ToolResultImageTests` (Cli.Tests) and the OpenAI wire test of
//! `AttachmentMessageTests`.

use std::sync::{Arc, Mutex};

use base64::Engine;
use orca_abstractions::{ToolResult, ToolResultPart};
use orca_core::attachments::{PromptAttachment, message_builder};
use orca_core::client::tool_images::{
    self, DROPPED_NOTE, IMAGES_INTRO, KEEP_IMAGES, MAX_IMAGES, MAX_TOOL_IMAGE_BYTES, NO_VISION_NOTE,
};
use orca_core::client::{
    ChatError, ChatMessage, ChatOptions, ClientSettings, ContentPart, ImagePart, OpenAiClient,
    Role, ServedModels, StreamEvent, ToolCall,
};
use orca_core::config::OrcaConfig;
use orca_testkit::{FakeOpenAi, Reply, ServerOptions};

const PNG: [u8; 7] = [0x89, 0x50, 0x4E, 0x47, 1, 2, 3];

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn tool_result(call_id: &str, images: usize) -> ChatMessage {
    ChatMessage::tool_result(call_id, "Screenshot of Launcher")
        .with_images((0..images).map(|_| ImagePart::new(PNG.to_vec(), "image/png")))
}

fn assistant_calls(ids: &[(&str, &str)]) -> ChatMessage {
    ChatMessage {
        content: None,
        tool_calls: ids
            .iter()
            .map(|(id, name)| ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: "{}".into(),
            })
            .collect(),
        ..ChatMessage::assistant("")
    }
}

fn conversation(tail: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut v = vec![
        ChatMessage::user("Check the app"),
        assistant_calls(&[("c1", "job"), ("c2", "read")]),
    ];
    v.extend(tail);
    v
}

fn client(url: &str, vision: Option<bool>) -> OpenAiClient {
    let mut settings = ClientSettings::new(url, "k", Some("m".into()));
    settings.vision = vision;
    OpenAiClient::new(settings, Arc::new(ServedModels::new()))
}

// ── ToolImagesChatClientTests ──────────────────────────────────────────

#[test]
fn images_move_to_a_user_message_after_the_whole_run_of_tool_results() {
    let output = tool_images::rewrite(
        &conversation(vec![
            tool_result("c1", 1),
            ChatMessage::tool_result("c2", "text"),
        ]),
        true,
    );

    assert_eq!(
        vec![
            Role::User,
            Role::Assistant,
            Role::Tool,
            Role::Tool,
            Role::User
        ],
        output.iter().map(|m| m.role).collect::<Vec<_>>()
    );
    assert!(!output[2].has_images());
    assert_eq!(Some("Screenshot of Launcher"), output[2].content.as_deref());
    assert!(output[2].parts.is_empty());
    assert_eq!(Some(IMAGES_INTRO), output[4].content.as_deref());
    assert_eq!(PNG.to_vec(), output[4].images().next().unwrap().data);
}

fn shots(count: usize) -> Vec<ChatMessage> {
    let mut turns = vec![ChatMessage::user("go")];
    for i in 0..count {
        turns.push(assistant_calls(&[(&format!("c{i}"), "job")]));
        turns.push(tool_result(&format!("c{i}"), 1));
    }
    turns
}

#[test]
fn only_the_newest_images_are_sent_older_ones_become_a_note() {
    let output = tool_images::rewrite(&shots(9), true);

    // Past MaxImages the oldest drop in one step, down to KeepImages.
    assert_eq!(
        KEEP_IMAGES,
        output.iter().map(|m| m.images().count()).sum::<usize>()
    );
    let notes: Vec<String> = output
        .iter()
        .filter(|m| m.role == Role::Tool)
        .map(|m| m.content.clone().unwrap_or_default())
        .collect();
    assert_eq!(5, notes.iter().filter(|n| n.contains(DROPPED_NOTE)).count());
    assert!(notes[0].contains(DROPPED_NOTE));
    assert!(!notes.last().unwrap().contains(DROPPED_NOTE));
}

#[test]
fn images_are_dropped_in_steps() {
    for (total, skipped) in [
        (0, 0),
        (4, 0),
        (8, 0),
        (9, 5),
        (13, 5),
        (14, 10),
        (18, 10),
        (19, 15),
    ] {
        assert_eq!(skipped, tool_images::images_to_skip(total), "{total}");
        if total > 0 {
            let sent = total - skipped;
            assert!(
                sent >= total.min(KEEP_IMAGES) && sent <= MAX_IMAGES,
                "{total}"
            );
        }
    }
}

#[test]
fn a_new_screenshot_usually_leaves_the_earlier_prompt_unchanged() {
    // orca-local3: with a sliding window every screenshot past the 4th rewrote an old one and
    // the server re-read 23-53k tokens. Now only every 5th one past 8 does.
    fn render(messages: &[ChatMessage]) -> String {
        messages
            .iter()
            .map(|m| {
                let mut s = format!("|{}:", &m.role.as_str()[..1]);
                s.push_str(m.content.as_deref().unwrap_or(""));
                for p in &m.parts {
                    match p {
                        ContentPart::Image(_) => s.push_str("<img>"),
                        ContentPart::Text(t) => s.push_str(t),
                    }
                }
                s
            })
            .collect()
    }
    let mut rewrites = 0;
    for n in 2..=20 {
        let before = render(&tool_images::rewrite(&shots(n - 1), true));
        let after = render(&tool_images::rewrite(&shots(n), true));
        if !after.starts_with(&before) {
            rewrites += 1;
        }
    }
    assert_eq!(3, rewrites); // at the 9th, 14th and 19th image
}

#[test]
fn without_vision_the_model_is_told_there_was_an_image() {
    let output = tool_images::rewrite(&conversation(vec![tool_result("c1", 1)]), false);
    assert_eq!(3, output.len());
    assert!(!output.iter().any(ChatMessage::has_images));
    assert!(
        output[2]
            .content
            .as_deref()
            .unwrap()
            .contains(NO_VISION_NOTE)
    );
}

#[tokio::test]
async fn on_the_wire_the_image_is_an_image_url_data_uri_in_a_user_message() {
    let server = FakeOpenAi::start(vec![Reply::text("A blue tile.")]).await;
    let c = client(&server.base_url(), Some(true));

    let response = c
        .complete(
            "t",
            &conversation(vec![
                tool_result("c1", 1),
                ChatMessage::tool_result("c2", "ok"),
            ]),
            &ChatOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!("A blue tile.", response.text);
    let body = server.requests()[0].body.to_string();
    assert!(body.contains("\"image_url\""));
    assert!(body.contains(&format!("data:image/png;base64,{}", b64(&PNG))));
    assert!(body.find("\"tool_call_id\":\"c2\"").unwrap() < body.find("image_url").unwrap());
    // The exact shape the .NET SDK writes (checked against the C# build in cross-check 108).
    let messages = server.requests()[0].body["messages"].clone();
    assert_eq!(
        serde_json::json!({"role": "user", "content": [
            {"type": "text", "text": IMAGES_INTRO},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", b64(&PNG))}}
        ]}),
        messages[4]
    );
}

#[tokio::test]
async fn vision_comes_from_the_server_once_per_endpoint() {
    let server = FakeOpenAi::start(vec![Reply::text("a"), Reply::text("b")]).await;
    let c = client(&server.base_url(), None);

    c.complete(
        "t",
        &conversation(vec![tool_result("c1", 1)]),
        &ChatOptions::default(),
    )
    .await
    .unwrap();
    let body = server.requests()[0].body.to_string();
    assert!(body.contains(NO_VISION_NOTE));
    assert!(!body.contains("image_url"));
    let probes = server.probes().len();
    assert!(probes > 0);

    // Another request with an image: no second probe. No image in the request: no probe.
    c.complete(
        "t",
        &conversation(vec![tool_result("c1", 1)]),
        &ChatOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(probes, server.probes().len());
}

#[test]
fn parse_vision_reads_strata_llama_cpp_and_lm_studio() {
    for (json, expected) in [
        (
            r#"{"modalities":{"vision":true},"total_slots":3}"#,
            Some(true),
        ),
        (r#"{"modalities":{"vision":false}}"#, Some(false)),
        (
            r#"{"object":"list","data":[{"id":"q","architecture":{"input_modalities":["text","image"]}}]}"#,
            Some(true),
        ),
        (
            r#"{"object":"list","data":[{"id":"q","architecture":{"input_modalities":["text"]}}]}"#,
            Some(false),
        ),
        (
            r#"{"data":[{"id":"a","type":"vlm","state":"not-loaded"},{"id":"b","type":"llm","state":"loaded"}]}"#,
            Some(false),
        ),
        (
            r#"{"data":[{"id":"a","type":"vlm","state":"loaded"}]}"#,
            Some(true),
        ),
        (r#"{"object":"list","data":[{"id":"gpt"}]}"#, None),
        ("not json", None),
    ] {
        assert_eq!(expected, tool_images::parse_vision(json), "{json}");
    }
}

#[tokio::test]
async fn detect_falls_through_to_models_and_sends_the_key() {
    let server = FakeOpenAi::start_with(
        vec![],
        ServerOptions::default()
            .route("GET /props", 404, "{}")
            .route(
                "GET /v1/models",
                200,
                r#"{"data":[{"id":"q","architecture":{"input_modalities":["text","image"]}}]}"#,
            ),
    )
    .await;
    assert!(tool_images::detect(&server.base_url(), Some("secret")).await);
    assert!(
        server
            .probe_auth()
            .iter()
            .all(|a| a.as_deref() == Some("Bearer secret"))
    );
    assert_eq!(vec!["GET /props", "GET /v1/models"], server.probes());

    let empty = FakeOpenAi::start(vec![]).await;
    assert!(!tool_images::detect(&empty.base_url(), None).await);
}

// ── VisionRefusalTests ─────────────────────────────────────────────────

/// ninfer's answer to a request with an image (probe of 2026-11-21).
const NINFER_REFUSAL: &str = r#"{"error":{"code":"vision_disabled","message":"Vision is disabled for this server","param":"messages","type":"invalid_request_error"}}"#;

fn with_screenshot() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("Check the app"),
        assistant_calls(&[("c1", "job")]),
        tool_result("c1", 1),
    ]
}

fn with_attachment() -> Vec<ChatMessage> {
    vec![ChatMessage::user("What is this?").with_images([ImagePart {
        data: PNG.to_vec(),
        media_type: "image/png".into(),
        name: Some("C:\\shots\\a.png".into()),
    }])]
}

async fn ask(
    c: &OpenAiClient,
    messages: &[ChatMessage],
    streaming: bool,
) -> Result<String, ChatError> {
    if streaming {
        let mut text = String::new();
        let r = c
            .stream("t", messages, &ChatOptions::default(), &mut |e| {
                if let StreamEvent::Text(t) = e {
                    text.push_str(&t);
                }
            })
            .await?;
        assert_eq!(text, r.text);
        Ok(r.text)
    } else {
        Ok(c.complete("t", messages, &ChatOptions::default())
            .await?
            .text)
    }
}

#[tokio::test]
async fn a_refused_image_request_goes_again_without_images_and_the_server_is_remembered() {
    let notices = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = notices.clone();
    tool_images::set_on_vision_refused(Some(Arc::new(move |t: &str| {
        sink.lock().unwrap().push(t.to_string())
    })));
    for streaming in [false, true] {
        notices.lock().unwrap().clear();
        let server = FakeOpenAi::start(vec![
            Reply::error(400, NINFER_REFUSAL),
            Reply::text("hi"),
            Reply::text("again"),
        ])
        .await;
        let url = server.base_url();
        let c = client(&url, Some(true));
        assert!(!tool_images::has_refused_images(&url));

        assert_eq!("hi", ask(&c, &with_screenshot(), streaming).await.unwrap());

        let bodies: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.body.to_string())
            .collect();
        assert_eq!(2, bodies.len());
        assert!(bodies[0].contains("\"image_url\""));
        assert!(!bodies[1].contains("\"image_url\""));
        assert!(bodies[1].contains("this model server does not take images")); // the model is told why
        assert!(tool_images::has_refused_images(&format!("{url}/")));

        // From now on nothing is tried with images: one request per turn, also for an
        // attachment, and prompts are built with file paths.
        ask(&c, &with_attachment(), streaming).await.unwrap();
        let bodies: Vec<String> = server
            .requests()
            .iter()
            .map(|r| r.body.to_string())
            .collect();
        assert_eq!(3, bodies.len());
        assert!(!bodies[2].contains("\"image_url\""));
        assert!(bodies[2].contains("a.png"));
        assert!(!tool_images::supports_vision(&url, Some("k"), Some(true)).await);

        let notices = notices.lock().unwrap().clone();
        assert_eq!(1, notices.len(), "{streaming}");
        assert!(notices[0].contains("does not take images"));
        assert!(notices[0].contains("vision_disabled"));
        assert!(notices[0].contains("lmStudio.vision")); // it was forced on in the config
    }
    tool_images::set_on_vision_refused(None);
}

#[tokio::test]
async fn another_bad_request_is_not_taken_for_a_refusal() {
    let server = FakeOpenAi::start(vec![Reply::error(
        400,
        r#"{"error":{"code":"invalid_value","message":"temperature must be between 0 and 2","type":"invalid_request_error"}}"#,
    )])
    .await;
    let url = server.base_url();
    let c = client(&url, Some(true));

    let err = c
        .complete("t", &with_screenshot(), &ChatOptions::default())
        .await
        .unwrap_err();

    assert_eq!(Some(400), err.status());
    assert_eq!(1, server.requests().len());
    assert!(!tool_images::has_refused_images(&url));
}

#[tokio::test]
async fn a_refusal_without_images_in_the_request_is_not_retried() {
    // Nothing to take out: the error is the caller's to see.
    let server = FakeOpenAi::start(vec![Reply::error(400, NINFER_REFUSAL)]).await;
    let url = server.base_url();
    let c = client(&url, Some(true));

    assert!(
        c.complete("t", &[ChatMessage::user("hi")], &ChatOptions::default())
            .await
            .is_err()
    );

    assert_eq!(1, server.requests().len());
    assert!(!tool_images::has_refused_images(&url));
}

#[test]
fn refusal_wordings() {
    for (message, refusal) in [
        (
            "HTTP 400 (invalid_request_error: vision_disabled)\nParameter: messages\n\nVision is disabled for this server",
            true,
        ),
        (
            "image input is not supported - hint: if this is unexpected, you may need to provide the mmproj",
            true,
        ),
        ("This model does not support image input", true),
        (
            "Invalid request: images are not supported by this model",
            true,
        ),
        (
            "the request exceeds the available context size (131072 tokens)",
            false,
        ),
        ("Service unavailable", false),
    ] {
        assert_eq!(
            refusal,
            tool_images::is_vision_refusal_message(message),
            "{message}"
        );
        assert_eq!(
            refusal,
            tool_images::is_vision_refusal(&ChatError::Http {
                status: 400,
                url: "u".into(),
                body: message.into()
            })
        );
    }
    // Only 400 / 415 / 422 count.
    assert!(!tool_images::is_vision_refusal(&ChatError::Http {
        status: 500,
        url: "u".into(),
        body: "vision_disabled".into()
    }));
}

// ── ToolResultImageTests (Cli.Tests) ───────────────────────────────────

fn image_result(mime: &str, data: Option<Vec<u8>>) -> ToolResult {
    ToolResult {
        parts: Some(vec![ToolResultPart::Image {
            data: data.unwrap_or_else(|| PNG.to_vec()),
            mime_type: mime.into(),
        }]),
        ..ToolResult::success("Read image file [image/png]")
    }
}

fn config(provider: Option<&str>, vision: Option<bool>) -> OrcaConfig {
    let mut c = OrcaConfig::default();
    c.lm_studio.provider = provider.map(str::to_string);
    c.lm_studio.vision = vision;
    c
}

#[test]
fn anthropic_gets_the_image() {
    let images = tool_images::tool_result_images(
        &image_result("image/png", None),
        &config(Some("anthropic"), None),
    );
    assert_eq!(1, images.len());
    assert_eq!("image/png", images[0].media_type);
    assert_eq!(PNG.to_vec(), images[0].data);
}

#[test]
fn open_ai_compatible_gets_the_image_unless_vision_is_off() {
    let r = image_result("image/png", None);
    assert_eq!(
        1,
        tool_images::tool_result_images(&r, &config(None, None)).len()
    );
    assert!(tool_images::tool_result_images(&r, &config(None, Some(false))).is_empty());
    assert_eq!(
        1,
        tool_images::tool_result_images(&r, &config(Some("anthropic"), Some(false))).len()
    );
}

#[test]
fn unsupported_type_or_oversized_is_skipped() {
    let c = config(Some("anthropic"), None);
    assert!(tool_images::tool_result_images(&image_result("image/bmp", None), &c).is_empty());
    assert!(
        tool_images::tool_result_images(
            &image_result("image/png", Some(vec![0; MAX_TOOL_IMAGE_BYTES + 1])),
            &c
        )
        .is_empty()
    );
}

#[test]
fn text_only_result_has_no_images() {
    assert!(
        tool_images::tool_result_images(
            &ToolResult::success("text"),
            &config(Some("anthropic"), None)
        )
        .is_empty()
    );
}

// ── AttachmentMessageTests: the OpenAI wire format ─────────────────────

#[tokio::test]
async fn open_ai_compatible_sends_the_image_as_an_image_url_data_uri() {
    let dir = tempfile::tempdir().unwrap();
    let png = orca_core::attachments::clipboard::image_codec::encode_png(1, 1, &[0, 0, 255, 255]);
    let path = dir.path().join("s.png");
    std::fs::write(&path, &png).unwrap();
    let shot = PromptAttachment::for_path(1, path.to_string_lossy());
    let message = message_builder::build(
        &format!("look {}", shot.chip()),
        &[shot],
        true,
        &dir.path().to_string_lossy(),
    )
    .message;
    let server = FakeOpenAi::start(vec![Reply::text("ok")]).await;

    client(&server.base_url(), Some(true))
        .complete("t", &[message], &ChatOptions::default())
        .await
        .unwrap();

    let body = &server.requests()[0].body;
    let parts = &body["messages"][0]["content"];
    assert_eq!("text", parts[0]["type"]);
    assert_eq!("look [image 1: s.png]", parts[0]["text"]);
    assert_eq!("image_url", parts[1]["type"]);
    assert_eq!(
        format!("data:image/png;base64,{}", b64(&png)),
        parts[1]["image_url"]["url"].as_str().unwrap()
    );
}
