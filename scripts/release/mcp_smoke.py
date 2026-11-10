"""Loopback MCP coverage for the default host smoke on every release target."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading


def check_mcp_http(host):
    methods = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            method = request['method']
            methods.append((self.path, method))
            assert self.headers['x-host23-fixture'] == 'loopback'
            if method != 'initialize':
                assert self.headers['mcp-session-id'] == 'host23-session'
            if method == 'notifications/initialized':
                self.send_response(202)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            reply = {'jsonrpc': '2.0', 'id': request['id']}
            if method == 'initialize':
                reply['result'] = {
                    'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}},
                    'serverInfo': {'name': 'host23-fixture', 'version': '1'},
                }
            elif self.path == '/error':
                reply['error'] = {'code': -32603, 'message': 'fixture failure'}
            else:
                assert method == 'tools/list'
                reply['result'] = {'tools': [{
                    'name': 'host23_tool', 'description': 'Loopback fixture',
                    'inputSchema': {'type': 'object'},
                }]}
            body = json.dumps(reply)
            content_type = 'application/json'
            if self.path == '/sse':
                content_type = 'text/event-stream'
                body = 'event: message\ndata: ' + body + '\n\n'
            payload = body.encode()
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(payload)))
            self.send_header('mcp-session-id', 'host23-session')
            self.end_headers()
            self.wfile.write(payload)

    with ThreadingHTTPServer(('127.0.0.1', 0), Handler) as fixture:
        thread = threading.Thread(target=fixture.serve_forever, daemon=True)
        thread.start()
        try:
            for path in ('/json', '/sse', '/error'):
                body = {'name': 'host23-loopback', 'transport': {
                    'type': 'http', 'url': f'http://127.0.0.1:{fixture.server_port}{path}',
                    'headers': {'x-host23-fixture': 'loopback'},
                }}
                if path == '/error':
                    code, result = host.api('/api/mcp/test-connection', 'POST', body, expected=(502,))
                    assert code == 502 and result['code'] == 'MCP_RPC_ERROR', result
                    assert result['details'] == {'method': 'tools/list', 'rpc_code': -32603}, result
                else:
                    _, result = host.api('/api/mcp/test-connection', 'POST', body)
                    assert result['success'] is True, result
                    assert len(result['tools']) == 1, result
                    assert result['tools'][0]['name'] == 'host23_tool', result
                assert [method for seen, method in methods if seen == path] == [
                    'initialize', 'notifications/initialized', 'tools/list'], methods
        finally:
            fixture.shutdown()
            thread.join(timeout=5)
