//! Exercise the real CLI, Jev HTTP adapter, GitHub command adapter and durable state.
//! External services are replaced at the process boundary; no account or API key is used.
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::{net::TcpListener, process::Command};

const GH: &str = r#"#!/usr/bin/env python3
import json, os, sys
from urllib.parse import urlsplit, parse_qs
p = os.environ['FIXTURE']
s = json.load(open(p))
a = sys.argv[1:]
with open(p + '.calls', 'a') as f: f.write(json.dumps(a) + '\n')
if a[:2] == ['repo', 'clone']:
    import subprocess
    sys.exit(subprocess.call(['git','clone',s['remote'],a[3]]))
if a[:2] == ['pr', 'diff']:
    if s.get('fail_diff'):
        print('diff unavailable', file=sys.stderr)
        sys.exit(1)
    if int(a[2]) in s.get('oversized_prs', []):
        print('x' * (80 * 1024 + 1))
        sys.exit(0)
    print('diff --git a/db.rs b/db.rs\n--- a/db.rs\n+++ b/db.rs\n@@ -1 +1 @@\n-bug\n+fix')
    sys.exit(0)
assert a[:3] == ['api', '--method', a[2]], a
method, endpoint = a[2:4]
body = json.load(sys.stdin) if '--input' in a else None
path = endpoint.split('?')[0]
root = 'repos/owner/repo'
result = None
if s.get('rate_limit_endpoint') == path:
    print('HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 4102444800\r\n\r\n{}')
    print('gh: API rate limit exceeded for user ID 1 (HTTP 403)', file=sys.stderr)
    sys.exit(1)
if path == 'user': result = {'login':'fiach-bot'}
elif path == root + '/issues':
    query = parse_qs(urlsplit(endpoint).query)
    page = int(query.get('page',['1'])[0]); size = int(query.get('per_page',['100'])[0])
    if s.get('fail_inventory_page') == page: sys.exit(1)
    result = sorted(s['items'], key=lambda i: i['number'])[(page-1)*size:page*size]
elif path == root + '/labels':
    result = [{'name':x} for x in s['labels']]
    if method == 'POST': s['labels'].append(body['name']); result = body
elif path == root + '/issues/comments':
    query = parse_qs(urlsplit(endpoint).query)
    page = int(query.get('page',['1'])[0]); size = int(query.get('per_page',['100'])[0])
    if s.get('fail_comments_page') == page: sys.exit(1)
    comments = [dict(c, issue_url='https://api.github.com/' + root + '/issues/' + n) for n, cs in s['comments'].items() for c in cs]
    result = comments[(page-1)*size:page*size]
elif path.startswith(root + '/issues/comments/'):
    assert method == 'PATCH'
    comment = next(c for cs in s['comments'].values() for c in cs if c['id'] == int(path.split('/')[-1]))
    comment['body'] = body['body']; result = comment
elif path.startswith(root + '/issues/'):
    tail = path[len(root + '/issues/'):].split('/')
    n = int(tail[0]); issue = next(i for i in s['items'] if i['number'] == n)
    if len(tail) == 1: result = issue
    elif tail[1] == 'comments':
        comments = s['comments'].setdefault(str(n), [])
        if method == 'POST':
            comments.append({'id':1000+n,'user':{'login':'fiach-bot'},'body':body['body']})
            issue['updated_at'] = 'bot-updated'
            result = comments[-1]
        else: result = comments
    elif tail[1] == 'labels':
        if method == 'POST':
            for label in body['labels']:
                if not any(l['name'] == label for l in issue['labels']): issue['labels'].append({'name':label})
        elif method == 'DELETE':
            from urllib.parse import unquote
            issue['labels'] = [l for l in issue['labels'] if l['name'] != unquote(tail[2])]
        else: raise Exception(a)
        result = issue['labels']
    else: raise Exception(a)
elif path.startswith(root + '/git/ref/heads/') or path.startswith(root + '/commits/'):
    import subprocess
    ref = path.split('/heads/',1)[1] if '/heads/' in path else path.split('/commits/',1)[1]
    sha = subprocess.check_output(['git','rev-parse',ref],cwd=s['remote'],text=True).strip()
    result = {'object':{'sha':sha},'sha':sha}
