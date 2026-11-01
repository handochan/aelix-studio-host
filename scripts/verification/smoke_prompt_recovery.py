"""Host HTTP -> ACP subprocess -> loopback model fault regression (POSIX).

The ACP peer and model are synthetic. A model request is held with no output;
only then does the harness kill the ACP process or break its stdout. The
pre-dispatch control closes an established connection before the next prompt.
An initial session/new failure also checks that build errors retain their
existing behavior (terminal error before any prompt/model request).
No provider credentials or real agent CLI behavior are involved.
"""

import argparse
import contextlib
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'release'))
from smoke import Host, wait_until  # noqa: E402


def record(root, **entry):
    with (root / 'wire.jsonl').open('a') as stream:
        stream.write(json.dumps(entry) + '\n')


def peer(root, model_url):
    def close_stdout(_signal, _frame):
        os.close(1)

    def invalid_stdout(_signal, _frame):
        os.write(1, b'\xff\n')

    signal.signal(signal.SIGUSR1, close_stdout)
    signal.signal(signal.SIGUSR2, invalid_stdout)
    for line in sys.stdin:
        request = json.loads(line)
        method, request_id = request.get('method'), request.get('id')
        if not method or request_id is None:
            continue
        record(root, method=method, pid=os.getpid())
        response = {'jsonrpc': '2.0', 'id': request_id}
        if method == 'initialize':
            response['result'] = {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True}, 'authMethods': []}
        elif method in ('session/new', 'session/load'):
            fail = root / 'fail-before-prompt'
            if fail.exists():
                fail.unlink()
                response['error'] = {'code': -32603, 'message': 'Synthetic setup failure before prompt'}
            else:
                response['result'] = {'sessionId': 'fixture-session'} if method == 'session/new' else {}
        elif method == 'session/prompt':
            (root / 'prompt-pid').write_text(str(os.getpid()))
            urllib.request.urlopen(urllib.request.Request(model_url, data=b'fixture'), timeout=60).close()
            response['result'] = {'stopReason': 'end_turn'}
        else:
            response['error'] = {'code': -32601, 'message': 'Method not found'}
        try:
            print(json.dumps(response), flush=True)
        except OSError:
            return


class Model(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers['Content-Length']))
        with self.server.lock:
            self.server.requests += 1
        self.server.entered.set()
        self.server.release.wait(60)
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()

    def log_message(self, *_args):
        pass


