"""Host smoke: drive a built Aelix Studio host binary over its real HTTP API.

The binary runs in local mode on an ephemeral loopback port with an isolated
HOME, data directory and workspace. A synthetic ACP agent (smoke_agent.py)
is registered through the ordinary custom-agent path, so the smoke covers
startup, the listening event, update_check_disabled_by_default (including its
stable error code), custom-agent probe, ACP client identity, assistant listing,
conversation creation, runtime warm-up, the legacy launch-only model
selector, rejection of an unavailable model, one prompt/reply turn with
persistence, idle runtime state and graceful shutdown (SIGTERM, or the
desktop parent-exit path on Windows).

Uses only the Python standard library and a loopback synthetic agent, without
real provider credentials. This functional smoke does not audit outbound
connection attempts; that is separate acceptance work.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from provenance import workspace_version  # noqa: E402

AGENT_NAME = 'Host smoke synthetic agent'
PROMPT = 'HOST_SMOKE_PROMPT'
REPLY = 'HOST_SMOKE_OK'
MODEL = 'fixture/one'


def http(port, route, method='GET', body=None, timeout=30):
    request = urllib.request.Request(
        'http://127.0.0.1:' + str(port) + route,
        data=None if body is None else json.dumps(body).encode(),
        method=method,
        headers={'Content-Type': 'application/json'},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as failure:
        try:
            return failure.code, json.loads(failure.read())
        except ValueError:
            return failure.code, {}


def listening_port(raw):
    """Return the port from the first complete AIONCORE_LISTENING line, if any."""
    complete = raw.rsplit(b'\n', 1)[0] if b'\n' in raw else b''
    for line in complete.splitlines():
        line = line.strip()
        if not line.startswith(b'AIONCORE_LISTENING '):
            continue
        value = json.loads(line[len(b'AIONCORE_LISTENING '):])
        if value.get('host') != '127.0.0.1' or type(value.get('port')) is not int:
            raise RuntimeError('Unexpected listening event: ' + line.decode(errors='replace'))
        return value['port']
    return None


def wait_until(predicate, timeout, message):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(message)
        time.sleep(0.1)


class Host:
    def __init__(self, binary, root):
        self.binary, self.root = binary, root
        self.log_path = root / 'host.log'
        self.process = self.parent = None
        self.port = None

    def start(self):
        home = self.root / 'home'
        workspace = self.root / 'workspace'
        for directory in (home, workspace, self.root / 'extensions'):
            directory.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ)
        env.pop('AELIX_STUDIO_HOST_UPDATE_CHECK', None)
        env.update(
            HOME=str(home),
            USERPROFILE=str(home),
            AIONUI_DISABLE_AUTO_UPDATE='1',
            AIONUI_EXTENSIONS_PATH=str(self.root / 'extensions'),
            AIONUI_EXTENSION_STATES_FILE=str(self.root / 'extension-states.json'),
        )
        argv = [str(self.binary), '--local', '--port', '0', '--data-dir', str(self.root / 'data'),
                '--managed-resources-mode', 'bundled', '--log-level', 'info']
        if os.name == 'nt':
            # Windows has no SIGTERM. Use the desktop's parent-exit path with a
            # stand-in parent; on Windows the host waits on any process handle.
            self.parent = subprocess.Popen([sys.executable, '-c', 'import time\nwhile True: time.sleep(60)'],
                                           stdin=subprocess.DEVNULL)
            argv += ['--parent-pid', str(self.parent.pid)]
        self.log = self.log_path.open('wb')
        options = {'start_new_session': True} if os.name != 'nt' else {
            'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP}
        self.process = subprocess.Popen(argv, cwd=workspace, env=env, stdin=subprocess.DEVNULL,
                                        stdout=self.log, stderr=subprocess.STDOUT, **options)

        def ready():
            if self.process.poll() is not None:
                raise RuntimeError('Host exited during startup with ' + str(self.process.returncode))
            port = listening_port(self.log_path.read_bytes())
            if port is None:
                return None
            try:
                code, _ = http(port, '/api/assistants', timeout=5)
            except OSError:
                return None
            if code != 200:
                raise RuntimeError('Unexpected startup status ' + str(code))
            return port

        self.port = wait_until(ready, 60, 'Host did not become ready')
        return self.port

    def api(self, route, method='GET', body=None, expected=(200, 201, 202)):
        code, value = http(self.port, route, method, body)
        if code not in expected:
            raise AssertionError(method + ' ' + route + ' returned ' + str(code) + ': ' + json.dumps(value)[:2000])
        return code, value.get('data', value) if isinstance(value, dict) else value

    def stop(self, timeout=30):
        """Request graceful shutdown; return the host exit code, or None if it had to be killed."""
        if self.parent is not None:
            self.parent.kill()
            self.parent.wait(10)
        else:
            self.process.send_signal(signal.SIGTERM)
        if self.process.poll() is None:
            try:
                self.process.wait(timeout)
            except subprocess.TimeoutExpired:
                self.kill()
                return None
        if os.name != 'nt':
            # Reap anything the host left in its process group (for example the agent).
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        self.log.close()
        return self.process.returncode

    def kill(self):
        if self.parent is not None and self.parent.poll() is None:
            self.parent.kill()
        if self.process is None:
            return
        if os.name == 'nt':
            subprocess.run(['taskkill', '/T', '/F', '/PID', str(self.process.pid)], capture_output=True)
        else:
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.process.wait(10)


def run(binary, python, keep, report):
    checks = report['checks']
    report['binarySha256'] = hashlib.sha256(binary.read_bytes()).hexdigest()
    report['version'] = subprocess.run([str(binary), '--version'], capture_output=True, text=True,
                                       timeout=30, check=True).stdout.strip()
    expected_version = 'aelix-studio-host ' + workspace_version()
    assert report['version'] == expected_version, ('--version', report['version'], expected_version)
    checks.append('version')
    if any(character.isspace() for character in python):
        # The host resolves a custom agent command by its first whitespace token.
        raise ValueError('The Python executable path must not contain whitespace')
    temporary = tempfile.mkdtemp(prefix='aioncore-smoke-')
    root = Path(temporary).resolve()
    host = Host(binary, root)
    try:
        host.start()
        checks.append('startup_listening_event')
        code, health = http(host.port, '/health')
        assert code == 200, ('/health', code, health)
        checks.append('health')
        code, disabled = http(host.port, '/api/system/check-update', 'POST', {'repo': 'iOfficeAI/AionUi'})
        assert code == 400, ('disabled update check', code, disabled)
        assert disabled.get('code') == 'UPDATE_CHECK_DISABLED', disabled
        assert disabled.get('error') == 'Update checks are disabled; set AELIX_STUDIO_HOST_UPDATE_CHECK=1 to enable them', disabled
        checks.append('update_check_disabled_by_default')
        host.api('/api/agents/custom', 'POST', {
            'name': AGENT_NAME, 'command': python, 'args': ['-I', '-B', str(HERE / 'smoke_agent.py')], 'env': [],
        })
        checks.append('custom_agent_registration')
        _, assistants = host.api('/api/assistants')
        assistant = next(item for item in assistants if item['name'] == AGENT_NAME)
        checks.append('custom_agent_assistant')
        _, conversation = host.api('/api/conversations', 'POST', {
            'name': 'Host smoke', 'assistant': {'id': assistant['id']},
            'extra': {'workspace': str(root / 'workspace'), 'custom_workspace': True},
        })
        route = '/api/conversations/' + conversation['id']
        checks.append('conversation_create')
        _, observed = host.api(route + '/runtime/ensure', 'POST')
        model = next(item for item in observed.get('config_options') or [] if item['id'] == 'model')
        assert model['current_value'] == MODEL, model
        assert len(model['options']) == 1, model
        checks.append('runtime_ensure_model_selector')
        checks.append('acp_client_identity')
        code, rejection = http(host.port, route + '/config-options/model', 'PUT', {'value': 'fixture/two'})
        # Raised by the ACP agent manager for a value outside the advertised options.
        assert (code, rejection.get('code'), rejection.get('error')) == (
            400, 'BAD_REQUEST', "Value 'fixture/two' is not selectable for config option 'model'"), (code, rejection)
        checks.append('unavailable_model_rejected')
        host.api(route + '/messages', 'POST', {'content': PROMPT})

        def replied():
            _, messages = host.api(route + '/messages')
            return REPLY in json.dumps(messages)

        wait_until(replied, 60, 'Agent reply was not persisted')
        checks.append('prompt_reply_persisted')

        def idle():
            _, current = host.api(route)
            runtime = current.get('runtime') or {}
            return (runtime.get('state') == 'idle' and runtime.get('can_send_message') is True
                    and runtime.get('is_processing') is False and runtime.get('pending_confirmations') == 0)

        wait_until(idle, 30, 'Runtime did not return to idle')
        checks.append('runtime_idle_after_turn')
        report['exitCode'] = host.stop()
        assert report['exitCode'] == 0, 'Host did not shut down cleanly'
        checks.append('graceful_shutdown')
        report['status'] = 'passed'
    except BaseException as failure:
        report['error'] = repr(failure)
        if host.log_path.exists():
            report['hostLogTail'] = host.log_path.read_bytes()[-6000:].decode(errors='replace')
        raise
    finally:
        if (host.process is not None and host.process.poll() is None) or (
                host.parent is not None and host.parent.poll() is None):
            host.kill()
        if getattr(host, 'log', None) is not None:
            host.log.close()
        if not keep:
            shutil.rmtree(root, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--python', default=sys.executable, help='Interpreter for the synthetic agent')
    parser.add_argument('--report', type=Path, help='Write the JSON report here')
    parser.add_argument('--keep', action='store_true', help='Keep the temporary root for inspection')
    args = parser.parse_args()
    report = {'schema': 'aelix-studio-host.smoke/1', 'status': 'failed', 'checks': []}
    try:
        run(args.binary.resolve(), os.path.abspath(args.python), args.keep, report)
    finally:
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: report.get(key) for key in ('status', 'checks', 'version')}))


if __name__ == '__main__':
    main()
