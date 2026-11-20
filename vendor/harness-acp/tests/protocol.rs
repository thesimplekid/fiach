#![allow(clippy::unwrap_used, clippy::expect_used)]
use async_trait::async_trait;
use harness_acp::*;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio_util::sync::CancellationToken;

struct Fake {
    stream: Mutex<Option<DuplexStream>>,
    stopped: Arc<AtomicBool>,
}
struct Owner(Arc<AtomicBool>);
impl Drop for Owner {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[async_trait]
impl Transport for Owner {
    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl Launcher for Fake {
    async fn launch(&self, _: &LaunchSpec, _: &Task) -> std::io::Result<Connection> {
        let stream = self.stream.lock().unwrap().take().unwrap();
        let (reader, writer) = tokio::io::split(stream);
        Ok(Connection {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
            transport: Box::new(Owner(self.stopped.clone())),
        })
    }
}
fn task() -> Task {
    Task {
        prompt: "inspect".into(),
        instructions: "trusted policy".into(),
        provider: "openai".into(),
        model: "test".into(),
        cwd: std::env::current_dir().unwrap(),
        output_schema: Some(
            json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}},"additionalProperties":false}),
        ),
        extensions: vec![],
        timeout: Duration::from_secs(3),
        max_turns: 5,
    }
}
async fn send(writer: &mut tokio::io::WriteHalf<DuplexStream>, v: Value) {
    writer.write_all(format!("{v}\n").as_bytes()).await.unwrap();
}
async fn scripted(mode: &str, server: DuplexStream) {
    let (read, mut write) = tokio::io::split(server);
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
        let req: Value = serde_json::from_str(&line).unwrap();
        let result = match req["method"].as_str() {
            Some("initialize") => json!({"protocolVersion":1,"agentCapabilities":{}}),
            Some("_goose/unstable/recipes/encode") => {
                assert_eq!(req["params"]["recipe"]["extensions"], json!([]));
                json!({"deeplink":"recipe"})
            }
            Some("session/new") => json!({"sessionId":"s"}),
            Some("session/prompt") => {
                if mode == "fragment" {
                    let frame = format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"fragmented evidence"}}}})
                    );
                    let half = frame.len() / 2;
                    write.write_all(&frame.as_bytes()[..half]).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    write.write_all(&frame.as_bytes()[half..]).await.unwrap();
                    send(
                        &mut write,
                        json!({"jsonrpc":"2.0","id":req["id"],"result":{"stopReason":"cancelled"}}),
                    )
                    .await;
                    continue;
                }
                if mode == "capabilities" {
                    send(&mut write,json!({"jsonrpc":"2.0","id":"permission","method":"session/request_permission","params":{}})).await;
                    let response: Value =
                        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                    assert_eq!(response["result"]["outcome"]["outcome"], "cancelled");
                    send(&mut write,json!({"jsonrpc":"2.0","id":"read","method":"fs/read_text_file","params":{"path":"/etc/passwd"}})).await;
                    let response: Value =
                        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                    assert_eq!(response["error"]["code"], -32601);
                }
                if mode == "oversize" {
                    write.write_all(&vec![b'x'; 500]).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    return;
                }
                send(&mut write,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"secret reasoning"}}}})).await;
                send(&mut write,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"progress SECRET_TOKEN_VALUE"}}}})).await;
                if mode == "cancel" {
                    continue;
                }
                if mode == "disconnect" {
                    return;
                }
                if mode != "missing" {
                    let args = if mode == "invalid" {
                        json!({"ok":"wrong"})
                    } else {
                        json!({"ok":true})
                    };
                    send(&mut write,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"f","_meta":{"goose":{"toolCall":{"toolName":"final_output"}}},"rawInput":args,"status":"pending"}}})).await;
                    send(&mut write,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"f","status":if mode=="rejected"{"failed"}else{"completed"}}}})).await;
                }
                json!({"stopReason":if mode == "turn_limit" {"max_turn_requests"} else {"end_turn"}})
            }
            Some("session/cancel") => return,
            _ => continue,
        };
        send(
            &mut write,
            json!({"jsonrpc":"2.0","id":req["id"],"result":result}),
        )
        .await;
    }
}
async fn scenario(mode: &'static str) -> RunResult {
    let (client, server) = tokio::io::duplex(8192);
    let stopped = Arc::new(AtomicBool::new(false));
    let fake = Fake {
        stream: Mutex::new(Some(client)),
        stopped: stopped.clone(),
    };
    let peer = tokio::spawn(scripted(mode, server));
    let mut spec = LaunchSpec::new("unused");
    spec.env.insert("TOKEN".into(), "SECRET_TOKEN_VALUE".into());
    spec.cancellation_grace = Duration::from_millis(if mode == "fragment" { 100 } else { 10 });
    let cancel = CancellationToken::new();
    if mode == "cancel" || mode == "fragment" {
        let c = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c.cancel();
        });
    }
    let result = run_with_launcher(&fake, &spec, &task(), cancel).await;
    assert!(stopped.load(Ordering::SeqCst));
    peer.abort();
    result
}
#[tokio::test]
async fn accepted_output_and_evidence() {
    let r = scenario("valid").await;
    assert_eq!(r.status, Completion::Completed);
    assert_eq!(r.output, Some(json!({"ok":true})));
    assert!(r.usage.is_none());
    let log = serde_json::to_string(&r.transcript).unwrap();
    assert!(!log.contains("secret reasoning"));
    assert!(!log.contains("SECRET_TOKEN_VALUE"));
    assert!(log.contains("REDACTED"));
}
#[tokio::test]
async fn rejects_missing_invalid_and_failed_submission() {
    for mode in ["missing", "invalid", "rejected"] {
        assert_eq!(
            scenario(mode).await.status,
            Completion::InvalidOutput,
            "{mode}"
        );
    }
}
#[tokio::test]
async fn cancellation_and_disconnect_keep_partial_evidence() {
    for (mode, status) in [
        ("cancel", Completion::Cancelled),
        ("disconnect", Completion::ProtocolError),
    ] {
        let r = scenario(mode).await;
        assert_eq!(r.status, status);
        assert!(!r.transcript.is_empty());
    }
}