def scenario(binary, mode, output):
    root = Path(tempfile.mkdtemp(prefix='host-prompt-recovery-')).resolve()
    host = Host(binary, root)
    model = ThreadingHTTPServer(('127.0.0.1', 0), Model)
    model.requests, model.lock = 0, threading.Lock()
    model.entered, model.release = threading.Event(), threading.Event()
    threading.Thread(target=model.serve_forever, daemon=True).start()
    observed = {'scenario': mode, 'root': str(root)}
    try:
        host.start()
        name = 'Prompt recovery ' + mode
        host.api('/api/agents/custom', 'POST', {
            'name': name, 'command': sys.executable,
            'args': ['-I', '-B', str(Path(__file__).resolve()), 'peer', str(root),
                     'http://127.0.0.1:' + str(model.server_port)], 'env': [],
        })
        # Registration probes the peer with a separate session. Count only
        # this conversation's setup and turn when checking recovery.
        mark = len((root / 'wire.jsonl').read_text().splitlines())
        _, assistants = host.api('/api/assistants')
        assistant = next(item for item in assistants if item['name'] == name)
        _, conversation = host.api('/api/conversations', 'POST', {
            'name': name, 'assistant': {'id': assistant['id']},
            'extra': {'workspace': str(root / 'workspace'), 'custom_workspace': True},
        })
        route = '/api/conversations/' + conversation['id']
        before_dispatch = mode in ('setup-failure', 'before-connection-loss')
        if mode == 'setup-failure':
            (root / 'fail-before-prompt').touch()
            model.release.set()
        else:
            host.api(route + '/runtime/ensure', 'POST')
            if mode == 'before-connection-loss':
                frames = [json.loads(line) for line in (root / 'wire.jsonl').read_text().splitlines()]
                os.kill(frames[-1]['pid'], signal.SIGKILL)
                time.sleep(0.3)  # Let the host observe EOF before dispatch.
                model.release.set()
        host.api(route + '/messages', 'POST', {'content': 'RECOVERY_FIXTURE'})
        if mode != 'setup-failure':
            assert model.entered.wait(30), 'Model request did not start'
        if not before_dispatch:
            pid = int((root / 'prompt-pid').read_text())
            fault = {'process-kill': signal.SIGKILL, 'stdout-eof': signal.SIGUSR1,
                     'stdout-read-error': signal.SIGUSR2}[mode]
            os.kill(pid, fault)
            model.release.set()

        def idle():
            _, current = host.api(route)
            runtime = current.get('runtime') or {}
            return runtime.get('state') == 'idle' and not runtime.get('is_processing') and runtime.get('turn_id') is None

        wait_until(idle, 40, 'Turn did not terminate after the injected fault')
        # Releasing the model also allows an accidentally replayed request to
        # finish. Observe again after idle to catch late background dispatch.
        model.release.set()
        time.sleep(0.5)
        _, messages = host.api(route + '/messages')
        frames = [json.loads(line) for line in (root / 'wire.jsonl').read_text().splitlines()[mark:]]
        methods = [frame['method'] for frame in frames]
        observed.update(modelRequestsForTurn=model.requests, promptRequests=methods.count('session/prompt'),
                        sessionOpenRequests=sum(methods.count(m) for m in ('session/new', 'session/load')),
                        methods=methods)
        if mode == 'setup-failure':
            assert model.requests == 0, observed
            assert observed['promptRequests'] == 0, observed
            assert observed['sessionOpenRequests'] == 1, observed
            assert 'UNKNOWN_UPSTREAM_ERROR' in json.dumps(messages), messages
            assert 'conversation turn auto replay starting' not in host.log_path.read_text(), observed
        else:
            assert model.requests == 1, observed
            assert observed['promptRequests'] == 1, observed
        if mode == 'before-connection-loss':
            assert observed['sessionOpenRequests'] == 2, observed
            assert 'conversation turn auto replay completed' in host.log_path.read_text(), observed
        elif not before_dispatch:
            encoded = json.dumps(messages)
            assert 'USER_AGENT_PROMPT_OUTCOME_UNKNOWN' in encoded, messages
            assert '"retryable": false' in encoded, messages
            assert observed['sessionOpenRequests'] == 1, observed
            assert 'conversation turn auto replay starting' not in host.log_path.read_text(), observed
        observed['status'] = 'passed'
        return observed
    finally:
        model.release.set()
        model.shutdown()
        model.server_close()
        if host.process is not None:
            host.stop()
            host.log.close()
        destination = output / mode
        destination.mkdir(parents=True, exist_ok=True)
        for name in ('host.log', 'wire.jsonl'):
            if (root / name).exists():
                shutil.copy2(root / name, destination / name)
        (destination / 'observed.json').write_text(json.dumps(observed, indent=2) + '\n')
        shutil.rmtree(root)


def main():
    if len(sys.argv) == 4 and sys.argv[1] == 'peer':
        peer(Path(sys.argv[2]), sys.argv[3])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--scenario', action='append', help='Run only selected scenarios')
    args = parser.parse_args()
    report = {'schema': 'aelix-studio-host.prompt-recovery/1', 'evidence': 'synthetic ACP and loopback model',
              'binarySha256': hashlib.sha256(args.binary.read_bytes()).hexdigest(), 'scenarios': []}
    try:
        for mode in args.scenario or ('process-kill', 'stdout-eof', 'stdout-read-error', 'setup-failure', 'before-connection-loss'):
            report['scenarios'].append(scenario(args.binary.resolve(), mode, args.output))
        report['status'] = 'passed'
    finally:
        args.output.mkdir(parents=True, exist_ok=True)
        (args.output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
