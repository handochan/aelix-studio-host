"""Manual host #4 scope 2/3 probe; not run by CI.

Real host + ACP/native processes, with a synthetic loopback model and isolated
state. No real provider credentials. Adapted from aelix-acp@16d6555's smoke_host.py.
Only structural wire data, synthetic child metadata and structured errors are
retained. This does not test Studio rendering or history reconciliation.
"""
import argparse
import asyncio
import contextlib
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading

from smoke_real_acp_crash import Host, frames, http


def tap(log, argv):
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)

    def record(direction, line):
        with contextlib.suppress(ValueError):
            frame = json.loads(line)
            params = frame.get("params") or {}
            update = params.get("update") or {}
            error = frame.get("error") or {}
            item = {"direction": direction, "method": frame.get("method"), "sessionId": params.get("sessionId"),
                    "sessionUpdate": update.get("sessionUpdate"), "toolCallId": update.get("toolCallId"),
                    "status": update.get("status"), "meta": update.get("_meta"),
                    "errorCode": error.get("code"), "errorData": error.get("data")}
            with log.open("a", encoding="utf-8") as stream:
                stream.write(json.dumps(item) + "\n")

    def outgoing():
        for line in child.stdout:
            record("adapter_to_host", line)
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
        os._exit(child.wait() or 1)

    thread = threading.Thread(target=outgoing, daemon=True)
    thread.start()
    for line in sys.stdin.buffer:
        record("host_to_adapter", line)
        child.stdin.write(line)
        child.stdin.flush()
    child.stdin.close()
    try:
        child.wait(10)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
    thread.join(5)


class Model:
    """OpenAI-compatible streaming fixture with scripted delegation and failure."""

    def __init__(self):
        self.requests = []

    async def start(self):
        self.server = await asyncio.start_server(self.respond, "127.0.0.1", 0)
        return self.server.sockets[0].getsockname()[1]

    async def respond(self, reader, writer):
        try:
            header = (await reader.readuntil(b"\r\n\r\n")).decode()
            length = int(next(line.split(":", 1)[1] for line in header.split("\r\n") if line.lower().startswith("content-length:")))
            request = json.loads(await reader.readexactly(length))
            self.requests.append(request)
            messages = request.get("messages", [])
            last = messages[-1] if messages else {}
            content = json.dumps(last.get("content"))
            if last.get("role") == "user" and "FAIL_NOW" in content:
                body = b'data: {"error":{"message":"SYNTHETIC_TURN_FAILURE","type":"server_error"}}\n\n'
            else:
                if last.get("role") == "user" and "DELEGATE_NOW" in content:
                    delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"call_agent_{len(self.requests)}", "type": "function",
                             "function": {"name": "agent", "arguments": json.dumps({"profile": "reviewer", "task": "CHILD_TASK reply"})}}]}
                    stop = "tool_calls"
                elif last.get("role") == "user" and "CHILD_TASK" in content:
                    await asyncio.sleep(1.5)  # Long enough for live progress frames.
                    delta, stop = {"role": "assistant", "content": "CHILD_OK"}, "stop"
                else:
                    delta, stop = {"role": "assistant", "content": "SMOKE_OK"}, "stop"
                body = ("".join("data: " + json.dumps({"id": "f", "object": "chat.completion.chunk", "created": 0,
                        "model": request.get("model", "one"), "choices": [{"index": 0, "delta": d, "finish_reason": f}]}) + "\n\n"
                        for d, f in ((delta, None), ({}, stop))) + "data: [DONE]\n\n").encode()
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: "
                         + str(len(body)).encode() + b"\r\n\r\n" + body)
            await writer.drain()
        except (ConnectionError, asyncio.IncompleteReadError):
            pass
        finally:
            writer.close()

    def count(self, marker):
        return sum(1 for request in self.requests if request.get("messages")
                   and request["messages"][-1].get("role") == "user" and marker in json.dumps(request["messages"][-1].get("content")))


