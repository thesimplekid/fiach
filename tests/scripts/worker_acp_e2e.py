#!/usr/bin/env python3
"""Exercise the packaged worker, real nspawn and real Goose against a local model.

No forge commands or real model credentials are used. Artifacts are retained.
Run with --rootfs pointing to the managed Nix root, or --module-config pointing
at checks.<system>.nixos-module/config.toml. --sudo applies only to nspawn and
signals sent to process groups or identity-checked children owned by this runner.
"""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import shutil
import shlex
import signal
import socket
import subprocess
import tempfile
import threading
import time
import tomllib
import uuid

KEY = "fiach-acp-e2e-synthetic-key"
MODEL = "gpt-4o-mini"
BLOCKED = dict(status="needs_review", summary="Work preserved; reporting recovery only",
               test_files=[], reproduction=[], approved=False)
CANDIDATE = dict(status="candidate", summary="Correct addition with a regression test",
                 test_files=["test_calc.py"], reproduction=["python3", "test_calc.py"], approved=False)
VERIFIED = dict(status="verified", summary="Independently ran the regression and inspected the diff",
                test_files=[], reproduction=[], approved=True)
WORK = """set -eu
cd /workspace
python3 - <<'CODE'
from pathlib import Path
Path('calc.py').write_text('def add(a, b):\\n    return a + b\\n')
Path('test_calc.py').write_text('from calc import add\\nassert add(2, 3) == 5\\n')
with Path('actions.log').open('a') as stream:
    stream.write('work\\n')
CODE
git add -N test_calc.py
python3 test_calc.py
"""
VERIFY = """set -eu
cd /workspace
git diff --no-ext-diff --no-textconv HEAD -- calc.py test_calc.py
python3 test_calc.py
printf 'verify\\n' >> verifier.log
"""


