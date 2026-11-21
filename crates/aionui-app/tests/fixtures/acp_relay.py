"""Scripted ACP peer for the host transport contract tests."""
import base64
import hashlib
import json
import os
from pathlib import Path
import sys
import threading
import time

counter = Path(sys.argv[1])
binding = None
session = "sdk-session"
prompt = None
writes = {}
active = 0
elapsed_ms = 0
output_lock = threading.Lock()
pause_next = False
byte_exchange = None
host_bytes_enabled = counter.with_suffix(".host-bytes-enabled").exists()
diagnostic_fixture = counter.with_suffix(".diagnostic-fixture").exists()
raw_marker = "private-h1-standard-raw"
escaped_marker = 'private-h1-standard-"quoted"\\끝'




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


def reverse_read(phase, **fields):
    byte_exchange["phase"] = phase
    payload = byte_exchange["payload"]
    # A long ID forces the Host to size raw data against the full JSON frame.
    request_id = ("byte-read-" + "x" * 1012) if phase == "first" else "byte-" + phase
    send({"id": request_id, "method": "_aelix/host/bytes/read", "params": {
        "sessionId": payload.get("sessionOverride", session),
        "bindingId": payload.get("bindingOverride", binding),
        "ref": payload["grant"], "readerId": "fixture-reader", **fields,
    }})


def continue_after_controls():
    if byte_exchange and byte_exchange["phase"] == "controls" and byte_exchange.get("allowed") and byte_exchange.get("stopped"):
        reverse_read("last", offset=len(byte_exchange["first"]), maxBytes=147264)


def accept_byte_reply(line, frame):
    exchange = byte_exchange
    phase = exchange["phase"]
    if phase == "denied":
        result(exchange["id"], {"code": frame["error"]["code"]})
    elif phase == "first":
        size = len(line.rstrip("\n").encode())
        assert 196605 <= size <= 196608
        chunk = frame["result"]
        assert chunk["offset"] == 0 and not chunk["eof"] and chunk["byteLength"] == exchange["payload"]["length"]
        exchange["size"] = size
        exchange["first"] = base64.b64decode(chunk["data"], validate=True)
        assert len(exchange["first"]) < 147264
        assert chunk["sha256"] == exchange["payload"]["sha256"]
        print(exchange["payload"]["marker"], chunk["sha256"], exchange["payload"]["grant"],
              "data:application/octet-stream;base64," + chunk["data"], file=sys.stderr, flush=True)
        exchange["phase"] = "controls"
        send({"id": "byte-permission", "method": "session/request_permission", "params": {
            "sessionId": session, "toolCall": {"toolCallId": "byte-permission", "title": "Fixture permission"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}],
        }})
    elif phase == "last":
        chunk = frame["result"]
        assert chunk["eof"] and chunk["offset"] == len(exchange["first"])
        full = exchange["first"] + base64.b64decode(chunk["data"], validate=True)
        assert len(full) == exchange["payload"]["length"]
        assert hashlib.sha256(full).hexdigest() == exchange["payload"]["sha256"] == chunk["sha256"]
        # Drop the application-level result and request the same last offset.
        exchange["last"] = chunk
        reverse_read("replay-last", offset=chunk["offset"], maxBytes=147264)
    elif phase == "replay-last":
        assert frame["result"] == exchange.pop("last")
        reverse_read("finish", offset=exchange["payload"]["length"], finish=True)
    elif phase == "finish":
        assert frame["result"] == {"released": True}
        reverse_read("replay-finish", offset=exchange["payload"]["length"], finish=True)
    elif phase == "replay-finish":
        assert frame["result"] == {"released": True}
        result(exchange["id"], {"lineBytes": exchange["size"], "replayed": True, "released": True,
                                "permission": exchange["allowed"], "stop": exchange["stopped"]})
    else:
        assert False, "Unexpected byte exchange phase"