elif path == root + '/pulls':
    if method == 'POST':
        if s.get('fail_pr_create'):
            s['fail_pr_create'] = False
            json.dump(s,open(p,'w'))
            sys.exit(1)
        assert body['draft'] is True
        assert 'Fixes' not in body['body'] and 'Closes' not in body['body']
        result = dict(body, html_url='https://github.com/owner/repo/pull/9', state='open')
        s.setdefault('prs', []).append(result)
        if s.get('lose_pr_response'):
            s['lose_pr_response'] = False
            json.dump(s,open(p,'w'))
            sys.exit(1)
    else: result = s.get('prs', [])
elif path.startswith(root + '/pulls/'):
    n = int(path.split('/')[-1]); result = {'state':'open','head':{'sha':'abc'},'base':{'sha':'base'}}
    s['pull_reads'] = s.get('pull_reads', 0) + 1
    if s.get('change_revision') and s['pull_reads'] >= 3:
        result[s['change_revision']]['sha'] = 'changed'
else: raise Exception(a)
json.dump(s,open(p,'w'))
if '--include' in a: print('HTTP/2.0 200 OK\r\nX-RateLimit-Remaining: 4000\r\n\r\n', end='')
print(json.dumps(result))
"#;

fn item(number: u64, pr: bool) -> Value {
    let mut value = json!({"number":number,"title":"Database returns wrong funding amount","body":"Expected stored amount, got zero. Reproduction supplied.","state":"open","updated_at":"original","labels":[{"name":"human-label"}]});
    if pr {
        value["pull_request"] = json!({"url":"unused"});
    }
    value
}

