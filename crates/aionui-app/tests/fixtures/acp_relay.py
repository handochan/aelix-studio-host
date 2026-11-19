"""Scripted ACP peer for the host transport contract tests."""
import json
from pathlib import Path
import sys
import threading

counter = Path(sys.argv[1])
binding = None
session = "sdk-session"
prompt = None
writes = {}
active = 0
elapsed_ms = 0
output_lock = threading.Lock()


def encode(frame):
    return json.dumps(frame, ensure_ascii=False, separators=(",", ":"))


def send(frame):
    with output_lock:
        print(encode({"jsonrpc": "2.0", **frame}), flush=True)


def result(rpc_id, value):
    send({"id": rpc_id, "result": value})


def update(kind, text, message_id=None, sid=session):
    payload = {"sessionUpdate": kind, "content": {"type": "text", "text": text}}
    if message_id:
        payload["messageId"] = message_id
    send({"method": "session/update", "params": {"sessionId": sid, "update": payload}})


for line in sys.stdin:
    frame = json.loads(line)
    method, params, rpc_id = frame.get("method"), frame.get("params", {}), frame.get("id")
    if method == "initialize":
        peer = sys.argv[2] if len(sys.argv) > 2 else "aelix-acp"
        relay = params["clientCapabilities"].get("_meta", {}).get("aelix/sessionRelay")
        if relay is not None:
            assert relay == {"version": 1, "notifications": False}
        result(rpc_id, {
            "protocolVersion": 1, "agentInfo": {"name": peer, "version": "fixture"},
            "agentCapabilities": {"loadSession": True, "_meta": {"aelix/sessionRelay": {
                "version": 1, "requests": ["_aelix/session/probe/echo", "_aelix/session/probe/size",
                                           "_aelix/session/probe/finish", "_aelix/session/probe/write",
                                           "_aelix/session/requests/lookup", "_aelix/session/probe/hold",
                                           "_aelix/session/probe/clock"],
                "notifications": []}}},
        })
    elif method in ("session/new", "session/load", "session/fork"):
        meta = params.get("_meta", {}).get("aelix/sessionRelay")
        binding = meta["bindingId"] if meta else None
        update("user_message_chunk", "session replay", "load-replay")
        if method == "session/load":
            assert params["sessionId"] == session
            # Standard replay frame matching the N4 measurement's largest line.
            replay = {"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId":session, "update":{
                    "sessionUpdate":"user_message_chunk", "messageId":"large-replay",
                    "content":{"type":"resource","resource":{"uri":"file:///synthetic.txt","text":""}}
                }
            }}
            replay["params"]["update"]["content"]["resource"]["text"] = "r" * (16777274 - len(encode(replay)))
            print(encode(replay), flush=True)
        out = {"sessionId": session} if method != "session/load" else {}
        if meta:
            out["_meta"] = {"aelix/sessionRelay": meta}
        result(rpc_id, out)
    elif method == "session/prompt":
        prompt = rpc_id
        update("agent_message_chunk", "before input")
    elif method == "session/cancel" and prompt is not None:
        result(prompt, {"stopReason": "cancelled"})
        prompt = None
    elif method and method.startswith("_"):
        assert set(params) == {"sessionId", "bindingId", "payload"}
        assert params["sessionId"] == session and params["bindingId"] == binding
        body = params["payload"]
        with counter.open("a", encoding="utf-8") as calls:
            calls.write(method + "\n")
        if method.endswith("/echo"):
            result(rpc_id, body.get("value"))
        elif method.endswith("/size"):
            frame = {"jsonrpc": "2.0", "id": rpc_id, "result": "한\"\\"}
            frame["result"] += "x" * (body["bytes"] - len(encode(frame).encode("utf-8")))
            print(encode(frame), flush=True)
        elif method.endswith("/hold"):
            timer = threading.Timer(2, result, (rpc_id, None))
            timer.daemon = True
            timer.start()
        elif method.endswith("/finish"):
            assert prompt is not None
            update("user_message_chunk", "ignored no id")
            update("user_message_chunk", "ignored wrong session", "wrong", sid="other")
            update("user_message_chunk", "same 입력", "once")
            update("user_message_chunk", "same 입력", "once")
            update("user_message_chunk", "same 입력", "twice")
            update("agent_message_chunk", "after input")
            result(prompt, {"stopReason": "end_turn"})
            prompt = None
            update("user_message_chunk", "ignored after result", "late")
            result(rpc_id, None)
        elif method.endswith("/clock"):
            elapsed_ms = body["elapsedMs"]
            result(rpc_id, None)
        elif method.endswith("/write"):
            print("relay-secret", file=sys.stderr, flush=True)
            key = body["requestId"]
            if key in writes and writes[key][0] != body:
                send({"id": rpc_id, "error": {"code": -32602, "message": "relay-secret", "data": body}})
            else:
                if key not in writes:
                    active += 1
                    writes[key] = (body, elapsed_ms, active)
                result(rpc_id, {"accepted": True, "effects": writes[key][2]})
        elif method.endswith("/lookup"):
            key = body["requestId"]
            saved = writes.get(key)
            if saved is None or elapsed_ms - saved[1] >= 600000:
                result(rpc_id, {"status": "unavailable"})
            else:
                result(rpc_id, {"status": "completed", "result": {"accepted": True, "effects": saved[2]}})
    elif rpc_id is not None:
        result(rpc_id, {})