async def scenario(args):
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="host-4-contracts-") as directory:
        root = Path(directory).resolve()
        for name in ("home", "agent/agents", "workspace", "host"):
            (root / name).mkdir(parents=True)
        model = Model()
        port = await model.start()
        (root / "agent/models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions", "apiKey": "synthetic-fixture-key",
            "models": [{"id": "one", "name": "one", "reasoning": False, "input": ["text"], "contextWindow": 32768, "maxTokens": 1024}]}}}))
        (root / "agent/settings.json").write_text(json.dumps({"defaultProvider": "fixture", "defaultModel": "one", "retry": {"enabled": False}}))
        (root / "agent/agents/reviewer.md").write_text("---\nname: reviewer\ndescription: Review\ntools: [read]\n---\nReview.\n")
        env = {key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "WINDIR", "TMP", "TEMP") if key in os.environ}
        env.update(HOME=str(root / "home"), USERPROFILE=str(root / "home"), AELIX_CODING_AGENT_DIR=str(root / "agent"),
                   AELIX_SETTINGS_PATH=str(root / "agent/settings.json"), AELIX_AUTH_PATH=str(root / "agent/auth.json"),
                   AIONUI_DISABLE_AUTO_UPDATE="1", AIONUI_EXTENSIONS_PATH=str(root / "extensions"),
                   AIONUI_EXTENSION_STATES_FILE=str(root / "extension-states.json"))
        log = args.output / "wire.jsonl"
        log.write_text("")
        launcher = root / "acp-launch.py"
        launcher.write_text(f"import sys\nsys.path.insert(0, {str(args.acp_source)!r})\nfrom aelix_acp.server import main\nmain()\n")
        argv = [str(Path(__file__).resolve()), "tap", str(log), "--", sys.executable, str(launcher),
                "--aelix", str(args.aelix), "--provider", "fixture", "--model", "one",
                "--permission-mode", "yolo", "--enable-agents", "--state-dir", str(root / "acp")]
        host = Host(args.host, root, env)
        observed = {}
        try:
            await host.start()
            name = "Host ACP contracts probe"
            await host.api("/api/agents/custom", "POST", {"name": name, "command": sys.executable, "args": argv, "env": [], "icon": "A"})
            assistant = next(a for a in await host.api("/api/assistants") if a["name"] == name)
            created = await host.api("/api/conversations", "POST", {"name": "contracts", "assistant": {"id": assistant["id"]},
                                     "extra": {"workspace": str(root / "workspace"), "custom_workspace": True}})
            conversation = created["id"]
            route = "/api/conversations/" + conversation
            await host.api(route + "/runtime/ensure", "POST")
            await host.api(route + "/messages", "POST", {"content": "DELEGATE_NOW"})
            await host.idle(route)
            outgoing = [f for f in frames(log) if f["direction"] == "adapter_to_host"]
            parent = next(f["toolCallId"] for f in outgoing if f["sessionUpdate"] == "tool_call")
            progress = [f for f in outgoing if f["toolCallId"] == parent and (f["meta"] or {}).get("aelix/subagents")]
            assert progress, host.rows(conversation)
            session_id = progress[-1]["sessionId"]
            live_meta = progress[-1]["meta"]
            assert any(f["status"] == "in_progress" for f in progress), progress
            assert live_meta["aelix/subagents"]["children"][0]["state"] == "done", progress
            row = next(r for r in host.rows(conversation) if r["id"] == parent)
            assert json.loads(row["content"])["_meta"] == live_meta and row["status"] == "finish", row
            await host.stop()
            await host.start()
            await host.api(route + "/runtime/ensure", "POST")
            for _ in range(2):
                listed = await host.api(route + "/messages")
                row = next(r for r in listed["items"] if r["id"] == parent)
                assert row["content"]["_meta"] == live_meta, row
            assert model.count("DELEGATE_NOW") == 1
            observed["metadata"] = {"progressFrames": len(progress), "liveEqualsStoredAndReloaded": True, "parentStatus": row["status"], "modelRequests": model.count("DELEGATE_NOW")}

            # A real post-dispatch model failure must reach Studio's error tip once.
            mark = len(frames(log))
            await host.api(route + "/messages", "POST", {"content": "FAIL_NOW"})
            await host.idle(route)
            await asyncio.sleep(2)
            after = frames(log)[mark:]
            replies = [f for f in after if f["errorCode"] is not None]
            assert len(replies) == 1 and replies[0]["errorCode"] == -33002, replies
            tips = [json.loads(r["content"]).get("error", {}) for r in host.rows(conversation) if r["type"] == "tips"]
            failure = next(e for e in tips if e.get("code") == "USER_AGENT_TURN_FAILED_AFTER_DISPATCH")
            assert failure["protocol_code"] == -33002 and failure["retryable"] is False
            assert failure["data"] == replies[0]["errorData"], failure
            assert model.count("FAIL_NOW") == 1
            observed["dispatchFailure"] = {"code": failure["code"], "data": failure["data"], "modelRequests": 1,
                                            "promptRequests": sum(f["method"] == "session/prompt" for f in after)}
            assert observed["dispatchFailure"]["promptRequests"] == 1

            # The test reads only its own ACP index to locate the native session.
            # No host implementation reads a native transcript or adapter DB.
            with contextlib.closing(sqlite3.connect(root / "acp/sessions.sqlite3")) as db:
                native_file = db.execute("SELECT file FROM sessions WHERE id=?", (session_id,)).fetchone()[0]
            await host.stop()
            holder = subprocess.Popen([str(args.aelix), "--mode", "rpc", "--offline", "--no-extensions", "--session", native_file],
                                      cwd=root / "workspace", env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            try:
                holder.stdin.write(b'{"id":"ready","type":"get_state"}\n')
                holder.stdin.flush()
                assert b'"ready"' in await asyncio.wait_for(asyncio.to_thread(holder.stdout.readline), 30)
                await host.start()
                mark = len(frames(log))
                status, result = await asyncio.to_thread(http, host.port, route + "/runtime/ensure", "POST")
                assert status == 502 and result["code"] == "USER_AGENT_SESSION_LOCKED", (status, result)
                details = result["details"]
                assert details["protocol_code"] == -33001 and details["retryable"] is False
                assert details["data"]["lockedBy"] == "native" and details["data"]["holder"] is None
                reply = next(f for f in frames(log)[mark:] if f["errorCode"] == -33001)
                assert reply["errorData"] == details["data"]
                assert not [f for f in frames(log)[mark:] if f["method"] in ("session/new", "session/prompt")]
                observed["lock"] = {"httpStatus": status, "code": result["code"], "protocolCode": details["protocol_code"], "data": details["data"], "noPromptOrNewSession": True}
            finally:
                holder.stdin.close()
                try:
                    holder.wait(10)
                except subprocess.TimeoutExpired:
                    holder.kill()
                    holder.wait()
            await host.api(route + "/runtime/ensure", "POST")
            await host.api(route + "/messages", "POST", {"content": "AFTER_UNLOCK"})
            await host.idle(route)
            assert model.count("AFTER_UNLOCK") == 1
            observed["unlockResumed"] = True
            return observed
        finally:
            await host.stop()
            model.server.close()
            await model.server.wait_closed()
            if (root / "host.log").exists():
                (args.output / "host.log").write_bytes((root / "host.log").read_bytes())


def main():
    if len(sys.argv) > 3 and sys.argv[1] == "tap" and sys.argv[3] == "--":
        tap(Path(sys.argv[2]), sys.argv[4:])
        return
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--host", type=Path, required=True)
    parser.add_argument("--aelix", type=Path, required=True)
    parser.add_argument("--acp-source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    observed = asyncio.run(scenario(args))
    report = {"status": "passed", "hostSha256": hashlib.sha256(args.host.read_bytes()).hexdigest(),
              "model": "synthetic loopback", "renderer": "not run", "observed": observed}
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