async fn setup(
    kind: &'static str,
    matching: &'static str,
    pr: bool,
    publish: bool,
) -> (
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<Value>>>,
) {
    let dir = tempfile::Builder::new()
        .prefix("fiach-integration-")
        .tempdir()
        .unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        GH.replace("/usr/bin/env python3", &python_executable().await),
    )
    .unwrap();
    std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut items = vec![item(1, false)];
    if pr {
        items.push(item(2, true));
    }
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&json!({"items":items,"labels":["human-label"],"comments":{}})).unwrap(),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(vec![]));
    let received = requests.clone();
    let app = Router::new().route("/v1/systemone", post(move |Json(request): Json<Value>| {
        let received = received.clone();
        async move {
            let fail_once = {
                let mut requests = received.lock().unwrap();
                let first = !requests.iter().any(|r| r == &request);
                requests.push(request.clone());
                first && request["state"]["candidate"]["body"] == "FAIL_ONCE"
            };
            let questions = request["questions"].as_object().unwrap();
            let mut answers = serde_json::Map::new();
            for (id, q) in questions {
                let selected = match id.as_str() {
                    "kind" => kind,
                    "information" => "sufficient",
                    "direction" => "established",
                    "area_2" => "no",
                    "match" => if request["state"]["candidate"]["comments"].to_string().contains("SAME_NOW") { "same" } else if request["state"]["pr_diff"].is_string() || request["state"]["candidate"]["is_pr"] == false { matching } else { "uncertain" },
                    _ => "yes",
                };
                let options = q["criteria"].as_object().unwrap();
                let probabilities: serde_json::Map<String,Value> = options.keys().map(|k| (k.clone(), json!(if k == selected {1.0} else {0.0}))).collect();
                answers.insert(id.clone(), json!({"type":"choice","choice":selected,"confidence":1.0,"probabilities":probabilities}));
            }
            if fail_once { answers["match"]["probabilities"] = json!({}); }
            let input_tokens = if request["state"]["issue"]["body"] == "LARGE_USAGE" { 10000 } else { 100 };
            Json(json!({"model":"jev-1.13.0","answers":answers,"usage":{"input_tokens":input_tokens,"output_tokens":10}}))
        }
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = format!(
        r#"
[issues]
state_path = "{state}"
scratch_dir = "{scratch}"
max_jev_cost_usd = 1.0
jev_base_url = "http://{address}"
publish = {publish}
auto_fix = false
[[issues.repos]]
repo = "owner/repo"
[[issues.repos.areas]]
label = "area:db"
description = "Database persistence and queries"
paths = ["calc.py", "tests/**"]
auto_fix = true
[[issues.repos.areas]]
label = "area:funding-source"
description = "Funding amounts and settlement"
paths = ["calc.py", "tests/**"]
auto_fix = true
"#,
        state = dir.path().join("state.redb").display(),
        scratch = dir.path().display()
    );
    std::fs::write(dir.path().join("fiach.toml"), config).unwrap();
    (dir, server, requests)
}

async fn run(dir: &Path) -> std::process::Output {
    let number = fixture(dir)["items"][0]["number"]
        .as_u64()
        .unwrap()
        .to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_fiach"));
    command.args([
        "--config",
        dir.join("fiach.toml").to_str().unwrap(),
        "issues",
    ]);
    if fixture(dir)["scan_all"] != true {
        command.args(["--issue", &number]);
    }
    command
        .env("FIXTURE", dir.join("fixture.json"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("TYPESAFE_API_KEY", "test-only")
        .env("RUST_LOG", "error")
        .current_dir(dir)
        .output()
        .await
        .unwrap()
}
fn fixture(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("fixture.json")).unwrap()).unwrap()
}
fn assert_ok(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn unlinked_open_pr_is_marked_and_diff_checked_without_closing_or_duplicate_comments() {
    let (dir, server, requests) = setup("bug", "same", true, true).await;
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    let labels: Vec<_> = state["items"][0]["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    for expected in [
        "bug",
        "area:db",
        "area:funding-source",
        "already-being-addressed",
        "human-label",
    ] {
        assert!(labels.contains(&expected));
    }
    assert_eq!(state["items"][0]["state"], "open");
    assert!(
        state["comments"]["1"][0]["body"]
            .as_str()
            .unwrap()
            .contains("#2")
    );
    assert!(requests.lock().unwrap().iter().any(|r| {
        r["state"]["pr_diff"]
            .as_str()
            .is_some_and(|s| s.contains("+fix"))
    }));
    let calls = requests.lock().unwrap().len();
    assert_ok(&run(dir.path()).await);
    assert_eq!(
        requests.lock().unwrap().len(),
        calls,
        "unchanged issue must not be re-evaluated"
    );
    assert_eq!(
        fixture(dir.path())["comments"]["1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
    assert!(!calls.contains("merge"));
    assert!(!calls.contains("close"));
    server.abort();
}

#[tokio::test]
async fn dry_run_emits_multiple_areas_without_writing_github() {
    let (dir, server, _) = setup("bug", "same", false, false).await;
    let output = run(dir.path()).await;
    assert_ok(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["decision"]["route"], "ready");
    assert_eq!(result["decision"]["labels"].as_array().unwrap().len(), 4);
    let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
    assert!(!calls.contains("POST"));
    assert!(!calls.contains("PATCH"));
    assert!(!calls.contains("DELETE"));
    server.abort();
}

#[tokio::test]
async fn fully_described_feature_still_requires_maintainer_and_reuses_comment_after_edit() {
    let (dir, server, _) = setup("feature", "same", false, true).await;
    assert_ok(&run(dir.path()).await);
    let mut state = fixture(dir.path());
    assert!(
        state["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "needs-decision")
    );
    state["items"][0]["body"] = json!("Updated feature request with more context");
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(
        fixture(dir.path())["comments"]["1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    server.abort();
}

const NSPAWN: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
root = next(a.split('=',1)[1] for a in args if a.startswith('--directory='))
request = next(a.split('=',1)[1].split(':')[0] for a in args if a.startswith('--bind-ro=') and a.endswith(':/input/request.json'))
output = next(a.split('=',1)[1].split(':')[0] for a in args if a.startswith('--bind=') and a.endswith(':/output'))
i = json.load(open(request)); workspace = root + '/workspace'
with open(os.environ['FIXTURE'] + '.phases','a') as f: f.write(i['phase'] + '\n')
report = {'status':'candidate','summary':'Fix persisted amount','test_files':['tests/test_amount.py'],'reproduction':['python3','tests/test_amount.py'],'approved':False}
if i['phase'] == 'code':
    fixture = json.load(open(os.environ['FIXTURE']))
    if fixture.get('concurrent_pr'):
        fixture['items'].append({'number':2,'title':'Fix funding amount','body':'Already implemented','state':'open','updated_at':'new','labels':[],'pull_request':{'url':'unused'}})
        json.dump(fixture,open(os.environ['FIXTURE'],'w'))
    if fixture.get('touch_denied'):
        os.mkdir(workspace + '/db')
        open(workspace + '/db/schema.sql','w').write('alter table accounts add column amount int;')
    if fixture.get('touch_ignored'):
        os.mkdir(workspace + '/db')
        open(workspace + '/db/schema.generated','w').write('restricted despite gitignore')
        subprocess.check_call(['git','add','-N','-f','db/schema.generated'],cwd=workspace)
    if fixture.get('touch_unmapped'):
        open(workspace + '/unmapped.txt','w').write('outside allowed scopes')
    open(workspace + '/calc.py','w').write('def amount():\n    return 1\n')
    os.mkdir(workspace + '/tests')
    open(workspace + '/tests/test_amount.py','w').write('import runpy\nassert runpy.run_path("calc.py")["amount"]() == 1\n')
    subprocess.check_call(['git','add','-N','.'],cwd=workspace)
    patch = subprocess.check_output(['git','diff','--binary',i['base']],cwd=workspace)
    open(output + '/patch.diff','wb').write(patch)
elif i['phase'] == 'check':
    p = subprocess.run(i['command'],cwd=workspace,capture_output=True,text=True)
    json.dump({'success':p.returncode==0,'output':p.stdout+p.stderr},open(output + '/check.json','w'))
    sys.exit(0)
elif i['phase'] == 'verify':
    assert 'Host regression command' in i['report']['summary']
    fixture = json.load(open(os.environ['FIXTURE']))
    report['approved'] = not fixture.get('reject_verifier',False)
    report['status'] = 'verified' if report['approved'] else 'needs_decision'
json.dump(report,open(output + '/report.json','w'))
"#;

async fn enable_worker(dir: &Path, reject: bool) {
    let remote = dir.join("remote");
    let output = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .arg(&remote)
        .output()
        .await
        .unwrap();
    assert_ok(&output);
    std::fs::write(remote.join("calc.py"), "def amount():\n    return 0\n").unwrap();
    std::fs::write(remote.join(".gitignore"), "*.generated\n").unwrap();
    let output = Command::new("git")
        .args(["add", "."])
        .current_dir(&remote)
        .output()
        .await
        .unwrap();
    assert_ok(&output);
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "base",
        ])
        .current_dir(&remote)
        .output()
        .await
        .unwrap();
    assert_ok(&output);
    let rootfs = dir.join("rootfs");
    std::fs::create_dir(&rootfs).unwrap();
    let nspawn = dir.join("bin/systemd-nspawn");
    std::fs::write(
        &nspawn,
        NSPAWN.replace("/usr/bin/env python3", &python_executable().await),
    )
    .unwrap();
    std::fs::set_permissions(nspawn, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut state = fixture(dir);
    state["remote"] = json!(remote);
    state["reject_verifier"] = json!(reject);
    std::fs::write(
        dir.join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let path = dir.join("fiach.toml");
    let mut config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("auto_fix = false", "auto_fix = true");
    config.push_str(&format!("\n[issues.worker]\nrootfs = {:?}\nprovider = \"test\"\nmodel = \"test\"\nnetwork = \"host\"\n",rootfs.to_str().unwrap()));
    std::fs::write(path, config).unwrap();
}

#[tokio::test]
async fn verified_fix_runs_real_regression_on_base_and_patch_then_opens_one_draft() {
    let (dir, server, _) = setup("bug", "different", false, true).await;
    enable_worker(dir.path(), false).await;
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    assert_eq!(state["prs"].as_array().unwrap().len(), 1);
    assert_eq!(state["prs"][0]["draft"], true);
    assert_eq!(state["items"][0]["state"], "open");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap(),
        "code\ncheck\ncheck\nverify\n"
    );
    assert_ok(&run(dir.path()).await);
    assert_eq!(fixture(dir.path())["prs"].as_array().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn verifier_rejection_marks_for_maintainer_and_does_not_retry_unchanged_issue() {
    let (dir, server, _) = setup("bug", "different", false, true).await;
    enable_worker(dir.path(), true).await;
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    assert!(state.get("prs").is_none());
    assert!(
        state["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "needs-decision")
    );
    let phases = std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap(),
        phases
    );
    server.abort();
}

fn set_flag(dir: &Path, name: &str) {
    let mut state = fixture(dir);
    state[name] = json!(true);
    std::fs::write(
        dir.join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn lost_pr_response_is_reconciled_without_a_second_fix_or_pr() {
    let (dir, server, _) = setup("bug", "different", false, true).await;
    enable_worker(dir.path(), false).await;
    set_flag(dir.path(), "lose_pr_response");
    assert!(!run(dir.path()).await.status.success());
    assert_eq!(fixture(dir.path())["prs"].as_array().unwrap().len(), 1);
    let phases = std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(fixture(dir.path())["prs"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap(),
        phases
    );
    server.abort();
}

#[tokio::test]
async fn pr_opened_during_coding_prevents_a_redundant_fix_pr() {
    let (dir, server, _) = setup("bug", "same", false, true).await;
    enable_worker(dir.path(), false).await;
    set_flag(dir.path(), "concurrent_pr");
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    assert!(state.get("prs").is_none());
    assert!(
        state["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "already-being-addressed")
    );
    assert!(
        state["comments"]["1"][0]["body"]
            .as_str()
            .unwrap()
            .contains("#2")
    );
    server.abort();
}

#[tokio::test]
async fn incomplete_inventory_never_authorizes_labels_or_a_fix() {
    let (dir, server, requests) = setup("bug", "same", false, true).await;
    let mut state = fixture(dir.path());
    state["items"]
        .as_array_mut()
        .unwrap()
        .extend((2..152).map(|n| item(n, true)));
    state["fail_inventory_page"] = json!(2);
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert!(!run(dir.path()).await.status.success());
    assert!(requests.lock().unwrap().is_empty());
    let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
    assert!(!calls.contains("POST"));
    server.abort();
}

async fn python_executable() -> String {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .await
        .unwrap();
    assert_ok(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn duplicate_issue_is_linked_to_older_report_and_left_open() {
    let (dir, server, _) = setup("bug", "same", true, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["number"] = json!(2);
    state["items"][1]["number"] = json!(1);
    state["items"][1]
        .as_object_mut()
        .unwrap()
        .remove("pull_request");
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    assert!(
        state["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "duplicate")
    );
    assert!(
        state["comments"]["2"][0]["body"]
            .as_str()
            .unwrap()
            .contains("#1")
    );
    assert_eq!(state["items"][0]["state"], "open");
    server.abort();
}

#[tokio::test]
async fn disabled_automation_pauses_pending_publication_and_can_resume() {
    for remove_worker in [false, true] {
        let (dir, server, _) = setup("bug", "different", false, true).await;
        enable_worker(dir.path(), false).await;
        set_flag(dir.path(), "fail_pr_create");
        assert!(!run(dir.path()).await.status.success());
        let config_path = dir.path().join("fiach.toml");
        let enabled = std::fs::read_to_string(&config_path).unwrap();
        let paused = if remove_worker {
            enabled.split("[issues.worker]").next().unwrap().to_owned()
        } else {
            enabled.replacen("auto_fix = true", "auto_fix = false", 1)
        };
        std::fs::write(&config_path, paused).unwrap();
        let calls_path = dir.path().join("fixture.json.calls");
        std::fs::write(&calls_path, "").unwrap();
        assert_ok(&run(dir.path()).await);
        let calls = std::fs::read_to_string(&calls_path).unwrap();
        assert!(!calls.contains("\"POST\", \"repos/owner/repo/pulls\""));
        assert!(fixture(dir.path()).get("prs").is_none());
        let state = fixture(dir.path());
        assert!(
            state["comments"]["1"][0]["body"]
                .as_str()
                .unwrap()
                .contains("paused")
        );
        std::fs::write(&config_path, enabled).unwrap();
        assert_ok(&run(dir.path()).await);
        assert_eq!(fixture(dir.path())["prs"].as_array().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap(),
            "code\ncheck\ncheck\nverify\n"
        );
        server.abort();
    }
}

#[tokio::test]
async fn changed_area_policy_blocks_recovering_a_previously_verified_patch() {
    let (dir, server, _) = setup("bug", "different", false, true).await;
    enable_worker(dir.path(), false).await;
    set_flag(dir.path(), "fail_pr_create");
    assert!(!run(dir.path()).await.status.success());
    let path = dir.path().join("fiach.toml");
    let config = std::fs::read_to_string(&path).unwrap().replace(
        "paths = [\"calc.py\", \"tests/**\"]",
        "paths = [\"tests/**\"]",
    );
    std::fs::write(path, config).unwrap();
    let output = run(dir.path()).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("current area policy"));
    assert!(fixture(dir.path()).get("prs").is_none());
    server.abort();
}

#[tokio::test]
async fn patch_cannot_cross_disabled_or_unmapped_areas_despite_jev_approval() {
    for flag in ["touch_denied", "touch_unmapped", "touch_ignored"] {
        let (dir, server, _) = setup("bug", "different", false, true).await;
        enable_worker(dir.path(), false).await;
        set_flag(dir.path(), flag);
        let path = dir.path().join("fiach.toml");
        let config = std::fs::read_to_string(&path).unwrap().replace(
            "[issues.worker]",
            r#"
[[issues.repos.areas]]
label = "area:restricted-db"
description = "Schema changes require a maintainer"
paths = ["db/**"]
auto_fix = false
[issues.worker]"#,
        );
        std::fs::write(path, config).unwrap();
        assert_ok(&run(dir.path()).await);
        let state = fixture(dir.path());
        assert!(state.get("prs").is_none());
        assert!(
            state["items"][0]["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["name"] == "needs-decision")
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("fixture.json.phases")).unwrap(),
            "code\n"
        );
        server.abort();
    }
}

#[tokio::test]
async fn rate_limit_wait_does_not_repoll_and_can_be_cancelled() {
    use std::{process::Stdio, time::Duration};

    use tokio::io::{AsyncBufReadExt, BufReader};

    for endpoint in ["user", "repos/owner/repo/issues"] {
        let (dir, server, _) = setup("bug", "different", false, false).await;
        let mut state = fixture(dir.path());
        state["rate_limit_endpoint"] = json!(endpoint);
        std::fs::write(
            dir.path().join("fixture.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let config = dir.path().join("fiach.toml");
        let text = std::fs::read_to_string(&config)
            .unwrap()
            .replace("[issues]", "[issues]\ninterval_secs = 1");
        std::fs::write(&config, text).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_fiach"))
            .args(["--config", config.to_str().unwrap(), "issues", "--watch"])
            .env("FIXTURE", dir.path().join("fixture.json"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("TYPESAFE_API_KEY", "test-only")
            .env("RUST_LOG", "info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("Workflow exited before waiting");
                if line.contains("Waiting for GitHub quota")
                    || (line.contains("Waiting for next issue poll")
                        && line.contains("rate_limited=true"))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let calls_path = dir.path().join("fixture.json.calls");
        let before = std::fs::read_to_string(&calls_path).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            std::fs::read_to_string(&calls_path).unwrap(),
            before,
            "The ordinary poll interval must not bypass quota backoff"
        );
        assert!(
            Command::new("kill")
                .args(["-INT", &child.id().unwrap().to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        server.abort();
    }
}

#[tokio::test]
async fn issue_rate_limit_stops_the_pass_before_other_issues_or_publication() {
    let (dir, server, _) = setup("bug", "same", true, true).await;
    let mut state = fixture(dir.path());
    state["scan_all"] = json!(true);
    state["rate_limit_endpoint"] = json!("repos/owner/repo/issues/2");
    state["items"].as_array_mut().unwrap().push(item(3, false));
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let output = run(dir.path()).await;
    assert!(!output.status.success());
    let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
    let calls: Vec<Value> = calls
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        calls
            .iter()
            .filter(|c| c[3] == "repos/owner/repo/issues/2")
            .count(),
        1
    );
    assert!(!calls.iter().any(|c| c[3] == "repos/owner/repo/issues/3"));
    assert!(
        !calls
            .iter()
            .any(|c| c[2] == "POST" || c[2] == "PATCH" || c[2] == "DELETE")
    );
    server.abort();
}

#[tokio::test]
async fn candidate_details_and_unchanged_diffs_are_shared_across_issues() {
    for revision in [None, Some("head"), Some("base")] {
        let (dir, server, _) = setup("bug", "same", true, false).await;
        let mut state = fixture(dir.path());
        state["scan_all"] = json!(true);
        state["change_revision"] = json!(revision);
        state["items"].as_array_mut().unwrap().push(item(3, false));
        std::fs::write(
            dir.path().join("fixture.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert_ok(&run(dir.path()).await);
        let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
        let calls: Vec<Value> = calls
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c[3] == "repos/owner/repo/issues/2")
                .count(),
            2,
            "The candidate should be fetched once, with its pagination consistency reread"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c[0] == "pr" && c[1] == "diff")
                .count(),
            if revision.is_some() { 2 } else { 1 }
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c[3] == "repos/owner/repo/pulls/2")
                .count(),
            if revision.is_some() { 4 } else { 3 },
            "Both issues must validate the current PR revisions, with an extra check after download"
        );
        server.abort();
    }
}

#[tokio::test]
async fn failed_pr_diff_command_still_fails_triage_with_candidate_context() {
    let (dir, server, _) = setup("bug", "same", true, true).await;
    let mut state = fixture(dir.path());
    state["fail_diff"] = json!(true);
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let output = run(dir.path()).await;
    assert!(!output.status.success());
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        logs.contains("Fetching diff for owner/repo#2 failed: diff unavailable"),
        "{logs}"
    );
    assert!(
        fixture(dir.path())["comments"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v.as_array().unwrap().is_empty())
    );
    server.abort();
}

#[tokio::test]
async fn oversized_pr_diff_preserves_uncertainty_and_continues_comparisons() {
    for matching in ["different", "same"] {
        let (dir, server, requests) = setup("bug", matching, true, true).await;
        enable_worker(dir.path(), false).await;
        let mut state = fixture(dir.path());
        state["oversized_prs"] = json!([2]);
        state["items"].as_array_mut().unwrap().push(item(3, true));
        std::fs::write(
            dir.path().join("fixture.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();

        assert_ok(&run(dir.path()).await);
        let state = fixture(dir.path());
        let expected = if matching == "same" {
            "already-being-addressed"
        } else {
            "needs-decision"
        };
        assert!(
            state["items"][0]["labels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["name"] == expected)
        );
        let body = state["comments"]["1"][0]["body"].as_str().unwrap();
        assert!(
            body.contains("#2") && body.contains("81920-byte") && body.contains("unresolved"),
            "{body}"
        );
        let received = requests.lock().unwrap();
        assert!(
            received.iter().any(
                |r| r["state"]["candidate"]["number"] == 3 && r["state"]["pr_diff"].is_string()
            ),
            "Later candidates must still be checked"
        );
        assert!(
            !received.iter().any(
                |r| r["state"]["candidate"]["number"] == 2 && r["state"]["pr_diff"].is_string()
            ),
            "Partial diffs must never reach Jev"
        );
        assert!(
            !dir.path().join("fixture.json.phases").exists(),
            "Unresolved or addressed issues must never start the worker"
        );
        server.abort();
    }
}

#[tokio::test]
async fn large_history_does_not_block_triage_or_hide_a_late_closed_duplicate() {
    let (dir, server, requests) = setup("bug", "same", false, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["number"] = json!(2000);
    let items = state["items"].as_array_mut().unwrap();
    for n in 1..1200 {
        let mut closed_pr = item(n, true);
        closed_pr["state"] = json!("closed");
        items.push(closed_pr);
    }
    let mut duplicate = item(1200, false);
    duplicate["state"] = json!("closed");
    items.push(duplicate);
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    // Work limit 1 must not prevent evidence collection beyond the first page.
    let path = dir.path().join("fiach.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("[issues]", "[issues]\nmax_items = 1");
    std::fs::write(path, config).unwrap();
    assert_ok(&run(dir.path()).await);
    let state = fixture(dir.path());
    assert!(
        state["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "duplicate")
    );
    assert!(
        state["comments"]["2000"][0]["body"]
            .as_str()
            .unwrap()
            .contains("#1200")
    );
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "closed PRs must not reach Jev; identical detailed evidence is cached"
    );
    server.abort();
}

#[tokio::test]
async fn work_limit_batches_changed_issues_without_counting_cached_results() {
    let (dir, server, _) = setup("feature", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["scan_all"] = json!(true);
    state["items"].as_array_mut().unwrap().push(item(2, false));
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let path = dir.path().join("fiach.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("[issues]", "[issues]\nmax_items = 1");
    std::fs::write(path, config).unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(
        fixture(dir.path())["comments"]["1"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        fixture(dir.path())["comments"]["2"]
            .as_array()
            .is_none_or(|cs| cs.is_empty())
    );
    assert_ok(&run(dir.path()).await);
    assert_eq!(
        fixture(dir.path())["comments"]["2"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn more_than_one_thousand_closed_issues_are_checked_without_a_history_cap() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["number"] = json!(1200);
    for n in 1..1100 {
        let mut closed = item(n, false);
        closed["state"] = json!("closed");
        state["items"].as_array_mut().unwrap().push(closed);
    }
    std::fs::write(
        dir.path().join("fixture.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 1100);
    assert!(
        fixture(dir.path())["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "ready-for-agent")
    );
    server.abort();
}

fn save_fixture(dir: &Path, state: &Value) {
    std::fs::write(dir.join("fixture.json"), serde_json::to_vec(state).unwrap()).unwrap();
}

#[tokio::test]
async fn new_candidates_reuse_old_comparisons_across_process_restarts() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["number"] = json!(10);
    state["items"].as_array_mut().unwrap().push(item(2, false));
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 2);
    let mut state = fixture(dir.path());
    state["items"].as_array_mut().unwrap().push(item(3, false));
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2]["state"]["candidate"]["number"], 3);
    server.abort();
}

#[tokio::test]
async fn human_candidate_discussion_invalidates_only_affected_comparison() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["number"] = json!(10);
    state["items"]
        .as_array_mut()
        .unwrap()
        .extend([item(2, false), item(3, false)]);
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 3);
    let mut state = fixture(dir.path());
    state["comments"]["2"] = json!([{"id":42,"user":{"login":"fiach-bot"},"body":"<!-- fiach-issue-triage -->\nbot status"}]);
    state["items"][1]["updated_at"] = json!("bot update");
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 3);
    let mut state = fixture(dir.path());
    state["comments"]["2"].as_array_mut().unwrap().push(json!({"id":43,"user":{"login":"maintainer"},"author_association":"OWNER","body":"SAME_NOW: this has the same root cause"}));
    state["items"][1]["updated_at"] = json!("human update");
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(
        fixture(dir.path())["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "duplicate")
    );
    // An edit changes the decision even if GitHub's timestamp stays the same.
    let mut state = fixture(dir.path());
    state["comments"]["2"][1]["body"] = json!("Actually a different cause");
    save_fixture(dir.path(), &state);
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 5);
    assert!(
        !fixture(dir.path())["items"][0]["labels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["name"] == "duplicate")
    );
    server.abort();
}

#[tokio::test]
async fn failed_comparison_preserves_progress_and_persists_retry_backoff() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["items"]
        .as_array_mut()
        .unwrap()
        .extend([item(2, false), item(3, false)]);
    state["items"][2]["body"] = json!("FAIL_ONCE");
    save_fixture(dir.path(), &state);
    assert!(!run(dir.path()).await.status.success());
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert!(
        fixture(dir.path())["comments"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v.as_array().unwrap().is_empty())
    );
    // A new process honors the cooldown and sends no new Jev requests.
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 3);
    // A changed budget explicitly bypasses the cooldown without discarding evidence.
    let path = dir.path().join("fiach.toml");
    let config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("max_jev_cost_usd = 1.0", "max_jev_cost_usd = 1.1");
    std::fs::write(path, config).unwrap();
    assert_ok(&run(dir.path()).await);
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert_eq!(
        requests.lock().unwrap()[3]["state"]["candidate"]["number"],
        3
    );
    assert!(
        !fixture(dir.path())["comments"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v.as_array().unwrap().is_empty())
    );
    server.abort();
}

#[tokio::test]
async fn exhausted_budget_resumes_without_repaying_for_completed_requests() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    let mut state = fixture(dir.path());
    state["items"][0]["body"] = json!("LARGE_USAGE");
    state["items"]
        .as_array_mut()
        .unwrap()
        .extend([item(2, false), item(3, false)]);
    save_fixture(dir.path(), &state);
    let path = dir.path().join("fiach.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    for (budget, expected_calls, success) in
        [(0.0005, 1, false), (0.00051, 2, false), (0.00052, 3, true)]
    {
        std::fs::write(
            &path,
            config.replace(
                "max_jev_cost_usd = 1.0",
                &format!("max_jev_cost_usd = {budget}"),
            ),
        )
        .unwrap();
        let output = run(dir.path()).await;
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(requests.lock().unwrap().len(), expected_calls);
        if !success {
            assert!(
                fixture(dir.path())["comments"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|v| v.as_array().unwrap().is_empty())
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn incomplete_discussion_inventory_cannot_reuse_a_cached_decision() {
    let (dir, server, requests) = setup("bug", "different", false, true).await;
    assert_ok(&run(dir.path()).await);
    let count = requests.lock().unwrap().len();
    let mut state = fixture(dir.path());
    state["fail_comments_page"] = json!(1);
    save_fixture(dir.path(), &state);
    std::fs::write(dir.path().join("fixture.json.calls"), "").unwrap();
    assert!(!run(dir.path()).await.status.success());
    assert_eq!(requests.lock().unwrap().len(), count);
    let calls = std::fs::read_to_string(dir.path().join("fixture.json.calls")).unwrap();
    assert!(!calls.contains("POST"));
    server.abort();
}
