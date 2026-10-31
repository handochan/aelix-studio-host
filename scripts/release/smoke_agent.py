"""Minimal synthetic ACP agent for the host smoke.

Speaks newline-delimited JSON-RPC 2.0 on stdio with the ACP v1 methods the
host uses for a custom agent: initialize, session/new (with the legacy
launch-only model selector), session/prompt and session/cancel. It answers
every prompt with a fixed text chunk, never reads files, never opens a
network connection and records nothing.
"""

import json
import sys

REPLY = 'HOST_SMOKE_OK'
MODEL = 'fixture/one'


def send(message):
    sys.stdout.write(json.dumps(message, separators=(',', ':')) + '\n')
    sys.stdout.flush()


def result(request_id, value):
    send({'jsonrpc': '2.0', 'id': request_id, 'result': value})


def error(request_id, code, message):
    send({'jsonrpc': '2.0', 'id': request_id, 'error': {'code': code, 'message': message}})


def main():
    sessions = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        message = json.loads(line)
        method, request_id = message.get('method'), message.get('id')
        params = message.get('params') or {}
        if method is None:
            # A response to a request this agent never sends; ignore it.
            continue
        if method == 'initialize':
            result(request_id, {
                'protocolVersion': 1,
                'agentCapabilities': {
                    'loadSession': False,
                    'promptCapabilities': {'image': False, 'audio': False, 'embeddedContext': False},
                },
                'authMethods': [],
            })
        elif method == 'session/new':
            sessions += 1
            result(request_id, {
                'sessionId': 'host-smoke-' + str(sessions),
                'models': {'currentModelId': MODEL, 'availableModels': [{'modelId': MODEL, 'name': 'one'}]},
            })
        elif method == 'session/prompt':
            send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
                'sessionId': params.get('sessionId'),
                'update': {'sessionUpdate': 'agent_message_chunk', 'content': {'type': 'text', 'text': REPLY}},
            }})
            result(request_id, {'stopReason': 'end_turn'})
        elif request_id is not None:
            error(request_id, -32601, 'Method not found')
        # Notifications such as session/cancel need no reply.


if __name__ == '__main__':
    main()
