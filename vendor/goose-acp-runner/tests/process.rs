#![allow(clippy::unwrap_used, clippy::expect_used)]
use goose_acp_runner::{Completion, LaunchSpec, Task, run};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixture() -> (tempfile::TempDir, LaunchSpec, Task) {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = LaunchSpec::new("sh");
    spec.args = vec![
        "-c".into(),
        "(while :; do printf x >> \"$MARKER\"; sleep 0.02; done) & wait".into(),
    ];
    spec.env
        .insert("MARKER".into(), dir.path().join("marker").into_os_string());
    spec.env
        .insert("PATH".into(), std::env::var_os("PATH").unwrap());
    let task = Task {
        prompt: "test".into(),
        instructions: String::new(),
        provider: "unused".into(),
        model: "unused".into(),
        cwd: dir.path().to_owned(),
        output_schema: None,
        extensions: vec![],
        timeout: Duration::from_secs(3),
        max_turns: 1,
    };
    (dir, spec, task)
}

async fn started(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn stopped(path: &std::path::Path) {
    tokio::time::sleep(Duration::from_millis(80)).await;
    let before = tokio::fs::read(path).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        before,
        tokio::fs::read(path).await.unwrap(),
        "descendant kept executing after cleanup"
    );
}

#[tokio::test]
async fn cancellation_stops_owned_descendants() {
    let (dir, spec, task) = fixture();
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let future = tokio::spawn(async move { run(&spec, &task, child_cancel).await });
    let marker = dir.path().join("marker");
    started(&marker).await;
    cancel.cancel();
    assert_eq!(future.await.unwrap().status, Completion::Cancelled);
    stopped(&marker).await;
}

#[tokio::test]
async fn dropping_execution_stops_owned_descendants() {
    let (dir, spec, task) = fixture();
    let future = tokio::spawn(async move { run(&spec, &task, CancellationToken::new()).await });
    let marker = dir.path().join("marker");
    started(&marker).await;
    future.abort();
    let _ = future.await;
    stopped(&marker).await;
}

#[tokio::test]
async fn zero_turn_limit_is_rejected_before_launch() {
    let (dir, spec, mut task) = fixture();
    task.max_turns = 0;
    let result = run(&spec, &task, CancellationToken::new()).await;
    assert_eq!(result.status, Completion::LaunchError);
    assert!(!dir.path().join("marker").exists());
}