def command(args, **kwargs):
    if str(args[0]) == "git":
        kwargs["env"] = dict(os.environ, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
    return subprocess.run([str(arg) for arg in args], check=True, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs).stdout


class Plan:
    def __init__(self, mode, path):
        self.mode, self.path = mode, path
        self.lock = threading.Lock()
        self.work_calls = 0
        self.recovery_calls = 0
        self.requests = 0
        self.stalled = threading.Event()
        self.release = threading.Event()

    def answer(self, body):
        names = sorted(tool["function"]["name"] for tool in body.get("tools", []))
        with self.lock:
            self.requests += 1
            with self.path.open("a") as stream:
                stream.write(json.dumps(dict(tools=names, body=body)) + "\n")
            if "final_output" not in names:
                return dict(text="Fixture task")  # Goose title/utility request.
            if "workspace__shell" in names:
                self.work_calls += 1
                # A fresh work conversation must request the action again. If
                # the harness accidentally replays work, the marker exposes it
                # rather than a global mock counter hiding the duplicate.
                if not any(message.get("role") == "tool" for message in body.get("messages", [])):
                    action = VERIFY if self.mode == "verify" else WORK
                    if self.mode in ("cancel", "hard_cancel"):
                        heartbeat = "import time\nfrom pathlib import Path\nwhile True:\n    with Path('/workspace/heartbeat.log').open('a') as f: f.write('beat\\n')\n    time.sleep(0.1)\n"
                        action += "python3 -u -c " + shlex.quote(heartbeat) + "\n"
                    return dict(name="workspace__shell", arguments=dict(command=action))
                if self.mode in ("disconnect", "cancel"):
                    return dict(control=self.mode)
                report = VERIFIED if self.mode == "verify" else CANDIDATE.copy()
                if self.mode in ("invalid", "recovery_exhausted"):
                    report["approved"] = True  # Valid JSON schema, rejected by host policy.
                return dict(name="final_output", arguments=report)
            self.recovery_calls += 1
            assert set(names) <= {"final_output", "load_skill"}, names
            if self.mode in ("invalid", "turn_limit") and self.recovery_calls == 1:
                return dict(name="workspace__shell", arguments=dict(command="touch /workspace/FORBIDDEN_RECOVERY"))
            report = BLOCKED if self.mode != "recovery_exhausted" else dict(CANDIDATE, approved=True)
            return dict(name="final_output", arguments=report)


class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        body = json.dumps(dict(object="list", data=[dict(id=MODEL, object="model", owned_by="fixture")])).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        plan = self.server.plan
        if self.headers.get("Authorization") != "Bearer " + KEY:
            self.send_error(403, "Only the synthetic fixture credential is accepted")
            return
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        answer = plan.answer(body)
        control = answer.get("control")
        if control == "disconnect":
            self.connection.shutdown(socket.SHUT_RDWR)
            self.connection.close()
            return
        if control == "cancel":
            plan.stalled.set()
            plan.release.wait(60)
            self.close_connection = True
            return
        if "text" in answer:
            delta = dict(role="assistant", content=answer["text"])
            finish = "stop"
        else:
            delta = dict(role="assistant", tool_calls=[dict(index=0, id=f"fixture-{plan.requests}", type="function",
                         function=dict(name=answer["name"], arguments=json.dumps(answer["arguments"])))])
            finish = "tool_calls"
        base = dict(id=f"completion-{plan.requests}", object="chat.completion.chunk", created=1, model=MODEL)
        chunks = [dict(base, choices=[dict(index=0, delta=delta, finish_reason=None)]),
                  dict(base, choices=[dict(index=0, delta={}, finish_reason=finish)],
                       usage=dict(prompt_tokens=100, completion_tokens=20, total_tokens=120))]
        payload = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(payload)


class Runner:
    def __init__(self, rootfs, artifacts, sudo):
        self.rootfs, self.artifacts = rootfs, artifacts
        self.privilege = ["sudo", "-n"] if sudo else []
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.summary = []

    def stop_group(self, process, sig):
        # Each child gets a new session. Only that exact owned group is signalled.
        if process.poll() is None:
            subprocess.run(self.privilege + ["kill", "-" + signal.Signals(sig).name.removeprefix("SIG"),
                                             "--", "-" + str(process.pid)], check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    @staticmethod
    def process_identity(pid):
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return fields[19] if fields[0] != "Z" else None
        except (FileNotFoundError, ProcessLookupError):
            return None

    @classmethod
    def is_same_process(cls, member):
        return member[1] is not None and cls.process_identity(member[0]) == member[1]

    @classmethod
    def process_tree(cls, root):
        rows = [line.split(maxsplit=2) for line in command(["ps", "-eo", "pid=,ppid=,comm="]).splitlines()]
        pending, found = [root], []
        while pending:
            pid = pending.pop(0)
            name = next((row[2] for row in rows if int(row[0]) == pid), "exited")
            found.append((pid, cls.process_identity(pid), name))
            pending.extend(int(row[0]) for row in rows if int(row[1]) == pid)
        return found

    def signal_member(self, member, sig):
        if self.is_same_process(member):
            subprocess.run(self.privilege + ["kill", "-" + signal.Signals(sig).name.removeprefix("SIG"),
                                             "--", str(member[0])], check=False,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def repository(self, name):
        repo = self.artifacts / name
        repo.mkdir()
        command(["git", "init", "-q", repo])
        command(["git", "-C", repo, "config", "user.name", "ACP fixture"])
        command(["git", "-C", repo, "config", "user.email", "fixture@invalid.example"])
        command(["git", "-C", repo, "config", "core.hooksPath", "/dev/null"])
        command(["git", "-C", repo, "config", "commit.gpgsign", "false"])
        (repo / "calc.py").write_text("def add(a, b):\n    return a - b\n")
        command(["git", "-C", repo, "add", "calc.py"])
        command(["git", "-C", repo, "commit", "-qm", "Fixture base"])
        return repo, command(["git", "-C", repo, "rev-parse", "HEAD"]).strip()

    def stage(self, name, repo, base, mode, phase="code", report=None, max_turns=8):
        stage = self.artifacts / name
        stage.mkdir()
        root = stage / "rootfs"
        shutil.copytree(self.rootfs, root, symlinks=True)
        root.chmod(0o755)
        for name_ in ["tmp", "run", "var", "var/tmp", "workspace", "output", "input", "nix", "nix/store"]:
            path = root / name_
            path.mkdir(exist_ok=True, parents=True)
            if not path.is_symlink():
                path.chmod(0o755)
        output = stage / "output"
        output.mkdir()
        request = dict(fix_kind="bug", config=dict(rootfs=str(self.rootfs), provider="openai", model=MODEL,
                       verifier_provider=None, verifier_model=None, network="host", max_turns=max_turns,
                       timeout_secs=20 if mode in ("cancel", "hard_cancel", "disconnect") else 90), phase=phase,
                       issue=dict(number=1, title="Addition subtracts", body="add(2, 3) should equal 5",
                                  open=True, is_pr=False, updated_at="2026-11-20T00:00:00Z", comments=[]),
                       base=base, report=report, command=["python3", "test_calc.py"] if phase == "check" else [],
                       areas=[dict(label="area:arithmetic", description="Arithmetic implementation and regression",
                                   paths=["calc.py", "test_calc.py"], auto_fix=True)])
        input_path = stage / "input.json"
        input_path.write_text(json.dumps(request))
        plan = Plan(mode, stage / "model-requests.jsonl")
        self.server.plan = plan
        machine = "fiach-acp-test-" + uuid.uuid4().hex[:12]
        args = self.privilege + ["systemd-nspawn", f"--directory={root}", f"--machine={machine}",
                "--private-users=no", "--keep-unit", "--settings=no", "--register=no", "--no-new-privileges=yes",
                "--quiet", "--resolv-conf=off", "--link-journal=no", "--bind-ro=/nix/store",
                f"--bind-ro={input_path}:/input/request.json", f"--bind={output}:/output", f"--bind={repo}:/workspace",
                "--setenv=PATH=/bin", "--setenv=HOME=/tmp", "--setenv=XDG_STATE_HOME=/tmp/state",
                "--setenv=GIT_CONFIG_COUNT=1", "--setenv=GIT_CONFIG_KEY_0=safe.directory",
                "--setenv=GIT_CONFIG_VALUE_0=/workspace",
                "--setenv=SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"]
        args += ["--kill-signal=SIGINT"]
        if phase != "check":
            args += [f"--setenv=OPENAI_API_KEY={KEY}",
                     f"--setenv=OPENAI_HOST=http://127.0.0.1:{self.server.server_port}"]
        args += ["/bin/fiach", "issue-worker", "--input", "/input/request.json"]
        started = time.monotonic()
        with (stage / "worker.log").open("w") as log:
            process = subprocess.Popen(args, stdout=log, stderr=log, start_new_session=True,
                                       env={"PATH": os.environ["PATH"], "LANG": "C.UTF-8"})
            owned = []
            cleanup_failure = []
            try:
                if mode in ("cancel", "hard_cancel"):
                    heartbeat = repo / "heartbeat.log"
                    until = time.monotonic() + 45
                    while not heartbeat.exists() and time.monotonic() < until and process.poll() is None:
                        time.sleep(0.1)
                    assert heartbeat.exists(), f"Worker did not reach active tool cancellation point: {stage}"
                    before = heartbeat.stat().st_size
                    time.sleep(0.3)
                    assert heartbeat.stat().st_size > before, "Heartbeat tool was not running before cancellation"
                    owned = self.process_tree(process.pid)
                    spawner = next(member for member in owned if member[2] == "systemd-nspawn")
                    if mode == "hard_cancel":
                        init = next(member for member in owned if member[2] == ".fiach-wrapped")
                        self.signal_member(init, signal.SIGSTOP)
                    self.signal_member(spawner, signal.SIGTERM)
                    if mode == "hard_cancel":
                        time.sleep(5)
                        assert process.poll() is None, "Stopped init should require forced shutdown"
                        # Same native sequence as process::NspawnChild. Rust tests
                        # cover host wiring; this fixture tests real nspawn cleanup.
                        self.signal_member(spawner, signal.SIGTERM)
                code = process.wait(timeout=15 if mode in ("cancel", "hard_cancel") else 100)
            finally:
                plan.release.set()
                if process.poll() is None:
                    owned = self.process_tree(process.pid)
                    self.stop_group(process, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        self.stop_group(process, signal.SIGKILL)
                        process.wait(timeout=10)
                # Reap only previously observed descendants with matching start
                # times. Check before cleanup so leaked children fail the test.
                time.sleep(0.2)
                cleanup_failure = [member for member in owned if self.is_same_process(member)]
                for member in reversed(cleanup_failure):
                    self.signal_member(member, signal.SIGKILL)
        (stage / "cleanup.json").write_text(json.dumps(dict(observed=owned, survivors=cleanup_failure), indent=2) + "\n")
        assert not cleanup_failure, f"Container left live descendants: {cleanup_failure}"
        if mode in ("cancel", "hard_cancel"):
            size, calls = (repo / "heartbeat.log").stat().st_size, plan.requests
            time.sleep(0.5)
            assert (repo / "heartbeat.log").stat().st_size == size, "Tool descendant survived container exit"
            assert plan.requests == calls, "Model calls continued after cancellation"
        item = dict(stage=name, exit=code, elapsed=time.monotonic()-started,
                    work_model_calls=plan.work_calls, recovery_model_calls=plan.recovery_calls)
        self.summary.append(item)
        (self.artifacts / "summary.json").write_text(json.dumps(self.summary, indent=2) + "\n")
        print(json.dumps(item), flush=True)
        return code, output, plan

    def run(self):
        repo, base = self.repository("success-repo")
        code, output, plan = self.stage("code", repo, base, "success")
        assert code == 0, output
        candidate = json.loads((output / "report.json").read_text())
        assert candidate == CANDIDATE and plan.work_calls == 2 and plan.recovery_calls == 0
        assert (repo / "actions.log").read_text().splitlines() == ["work"]
        patch = output / "patch.diff"
        assert "calc.py" in patch.read_text() and "test_calc.py" in patch.read_text()
        baseline, baseline_base = self.repository("baseline-repo")
        command(["git", "-C", baseline, "apply", "--include=test_calc.py", patch])
        code, checked, check_plan = self.stage("baseline-check", baseline, baseline_base, "check", phase="check")
        assert code == 0 and not json.loads((checked / "check.json").read_text())["success"]
        assert check_plan.requests == 0
        patched, patched_base = self.repository("patched-repo")
        command(["git", "-C", patched, "apply", patch])
        command(["git", "-C", patched, "add", "-N", "test_calc.py"])
        code, checked, check_plan = self.stage("patched-check", patched, patched_base, "check", phase="check")
        assert code == 0 and json.loads((checked / "check.json").read_text())["success"]
        assert check_plan.requests == 0
        code, verified, plan = self.stage("verify", patched, patched_base, "verify", phase="verify", report=candidate)
        assert code == 0 and json.loads((verified / "report.json").read_text()) == VERIFIED
        assert plan.work_calls == 2 and (patched / "verifier.log").read_text().splitlines() == ["verify"]
        for mode in ["invalid", "turn_limit", "recovery_exhausted", "cancel", "hard_cancel", "disconnect"]:
            repo, base = self.repository(mode + "-repo")
            code, output, plan = self.stage(mode, repo, base, mode, max_turns=1 if mode == "turn_limit" else 8)
            assert (repo / "actions.log").read_text().splitlines() == ["work"], f"Work replayed in {mode}"
            assert not (output / "patch.diff").exists(), mode
            if mode in ("invalid", "turn_limit"):
                assert code == 0 and json.loads((output / "report.json").read_text()) == BLOCKED
                assert plan.recovery_calls == 2, mode
                assert not (repo / "FORBIDDEN_RECOVERY").exists(), "Recovery executed a forbidden workspace tool"
                assert plan.work_calls == (1 if mode == "turn_limit" else 2), mode
            else:
                assert code != 0 and not (output / "report.json").exists(), mode
                assert plan.recovery_calls == (2 if mode == "recovery_exhausted" else 0), mode
            transcript = output / "transcript.jsonl"
            assert transcript.exists() and transcript.stat().st_size > 0, mode
            if mode == "hard_cancel":
                assert "workspace__shell" in transcript.read_text(), "Hard kill lost streamed tool evidence"
                assert not any("completion" in json.loads(line) for line in transcript.read_text().splitlines()), "Hard kill fabricated terminal completion"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--rootfs", type=Path)
    source.add_argument("--module-config", type=Path)
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--sudo", action="store_true")
    args = parser.parse_args()
    if args.module_config:
        with args.module_config.open("rb") as stream:
            args.rootfs = Path(tomllib.load(stream)["issues"]["worker"]["rootfs"])
    rootfs = args.rootfs.resolve(strict=True)
    assert (rootfs / "bin/fiach").exists() and (rootfs / "bin/goose").exists(), rootfs
    artifacts = args.artifacts or Path(tempfile.mkdtemp(prefix="fiach-acp-e2e-", dir="/data/rust/tmp"))
    artifacts = artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    assert not any(artifacts.iterdir()), "Artifact directory must be empty"
    print(f"Artifacts: {artifacts}", flush=True)
    runner = Runner(rootfs, artifacts, args.sudo)
    try:
        runner.run()
        print("PASS: real packaged worker/Goose/container workflows", flush=True)
    finally:
        runner.close()


if __name__ == "__main__":
    main()
