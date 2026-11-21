#!/usr/bin/env python3
"""Real issue scheduler, broker, nspawn and Goose; only GitHub/models are fixtures.

Run through nix develop on a Linux container-capable host. --sudo elevates only
our Fiach child and signals to its observed processes. All credentials are fake.
"""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

import worker_acp_e2e as worker


class Plans:
    def __init__(self, mode, directory):
        self.code = worker.Plan({'reject': 'success', 'lost_response': 'success', 'timeout': 'cancel'}.get(mode, mode), directory / 'coder-requests.jsonl')
        self.verify = worker.Plan('verify', directory / 'verifier-requests.jsonl')
        self.mode = mode
        self.requests = 0
        self.actions = {'code': 0, 'verify': 0}
        self.jev_requests = []
        self.errors = []

    def answer(self, body):
        self.requests += 1
        verify = body['model'] == 'gpt-4o'
        if (any(tool['function']['name'] == 'workspace__shell' for tool in body.get('tools', []))
                and not any(message.get('role') == 'tool' for message in body.get('messages', []))):
            self.actions['verify' if verify else 'code'] += 1
        answer = (self.verify if verify else self.code).answer(body)
        if answer.get('name') == 'workspace__shell' and 'command' in answer.get('arguments', {}):
            probe = ("import os; key=os.environ.get('OPENAI_API_KEY',''); "
                     "assert key.startswith('fiach-sandbox-'); "
                     "assert not os.environ.get('GITHUB_TOKEN'); "
                     "print('broker credential isolation verified')")
            answer['arguments']['command'] = 'set -eu\npython3 -c ' + worker.shlex.quote(probe) + '\n' + answer['arguments']['command']
        if verify and self.mode == 'reject' and answer.get('name') == 'final_output':
            answer['arguments'] = dict(worker.BLOCKED, summary='Fixture verifier rejects the proposed fix')
        return answer

    def __getattr__(self, name):
        return getattr(self.code, name)


class Model(worker.Model):
    def do_POST(self):
        try:
            if self.path != '/v1/systemone':
                assert self.path == '/v1/chat/completions', self.path
                return super().do_POST()
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.plan.jev_requests.append(body)
            answers = {}
            for name, question in body['questions'].items():
                selected = {'kind': 'bug', 'information': 'sufficient', 'direction': 'established',
                            'match': 'different', 'relevance': 'different'}.get(name, 'yes')
                assert selected in question['criteria'], (name, question)
                answers[name] = dict(type='choice', choice=selected, confidence=1.0,
                                     probabilities={key: float(key == selected) for key in question['criteria']})
            payload = json.dumps(dict(model='jev-1.13.0', answers=answers,
                                      usage=dict(input_tokens=100, output_tokens=10))).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception as error:
            self.server.plan.errors.append(repr(error))
            self.close_connection = True


def wait_for(predicate, process, seconds=90):
    deadline = time.monotonic() + seconds
    while not predicate():
        assert process.poll() is None, 'Fiach exited before reaching expected workflow state'
        assert time.monotonic() < deadline, 'Workflow timed out'
        time.sleep(0.1)


def read_json(path):
    return json.loads(path.read_text())