while True:
    first = ""
    if pause_next:
        first = sys.stdin.read(1)
        counter.with_suffix(".pipe-blocked").touch()
        while not counter.with_suffix(".resume-read").exists():
            time.sleep(0.01)
        pause_next = False
    line = first + sys.stdin.readline()
    if not line:
        break
    frame = json.loads(line)
    method, params, rpc_id = frame.get("method"), frame.get("params", {}), frame.get("id")
    if method == "initialize":
        peer = sys.argv[2] if len(sys.argv) > 2 else "aelix-acp"
        relay = params["clientCapabilities"].get("_meta", {}).get("aelix/sessionRelay")
        counter.with_suffix(".initialize").write_text("relay" if relay else "standard")
        if relay is not None:
            assert relay == {"version": 1, "notifications": False}
        capabilities = params["clientCapabilities"].get("_meta", {})
        byte_meta = {}
        byte_methods = []
        if host_bytes_enabled and relay:
            assert capabilities["aelix/hostBytes"] == {"version": 1}
            byte_meta["aelix/hostBytes"] = {"version": 1}
            byte_methods = ["_aelix/session/probe/bytes", "_aelix/session/probe/byte-denied",
                            "_aelix/session/probe/byte-error", "_aelix/session/probe/byte-disconnect"]
            counter.with_suffix(".provider-debug").write_text(str(
                any(name in os.environ for name in ("ANTHROPIC_LOG", "OPENAI_LOG", "GEMINI_NEXT_GEN_API_LOG"))))
        result(rpc_id, {
            "protocolVersion": 1, "agentInfo": {"name": peer, "version": "fixture"},
            "agentCapabilities": {"loadSession": True, "_meta": {**byte_meta, "aelix/sessionRelay": {
                "version": 1, "requests": ["_aelix/session/probe/echo", "_aelix/session/probe/size",
                                           "_aelix/session/probe/finish", "_aelix/session/probe/write",
                                           "_aelix/session/requests/lookup", "_aelix/session/probe/hold",
                                           "_aelix/session/probe/clock", "_aelix/session/probe/pause",
                                           "_aelix/session/probe/burst", "_aelix/session/probe/queued", *byte_methods],
                "notifications": []}}},
        })
    elif method in ("session/new", "session/load", "session/fork"):
        refusal = counter.with_suffix(".recovery-fixture")
        if host_bytes_enabled and refusal.exists():
            send({"id": rpc_id, "error": {"code": -32004,
                "message": escaped_marker, "data": json.loads(refusal.read_text(encoding="utf-8"))}})
            continue
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
        if diagnostic_fixture:
            out.setdefault("_meta", {})["fixtureDiagnostic"] = [raw_marker, escaped_marker]
        result(rpc_id, out)
    elif method == "session/prompt":
        prompt = rpc_id
        if diagnostic_fixture:
            print(raw_marker, json.dumps(escaped_marker), file=sys.stderr, flush=True)
            if params["prompt"][0].get("text") == "emit standard error":
                send({"id": rpc_id, "error": {"code": -33002,
                    "message": raw_marker, "data": {"detail": escaped_marker}}})
                prompt = None
                continue
            send({"method": "session/update", "params": {"sessionId": session, "update": {
                "sessionUpdate": "tool_call", "toolCallId": "diagnostic-call", "title": "Diagnostic fixture",
                "kind": "other", "status": "in_progress",
            }}})
            send({"method": "session/update", "params": {"sessionId": session, "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "diagnostic-call", "status": "completed",
                "rawOutput": {"content": [{"type": "image", "mimeType": "image/png",
                    "data": base64.b64encode(raw_marker.encode()).decode(), "metadata": escaped_marker}]},
            }}})
        if counter.with_suffix(".watch-shutdown").exists():
            # Fill stdout while the host drains stdin in the opposite direction.
            update("agent_message_chunk", "x" * (512 * 1024))
        else:
            update("agent_message_chunk", "before input")
        counter.with_suffix(".prompt-started").touch()
    elif method == "session/cancel":
        if counter.with_suffix(".watch-shutdown").exists():
            counter.with_suffix(".stop-recorded").write_text(params["sessionId"])
        if prompt is not None:
            result(prompt, {"stopReason": "cancelled"})
            prompt = None
        if byte_exchange and byte_exchange["phase"] == "controls":
            byte_exchange["stopped"] = True
            continue_after_controls()
    elif rpc_id == "byte-permission" and method is None:
        assert frame["result"]["outcome"] == {"outcome": "selected", "optionId": "allow"}
        byte_exchange["allowed"] = True
        counter.with_suffix(".byte-permission-answered").touch()
        continue_after_controls()
    elif isinstance(rpc_id, str) and rpc_id.startswith("byte-") and method is None:
        accept_byte_reply(line, frame)
    elif method and method.startswith("_"):
        assert set(params) == {"sessionId", "bindingId", "payload"}
        assert params["sessionId"] == session and params["bindingId"] == binding
        body = params["payload"]
        with counter.open("a", encoding="utf-8") as calls:
            calls.write(method + "\n")
        if method.endswith(("/echo", "/queued")):
            result(rpc_id, body.get("value"))
        elif method.endswith("/size"):
            frame = {"jsonrpc": "2.0", "id": rpc_id, "result": {"rpcId": rpc_id, "value": "한\"\\"}}
            frame["result"]["value"] += "x" * (body["bytes"] - len(encode(frame).encode("utf-8")))
            print(encode(frame), flush=True)
        elif method.endswith("/hold"):
            timer = threading.Timer(2, result, (rpc_id, None))
            timer.daemon = True
            timer.start()
        elif method.endswith("/pause"):
            pause_next = True
            result(rpc_id, None)
        elif method.endswith("/burst"):
            active_prompt = prompt
            prompt = None
            result(rpc_id, None)

            def burst():
                while not counter.with_suffix(".emit").exists():
                    time.sleep(0.01)
                update("user_message_chunk", "held input", "held")
                time.sleep(0.2)
                update("user_message_chunk", "lagged input", "lagged")
                for _ in range(340):
                    update("agent_message_chunk", "padding")
                result(active_prompt, {"stopReason": "end_turn"})

            threading.Thread(target=burst, daemon=True).start()
        elif method.endswith("/finish"):
            assert prompt is not None
            if body.get("failInsert"):
                active_prompt = prompt
                prompt = None
                result(rpc_id, None)

                def fail_insert():
                    while not counter.with_suffix(".reject-insert").exists():
                        time.sleep(0.01)
                    update("user_message_chunk", "blocked history", "rejected")
                    result(active_prompt, {"stopReason": "end_turn"})

                threading.Thread(target=fail_insert, daemon=True).start()
                continue
            update("user_message_chunk", "ignored no id")
            update("user_message_chunk", "ignored wrong session", "wrong", sid="other")
            update("user_message_chunk", "same 입력", "once")
            update("user_message_chunk", "same 입력", "once")
            update("user_message_chunk", "same 입력", "twice")
            send({"method": "session/update", "params": {"sessionId": session, "update": {
                "sessionUpdate": "tool_call", "toolCallId": "ordering-tool",
                "title": "ordering tool", "kind": "other", "status": "completed",
            }}})
            update("agent_message_chunk", "after input")
            result(prompt, {"stopReason": "end_turn"})
            prompt = None
            update("user_message_chunk", "ignored after result", "late")
            result(rpc_id, None)
        elif method.endswith("/clock"):
            elapsed_ms = body["elapsedMs"]
            result(rpc_id, None)
        elif method.endswith(("/bytes", "/byte-denied")):
            byte_exchange = {"id": rpc_id, "payload": body}
            phase = "denied" if method.endswith("/byte-denied") else "first"
            if body.get("holdForDatabase"):
                counter.with_suffix(".reverse-paused").touch()

                def after_database_lock():
                    while not counter.with_suffix(".reverse-go").exists():
                        time.sleep(0.01)
                    reverse_read("denied", offset=0, maxBytes=147264)

                threading.Thread(target=after_database_lock, daemon=True).start()
            else:
                reverse_read(phase, offset=0, maxBytes=147264)
        elif method.endswith("/byte-error"):
            print(json.dumps(body), file=sys.stderr, flush=True)
            send({"id": rpc_id, "error": {"code": -32603, "message": body["marker"], "data": body}})
        elif method.endswith("/byte-disconnect"):
            print(json.dumps(body), file=sys.stderr, flush=True)
            break
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
                send({"id": rpc_id, "error": {"code": -32602, "message": "relay-secret", "data": body}})
        elif method.endswith("/lookup"):
            key = body["requestId"]
            saved = writes.get(key)
            if saved is None or elapsed_ms - saved[1] >= 600000:
                result(rpc_id, {"status": "unavailable"})
            else:
                result(rpc_id, {"status": "completed", "result": {"accepted": True, "effects": saved[2]}})
    elif rpc_id is not None:
        result(rpc_id, {})