#[tokio::test]
async fn deadlines_do_not_replay() {
    let (client, server) = tokio::io::duplex(8192);
    let stopped = Arc::new(AtomicBool::new(false));
    let fake = Fake {
        stream: Mutex::new(Some(client)),
        stopped: stopped.clone(),
    };
    let peer = tokio::spawn(scripted("cancel", server));
    let mut t = task();
    t.timeout = Duration::from_millis(30);
    let mut s = LaunchSpec::new("unused");
    s.cancellation_grace = Duration::from_millis(10);
    let r = run_with_launcher(&fake, &s, &t, CancellationToken::new()).await;
    assert_eq!(r.status, Completion::TimedOut);
    assert!(stopped.load(Ordering::SeqCst));
    peer.abort();
}

#[tokio::test]
async fn bounded_frame_rejects_unterminated_input() {
    let (client, mut server) = tokio::io::duplex(8192);
    let stopped = Arc::new(AtomicBool::new(false));
    let fake = Fake {
        stream: Mutex::new(Some(client)),
        stopped,
    };
    let peer = tokio::spawn(async move {
        let mut buf = [0; 1024];
        use tokio::io::AsyncReadExt;
        let count = server.read(&mut buf).await.unwrap();
        assert!(count > 0);
        server.write_all(&vec![b'x'; 1024]).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let mut s = LaunchSpec::new("unused");
    s.max_frame_bytes = 512;
    let r = run_with_launcher(&fake, &s, &task(), CancellationToken::new()).await;
    assert_eq!(r.status, Completion::ProtocolError);
    assert!(r.error.unwrap().contains("frame exceeds"));
    peer.abort();
}

#[tokio::test]
async fn client_capabilities_are_denied() {
    assert_eq!(scenario("capabilities").await.status, Completion::Completed);
}

#[tokio::test]
async fn cancellation_retains_partial_frame_evidence() {
    let result = scenario("fragment").await;
    assert_eq!(result.status, Completion::Cancelled);
    assert!(result.text.contains("fragmented evidence"));
}

#[tokio::test]
async fn turn_limit_keeps_partial_output_without_accepting_success() {
    let result = scenario("turn_limit").await;
    assert_eq!(result.status, Completion::TurnLimit);
    assert!(result.output.is_some());
    assert!(!result.transcript.is_empty());
}