class Runner(worker.Runner):
    def __init__(self, rootfs, artifacts, sudo, timeout_scale=1):
        self.rootfs, self.artifacts = rootfs, artifacts
        self.timeout_scale = timeout_scale
        self.privilege = ['sudo', '-n'] if sudo else []
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.summary = []

    def setup(self, mode):
        directory = self.artifacts / mode
        directory.mkdir()
        repo, base = self.repository(mode + '-remote')
        worker.command(['git', '-C', repo, 'branch', '-M', 'main'])
        (repo / '.gitignore').write_text('actions.log\nverifier.log\nheartbeat.log\n__pycache__/\n')
        worker.command(['git', '-C', repo, 'add', '.gitignore'])
        worker.command(['git', '-C', repo, 'commit', '-qm', 'Ignore fixture instrumentation'])
        base = worker.command(['git', '-C', repo, 'rev-parse', 'HEAD']).strip()
        bindir = directory / 'bin'
        bindir.mkdir()
        gh = bindir / 'gh'
        gh.write_text((Path(__file__).resolve().parents[1] / 'fixtures/github.py').read_text()
                      .replace('/usr/bin/env python3', sys.executable))
        gh.chmod(0o755)
        issue = dict(number=1, title='Addition subtracts its second argument',
                     body='calc.add(2, 3) returns -1; expected 5. Fix addition and add a regression test.',
                     state='open', updated_at='original', labels=[dict(name='human-label')])
        (directory / 'fixture.json').write_text(json.dumps(dict(items=[issue], labels=['human-label'],
                                                               comments={}, remote=str(repo), lose_pr_response=mode == 'lost_response')))
        address = f'http://127.0.0.1:{self.server.server_port}'
        (directory / 'fiach.toml').write_text(f'''
[issues]
publish = true
# All publication goes to the fake gh executable and a local Git remote.
auto_fix = true
interval_secs = 1
state_path = "{directory / 'state.redb'}"
scratch_dir = "{directory}"
max_items = 1
jev_base_url = "{address}"
max_jev_cost_usd = 0.01
[[issues.repos]]
repo = "owner/repo"
[[issues.repos.areas]]
label = "area:calculator"
description = "Integer arithmetic and regression tests"
paths = ["calc.py", "test_calc.py"]
auto_fix = true
[issues.worker]
rootfs = "{self.rootfs}"
provider = "openai"
model = "gpt-4o-mini"
verifier_provider = "openai"
verifier_model = "gpt-4o"
network = "host"
max_turns = 8
timeout_secs = {(5 if mode == "timeout" else 60) * self.timeout_scale}
''')
        (directory / 'gitconfig').write_text('[safe]\n\tdirectory = *\n[core]\n\thooksPath = /dev/null\n')
        env = dict(PATH=f'{bindir}:{os.environ["PATH"]}', HOME=str(directory), LANG='C.UTF-8',
                   FIXTURE=str(directory / 'fixture.json'), TYPESAFE_API_KEY='fixture-only',
                   OPENAI_API_KEY=worker.KEY, OPENAI_HOST=address, RUST_LOG='info',
                   GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=str(directory / 'gitconfig'),
                   GIT_CONFIG_COUNT='2', GIT_CONFIG_KEY_0='safe.directory', GIT_CONFIG_VALUE_0='*',
                   GIT_CONFIG_KEY_1='core.hooksPath', GIT_CONFIG_VALUE_1='/dev/null')
        self.server.plan = Plans(mode, directory)
        return directory, repo, base, env

    def launch(self, directory, env, label, watch=False, cancel=False, timeout=False):
        log_path = directory / f'{label}.log'
        args = self.privilege + ['env', '-i'] + [f'{key}={value}' for key, value in env.items()]
        args += [str(self.rootfs / 'bin/fiach'), '--config', str(directory / 'fiach.toml'), 'issues']
        if watch:
            args += ['--watch']
        owned = []
        with log_path.open('w') as log:
            process = subprocess.Popen(args, cwd=directory, stdout=log, stderr=log, start_new_session=True,
                                       env={'PATH': os.environ['PATH'], 'LANG': 'C.UTF-8'})
            try:
                if cancel or timeout:
                    wait_for(lambda: any(directory.rglob('heartbeat.log')), process, 90 * self.timeout_scale)
                    heartbeat = next(directory.rglob('heartbeat.log'))
                    size = heartbeat.stat().st_size
                    time.sleep(0.3 * self.timeout_scale)
                    assert heartbeat.stat().st_size > size
                    owned = self.process_tree(process.pid)
                elif watch:
                    wait_for(lambda: log_path.read_text().count('Issue polling cycle complete') >= 2, process, 90 * self.timeout_scale)
                if watch or cancel:
                    owned = self.process_tree(process.pid)
                    host = next(member for member in owned if member[2] == '.fiach-wrapped')
                    self.signal_member(host, signal.SIGINT)
                code = process.wait(timeout=90 * self.timeout_scale)
                assert code == 0, f'Fiach failed: {log_path}'
            finally:
                self.server.plan.release.set()
                if process.poll() is None:
                    owned = self.process_tree(process.pid)
                    hosts = [member for member in owned if member[2] == '.fiach-wrapped']
                    if hosts:
                        self.signal_member(hosts[0], signal.SIGINT)
                    with contextlib.suppress(subprocess.TimeoutExpired):
                        process.wait(timeout=15 * self.timeout_scale)
                deadline = time.monotonic() + 15 * self.timeout_scale
                while any(self.is_same_process(member) for member in owned) and time.monotonic() < deadline:
                    time.sleep(0.1)
                survivors = [member for member in owned if self.is_same_process(member)]
                for member in reversed(survivors):
                    self.signal_member(member, signal.SIGKILL)
                (directory / f'{label}-cleanup.json').write_text(json.dumps(dict(observed=owned, survivors=survivors), indent=2))
                assert not survivors, f'Processes leaked: {survivors}'
        assert not self.server.plan.errors, self.server.plan.errors

    def scenario(self, mode):
        directory, repo, base, env = self.setup(mode)
        self.launch(directory, env, 'first', watch=mode in ('success', 'lost_response'),
                    cancel=mode == 'cancel', timeout=mode == 'timeout')
        plan = self.server.plan
        contexts = sorted(directory.glob('fiach-artifacts-*/context.json'), key=lambda path: path.stat().st_mtime_ns)
        phases = [read_json(path)['phase'] for path in contexts]
        state = read_json(directory / 'fixture.json')
        prs = state.get('prs', [])
        assert len(prs) == (1 if mode in ('success', 'lost_response') else 0), state
        assert state['items'][0]['state'] == 'open'
        assert plan.jev_requests, 'Real triage adapter never reached the local endpoint'
        assert plan.code.work_calls >= 1, 'Real worker never reached the broker/provider'
        brokers = [path / 'requests.jsonl' for path in directory.glob('*.broker')]
        assert len(brokers) == (2 if mode in ('success', 'lost_response', 'reject') else 1), brokers
        broker_records = [json.loads(line) for path in brokers
                          for line in worker.command(self.privilege + ['cat', path]).splitlines()]
        assert broker_records and all(row['provider'] == 'openai' for row in broker_records), broker_records
        assert any(row['path'].endswith('/chat/completions') and row['status'] == 200 for row in broker_records)
        assert sum(row['method'] == 'POST' for row in broker_records) == plan.requests, 'Model calls bypassed the broker'
        (directory / 'broker-requests.json').write_text(json.dumps(broker_records, indent=2) + '\n')
        if mode in ('success', 'lost_response', 'reject'):
            assert phases == ['code', 'check', 'check', 'verify'], phases
            checks = [read_json(path.parent / 'check.json')['success'] for path in contexts if read_json(path)['phase'] == 'check']
            assert checks == [False, True], checks
            assert plan.code.work_calls == plan.verify.work_calls == 2
            report = read_json(contexts[-1].parent / 'report.json')
            assert report['approved'] == (mode in ('success', 'lost_response'))
        else:
            assert phases == ['code'], phases
        if mode in ('success', 'lost_response'):
            assert prs[0]['draft'] is True and prs[0]['head'] == 'fiach/issue-1'
            assert worker.command(['git', '-C', repo, 'rev-parse', 'main']).strip() == base
            assert worker.command(['git', '-C', repo, 'show', 'fiach/issue-1:calc.py']) == 'def add(a, b):\n    return a + b\n'
            assert 'assert add(2, 3) == 5' in worker.command(['git', '-C', repo, 'show', 'fiach/issue-1:test_calc.py'])
        else:
            assert worker.command(['git', '-C', repo, 'branch', '--list', 'fiach/*']).strip() == ''
        if mode in ('cancel', 'timeout', 'disconnect'):
            assert plan.code.recovery_calls == 0

            assert not (contexts[0].parent / 'report.json').exists()
            assert (contexts[0].parent / 'transcript.jsonl').stat().st_size > 0
        assert plan.actions == {'code': 1, 'verify': int(mode in ('success', 'lost_response', 'reject'))}, plan.actions
        if mode == 'invalid':
            assert plan.code.recovery_calls == 2
            assert read_json(contexts[0].parent / 'report.json')['status'] == 'needs_review'
            assert not list(directory.rglob('FORBIDDEN_RECOVERY'))
        heartbeat_paths = list(directory.rglob('heartbeat.log'))
        heartbeats = {str(path): path.stat().st_size for path in heartbeat_paths}
        before = (plan.requests, len(contexts))
        self.launch(directory, env, 'restart')
        assert (plan.requests, len(list(directory.glob('fiach-artifacts-*/context.json')))) == before, 'Restart replayed work'
        assert len(read_json(directory / 'fixture.json').get('prs', [])) == len(prs), 'Duplicate publication'
        assert all(Path(path).stat().st_size == size for path, size in heartbeats.items()), 'Tool survived shutdown'
        calls = [json.loads(line) for line in (directory / 'fixture.json.calls').read_text().splitlines()]
        creates = [call for call in calls if call[:4] == ['api', '--method', 'POST', 'repos/owner/repo/pulls']]
        assert len(creates) == len(prs), creates
        writes_path = directory / 'fixture.json.writes'
        writes = [json.loads(line) for line in writes_path.read_text().splitlines()] if writes_path.exists() else []
        for write in writes:
            endpoint = write['endpoint']
            assert endpoint in ('repos/owner/repo/labels', 'repos/owner/repo/issues/1/labels',
                                'repos/owner/repo/issues/1/comments', 'repos/owner/repo/pulls') or endpoint.startswith(
                                    ('repos/owner/repo/issues/1/labels/', 'repos/owner/repo/issues/comments/')), write
        pr_writes = [write['body'] for write in writes if write['endpoint'] == 'repos/owner/repo/pulls']
        assert len(pr_writes) == len(prs)
        for body, pr in zip(pr_writes, prs):
            assert set(body) == {'head', 'base', 'title', 'body', 'draft'}, body
            assert all(pr[key] == value for key, value in body.items())
        if mode == 'lost_response':
            assert read_json(directory / 'fixture.json')['lose_pr_response'] is False
        result = dict(scenario=mode, phases=phases, draft_prs=len(prs), publication_attempts=len(creates),
                      coder_calls=plan.code.work_calls, verifier_calls=plan.verify.work_calls,
                      recovery_calls=plan.code.recovery_calls, work_actions=plan.actions, restart_replayed=False, broker_logs=len(brokers))
        self.summary.append(result)
        (self.artifacts / 'summary.json').write_text(json.dumps(self.summary, indent=2) + '\n')
        (directory / 'jev-requests.json').write_text(json.dumps(plan.jev_requests, indent=2) + '\n')
        print(json.dumps(result), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--module-config', type=Path, required=True)
    parser.add_argument('--artifacts', type=Path)
    parser.add_argument('--sudo', action='store_true')
    parser.add_argument('--timeout-scale', type=int, default=1, help='Multiply fixture time budgets for slow VM/CI hosts')
    parser.add_argument('--scenario', choices=['success', 'lost_response', 'reject', 'invalid', 'disconnect', 'cancel', 'timeout'])
    args = parser.parse_args()
    if args.timeout_scale < 1:
        parser.error('--timeout-scale must be positive')
    with args.module_config.open('rb') as stream:
        rootfs = Path(tomllib.load(stream)['issues']['worker']['rootfs'])
    artifacts = args.artifacts or Path(tempfile.mkdtemp(prefix='fiach-flow-', dir='/data/rust/tmp'))
    artifacts = artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    assert not any(artifacts.iterdir()), 'Artifact directory must be empty'
    assert len(str(artifacts / 'lost_response/fiach-artifacts-XXXXXX.broker/sockets/openai.sock').encode()) < 108, 'Artifact path is too long for Linux Unix sockets'
    print(f'Artifacts: {artifacts}', flush=True)
    runner = Runner(rootfs, artifacts, args.sudo, args.timeout_scale)
    try:
        for scenario in [args.scenario] if args.scenario else ['success', 'lost_response', 'reject', 'invalid', 'disconnect', 'cancel', 'timeout']:
            runner.scenario(scenario)
        print('PASS: complete issue ACP workflows with local services', flush=True)
    finally:
        runner.close()


if __name__ == '__main__':
    main()
