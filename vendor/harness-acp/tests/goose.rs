#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Real pinned Goose integration using only a loopback mock model provider.
use axum::{Json, extract::State, http::header, routing::post};
use harness_acp::{Completion, LaunchSpec, Task, run};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

async fn model(
    State(state): State<Mock>,
    Json(body): Json<Value>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    state.seen.lock().unwrap().push(body.clone());
    let structured = body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "final_output")
    });
    let delta = if structured {
        let call = state
            .calls
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| json!({"name":"final_output","arguments":"{\"ok\":true}"}));
        json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("result-{}",state.seen.lock().unwrap().len()),"type":"function","function":call}]})
    } else {
        json!({"role":"assistant","content":"Task"})
    };
    let chunk = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let end = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"test","choices":[{"index":0,"delta":{},"finish_reason":if structured{"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("data: {chunk}\n\ndata: {end}\n\ndata: [DONE]\n\n"),
    )
}

#[tokio::test]
#[ignore = "requires GOOSE_TEST_BINARY pointing at pinned Goose executable; no external provider"]
async fn pinned_goose_accepts_schema_and_has_no_ambient_tools() {
    probe(false, false).await;
}

#[tokio::test]
#[ignore = "requires pinned Goose; local mock provider only"]
async fn pinned_goose_custom_provider_and_restricted_mcp() {
    probe(true, false).await;
}

#[tokio::test]
#[ignore = "requires pinned Goose; local mock provider only"]
async fn pinned_goose_enforces_task_limit_over_inherited_environment() {
    probe(true, true).await;
}

#[derive(Clone)]
struct Mock {
    seen: Arc<Mutex<Vec<Value>>>,
    calls: Arc<Mutex<std::collections::VecDeque<Value>>>,
}

async fn probe(custom: bool, limited: bool) {
    let executable = std::env::var_os("GOOSE_TEST_BINARY").expect("GOOSE_TEST_BINARY");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(vec![]));
    let router = axum::Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(Mock{seen:seen.clone(),calls:Arc::new(Mutex::new(if limited {std::collections::VecDeque::from([json!({"name":"inspection__inspect","arguments":"{\"path\":\"source.rs\"}"})])}else if custom {std::collections::VecDeque::from([json!({"name":"developer__shell","arguments":"{\"command\":\"touch FORBIDDEN_EFFECT\"}"}),json!({"name":"load_skill","arguments":"{\"name\":\"/etc/passwd\"}"}),json!({"name":"inspection__inspect","arguments":"{\"path\":\"source.rs\"}"})])}else{Default::default()}))});
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut spec = LaunchSpec::new(executable);
    spec.env
        .insert("OPENAI_API_KEY".into(), "test-provider-secret".into());
    spec.env
        .insert("OPENAI_BASE_URL".into(), format!("http://{addr}/v1").into());
    // Supplied environment cannot weaken the task limit.
    spec.env.insert("GOOSE_MAX_TURNS".into(), "100".into());
    let cwd = tempfile::tempdir().unwrap();
    let handler = Arc::new(Inspection(std::sync::atomic::AtomicUsize::new(0)));
    let bridge = harness_acp::mcp::McpBridge::start(handler.clone())
        .await
        .unwrap();
    if custom {
        spec.provider_definitions.push(json!({"name":"custom_mock","engine":"openai","display_name":"Mock","base_url":format!("http://{addr}"),"base_path":"v1/chat/completions","api_key_env":"OPENAI_API_KEY","models":[{"name":"test","context_limit":8192}],"supports_streaming":true,"dynamic_models":false}));
    }
    let task = Task {
        prompt: "Submit ok true using final_output".into(),
        instructions: "Return a structured result.".into(),
        provider: if custom { "custom_mock" } else { "openai" }.into(),
        model: "test".into(),
        cwd: cwd.path().to_owned(),
        output_schema: Some(
            json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}},"additionalProperties":false}),
        ),
        extensions: if custom {
            vec![bridge.extension("inspection")]
        } else {
            vec![]
        },
        timeout: Duration::from_secs(30),
        max_turns: if limited || !custom { 1 } else { 8 },
    };
    let result = if !limited {
        let mut session = harness_acp::Session::connect(&spec, &task, CancellationToken::new())
            .await
            .unwrap();
        let result = session
            .prompt(&task.prompt, CancellationToken::new(), None)
            .await;
        assert_eq!(result.status, Completion::Completed, "{result:#?}");
        let followup = session
            .prompt(
                "Second turn: retain the first inspection",
                CancellationToken::new(),
                None,
            )
            .await;
        assert_eq!(followup.status, Completion::Completed, "{followup:#?}");
        session.close().await;
        assert_eq!(
            session
                .prompt("must not replay", CancellationToken::new(), None)
                .await
                .status,
            Completion::ProtocolError
        );
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .any(|body| body.to_string().contains("Second turn")
                    && (!custom || body.to_string().contains("inspected source.rs")))
        );
        result
    } else {
        run(&spec, &task, CancellationToken::new()).await
    };
    let structured_calls = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|body| {
            body["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|t| t["function"]["name"] == "final_output")
            })
        })
        .count();
    if limited {
        assert_eq!(result.status, Completion::TurnLimit, "{result:#?}");
        assert!(
            result.output.is_none(),
            "must not accept output after exhaustion"
        );
        assert_eq!(structured_calls, 1, "limit must stop the next inference");
        assert_eq!(handler.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            !result.transcript.is_empty(),
            "keep partial inspection evidence"
        );
        server.abort();
        return;
    }
    assert_eq!(result.status, Completion::Completed, "{result:#?}");
    assert_eq!(result.output, Some(json!({"ok":true})));
    // A completed final_output must terminate inference. Custom mode has three
    // inspection attempts, one final output, then one follow-up final output. The built-in provider completes two
    // prompts with max_turns=1, proving the limit resets per prompt.
    assert_eq!(structured_calls, if custom { 5 } else { 2 });
    let requests = seen.lock().unwrap();
    assert!(!requests.is_empty());
    for request in requests.iter() {
        if let Some(tools) = request["tools"].as_array() {
            for tool in tools {
                let name = tool["function"]["name"].as_str().unwrap();
                assert!(
                    matches!(name, "final_output" | "load_skill")
                        || (custom && name == "inspection__inspect"),
                    "unexpected tool: {tool}"
                );
            }
        }
    }
    assert!(!cwd.path().join("FORBIDDEN_EFFECT").exists());
    assert_eq!(
        handler.0.load(std::sync::atomic::Ordering::SeqCst),
        usize::from(custom)
    );
    if custom {
        let transcript = serde_json::to_string(&result.transcript).unwrap();
        assert!(transcript.contains("failed"));
        assert!(!transcript.contains("root:x:"));
    }
    server.abort();
}

struct Inspection(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl harness_acp::mcp::ToolHandler for Inspection {
    fn tools(&self) -> Vec<rmcp::model::Tool> {
        vec![rmcp::model::Tool::new(
            "inspect",
            "Read staged source",
            json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}})
                .as_object()
                .unwrap()
                .clone(),
        )]
    }
    async fn call(
        &self,
        name: &str,
        _: Option<rmcp::model::JsonObject>,
        _: CancellationToken,
    ) -> rmcp::model::CallToolResult {
        assert_eq!(name, "inspect");
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            "inspected source.rs",
        )])
    }
}
