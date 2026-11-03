"""Real npm/npx mirror integration: serve a synthetic ACP package on loopback.

Requires a separately prepared Node 24.11.0 tree (--node-root); never downloads
fixtures itself. Linux CI audits the host, actual npm and this local registry.
No corporate registry, provider credentials or real third-party agents are used.
"""
import argparse
import base64
import gzip
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import threading

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
from smoke import Host, PROMPT, REPLY, wait_until  # noqa: E402

PACKAGE = '@aelix-test/mirror-agent'
VERSION = '1.0.0'


def package_archive():
    body = b'#!/usr/bin/env python3\n' + (HERE / 'smoke_agent.py').read_bytes()
    with io.BytesIO() as output:
        with tarfile.open(fileobj=output, mode='w') as archive:
            files = {'package/agent.py': body, 'package/package.json': json.dumps({
                'name': PACKAGE, 'version': VERSION, 'bin': {'mirror-agent': 'agent.py'},
            }).encode()}
            for name, data in files.items():
                info = tarfile.TarInfo(name)
                info.size, info.mode = len(data), 0o755
                archive.addfile(info, io.BytesIO(data))
        return gzip.compress(output.getvalue(), mtime=0)


def run(binary, node_root, report, node_archive=None):
    archive = package_archive()
    requests = []

    class Registry(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            requests.append(self.path)
            self.send_error(405)

        def do_GET(self):
            requests.append(self.path)
            if node_archive and self.path == '/node/v24.11.0/' + node_archive.name:
                body = node_archive.read_bytes()
            elif self.path.lower() == '/npm/@aelix-test%2fmirror-agent':
                base = f'http://127.0.0.1:{self.server.server_port}'
                version = {'name': PACKAGE, 'version': VERSION, 'bin': {'mirror-agent': 'agent.py'},
                           'dist': {'tarball': base + '/npm/agent.tgz',
                                    'integrity': 'sha512-' + base64.b64encode(hashlib.sha512(archive).digest()).decode()}}
                body = json.dumps({'name': PACKAGE, 'dist-tags': {'latest': VERSION},
                                   'versions': {VERSION: version}}).encode()
            elif self.path == '/npm/agent.tgz':
                body = archive
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Content-Type', 'application/octet-stream' if self.path.endswith('.tgz') else 'application/json')
            self.end_headers()
            self.wfile.write(body)

    with ThreadingHTTPServer(('127.0.0.1', 0), Registry) as registry, tempfile.TemporaryDirectory(prefix='host-mirror-') as root:
        thread = threading.Thread(target=registry.serve_forever, daemon=True)
        thread.start()
        launchers = list(node_root.glob('node/node-v24.11.0-*/bin/npx'))
        assert len(launchers) == 1, 'Node fixture must contain exactly one npm/npx runtime'
        explicit_npx = launchers[0]
        host_env = {
            'AELIX_STUDIO_HOST_NPM_REGISTRY': f'http://127.0.0.1:{registry.server_port}/npm/',
            'AIONUI_BUNDLED_MANAGED_RESOURCES': str(node_root),
            'PATH': str(explicit_npx.parent) + os.pathsep + os.environ.get('PATH', ''),
        }
        if node_archive: host_env['AELIX_STUDIO_HOST_NODE_MIRROR'] = f'http://127.0.0.1:{registry.server_port}/node/'
        host = Host(binary, Path(root), env_overrides=host_env,
                    extra_args=('--managed-resources-mode', 'download' if node_archive else 'bundled'))
        try:
            host.start()
            if node_archive:
                _, prepared = host.api('/api/system/ensure-node-runtime', 'POST', {'scope':{'kind':'custom_agent','id':'node-checksum-fixture'}})
                assert prepared.get('ready') is True, prepared
            # npx must obtain this never-published package from the configured
            # prefix, through the ordinary custom-agent/session launch path.
            for label, command in [('Mirror smoke', 'npx'), ('Mirror absolute', str(explicit_npx))]:
                host.api('/api/agents/custom', 'POST', {
                    'name': label, 'command': command, 'args': ['-y', PACKAGE + '@' + VERSION], 'env': [],
                })
                _, assistants = host.api('/api/assistants')
                assistant = next(item for item in assistants if item['name'] == label)
                _, conversation = host.api('/api/conversations', 'POST', {
                    'name': 'Mirror smoke', 'assistant': {'id': assistant['id']},
                    'extra': {'workspace': str(Path(root) / 'workspace'), 'custom_workspace': True},
                })
                route = '/api/conversations/' + conversation['id']
                host.api(route + '/runtime/ensure', 'POST')
                host.api(route + '/messages', 'POST', {'content': PROMPT})
                wait_until(lambda: REPLY in json.dumps(host.api(route + '/messages')[1]), 60, 'Mirror agent reply missing')
            assert '/npm/agent.tgz' in requests, requests
            assert any(path.lower() == '/npm/@aelix-test%2fmirror-agent' for path in requests), requests
            assert all(path.lower() in ('/npm/@aelix-test%2fmirror-agent', '/npm/agent.tgz') or (node_archive and path == '/node/v24.11.0/' + node_archive.name) for path in requests), requests
            if node_archive: assert '/node/v24.11.0/' + node_archive.name in requests, requests
            assert host.stop() == 0
            report.update(status='passed', registryRequests=requests,
                          checks=['real_npx_registry_prefix', 'real_npm_tarball', 'real_npx_explicit_path', 'acp_prompt_reply', 'graceful_shutdown'] + (['official_node_archive_verified'] if node_archive else []))
        except BaseException:
            if host.log_path.exists():
                report['hostLogTail'] = host.log_path.read_text(errors='replace')[-6000:]
            raise
        finally:
            if host.process is not None and host.process.poll() is None:
                host.kill()
            registry.shutdown()
            thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--node-archive', type=Path, help='Official unchanged Node archive served by the loopback mirror')
    parser.add_argument('--node-root', type=Path, required=True, help='Bundled resources root containing node/node-v24.11.0-<platform>')
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args()
    report = {'status': 'failed'}
    try:
        run(args.binary.resolve(), args.node_root.resolve(), report, args.node_archive.resolve() if args.node_archive else None)
    finally:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps({'status': report['status'], 'checks': report.get('checks', [])}))


if __name__ == '__main__':
    main()
