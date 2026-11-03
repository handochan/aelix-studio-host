"""Manual-only host crash probe; this script is not run in CI.

Uses a checked-out real ACP adapter and native runtime.

Adapted from the Issue #6 handoff crash_probe.py (itself based on aelix-acp
scripts/smoke_host.py). Model requests use a synthetic loopback endpoint.
The model request must arrive before SIGKILL; the host receives no turn output.
Use --expected-model-requests 2 for an unpatched-host negative control.
"""
import argparse
import asyncio
import contextlib
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request



def http(port, path, method="GET", body=None):
    request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method,
                                     data=None if body is None else json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        try:
            return error.code, json.loads(error.read())
        except ValueError:
            return error.code, {}


def tap(log, argv):
    """Transparent stdio relay that records frame structure, never payload text."""
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)

    def record(direction, frame):
        params = frame.get("params") if isinstance(frame.get("params"), dict) else {}
        update = params.get("update") if isinstance(params.get("update"), dict) else {}
        error = frame.get("error") if isinstance(frame.get("error"), dict) else {}
        entry = {"direction": direction, "method": frame.get("method"), "id": frame.get("id"),
                 "sessionUpdate": update.get("sessionUpdate"), "toolCallId": update.get("toolCallId"),
                 "status": update.get("status"), "metaKeys": sorted(update.get("_meta") or {}),
                 "errorCode": error.get("code"),
                 "errorKind": (error.get("data") or {}).get("kind") if isinstance(error.get("data"), dict) else None}
        with open(log, "a", encoding="utf-8") as stream:
            stream.write(json.dumps(entry) + "\n")

    import threading

    def outgoing():
        for line in child.stdout:
            with contextlib.suppress(ValueError):
                record("adapter_to_host", json.loads(line))
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()

    def outgoing_then_exit():
        outgoing()
        os._exit(child.wait() or 1)

    thread = threading.Thread(target=outgoing_then_exit, daemon=True)
    thread.start()
    for line in sys.stdin.buffer:
        with contextlib.suppress(ValueError):
            record("host_to_adapter", json.loads(line))
        child.stdin.write(line)
        child.stdin.flush()
    child.stdin.close()
    try:
        child.wait(10)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
    thread.join(5)


class Model:
    """Streaming fixture that holds the crash request until explicitly released."""

    def __init__(self):
        self.requests = []
        self.entered = asyncio.Event()
        self.release = asyncio.Event()

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
            if last.get("role") == "user" and "CRASH_HOLD" in content:
                self.entered.set()
                await self.release.wait()
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


class Host:
    def __init__(self, binary, root, env):
        self.binary, self.root, self.env = binary, root, env
        self.process = None

    async def start(self):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        log = (self.root / "host.log").open("ab")
        self.process = await asyncio.create_subprocess_exec(
            str(self.binary), "--local", "--port", str(self.port), "--data-dir", str(self.root / "host"),
            "--managed-resources-mode", "bundled", cwd=self.root / "workspace", env=self.env,
            stdout=log, stderr=log, start_new_session=True)
        async with asyncio.timeout(30):
            while True:
                try:
                    await self.api("/api/assistants")
                    return
                except OSError:
                    if self.process.returncode is not None:
                        raise RuntimeError("Host exited during startup")
                    await asyncio.sleep(0.05)

    async def stop(self):
        if self.process and self.process.returncode is None:
            self.process.terminate()
            try:
                await asyncio.wait_for(self.process.wait(), 10)
            except TimeoutError:
                os.killpg(self.process.pid, signal.SIGKILL)
                await self.process.wait()

    async def api(self, route, method="GET", body=None, expected=(200, 201, 202)):
        status, result = await asyncio.to_thread(http, self.port, route, method, body)
        assert status in expected, (route, status, result)
        return result.get("data", result) if isinstance(result, dict) else result

    async def idle(self, route, timeout=60):
        async with asyncio.timeout(timeout):
            while True:
                runtime = (await self.api(route)).get("runtime") or {}
                if runtime.get("state") in ("idle", None) and not runtime.get("is_processing") and runtime.get("turn_id") is None:
                    return
                await asyncio.sleep(0.1)

    def rows(self, conversation):
        with contextlib.closing(sqlite3.connect(f"file:{self.root / 'host/aionui-backend.db'}?mode=ro", uri=True)) as db:
            return [dict(id=row[0], type=row[1], content=row[2], status=row[3]) for row in db.execute(
                "SELECT id, type, content, status FROM messages WHERE conversation_id=? ORDER BY created_at", (conversation,))]


def frames(log):
    if not log.exists():
        return []
    return [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]


async def scenario(args):
    label = "candidate"
    with tempfile.TemporaryDirectory(prefix="acp-host-smoke-", dir="/tmp" if os.name != "nt" else None) as directory:
        root = Path(directory).resolve()
        for name in ("home", "agent", "workspace", "host"):
            (root / name).mkdir(parents=True)
        model = Model()
        port = await model.start()
        (root / "agent/models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions", "apiKey": "synthetic-fixture-key",
            "models": [{"id": "one", "name": "one", "reasoning": False, "input": ["text"], "contextWindow": 32768, "maxTokens": 1024}]}}}))
        # Native retry off: a provider failure reaches ACP after one model request.
        (root / "agent/settings.json").write_text(json.dumps({"defaultProvider": "fixture", "defaultModel": "one", "retry": {"enabled": False}}))
        env = {key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "WINDIR", "TMP", "TEMP") if key in os.environ}
        env.update(HOME=str(root / "home"), USERPROFILE=str(root / "home"), AELIX_CODING_AGENT_DIR=str(root / "agent"),
                   AELIX_SETTINGS_PATH=str(root / "agent/settings.json"), AELIX_AUTH_PATH=str(root / "agent/auth.json"),
                   AIONUI_DISABLE_AUTO_UPDATE="1", AIONUI_EXTENSIONS_PATH=str(root / "extensions"),
                   AIONUI_EXTENSION_STATES_FILE=str(root / "extension-states.json"))
        log = root / "wire.jsonl"
        launcher = root / "acp-launch.py"
        launcher.write_text(f"import os, sys\nopen({str(root / 'acp.pid')!r}, 'a').write(str(os.getpid()) + '\\n')\nsys.path.insert(0, {str(args.acp_source)!r})\nfrom aelix_acp.server import main\nmain()\n")
        adapter = [str(launcher), "--aelix", str(args.aelix), "--provider", "fixture", "--model", "one",
                   "--permission-mode", "yolo", "--enable-agents", "--state-dir", str(root / "acp")]
        argv = [str(Path(__file__).resolve()), "tap", str(log), "--", args.python, *adapter]
        host = Host(args.host, root, env)
        observed = {}
        await host.start()
        try:
            agent_name = "Host crash probe"
            await host.api("/api/agents/custom", "POST", {"name": agent_name, "command": args.python, "args": argv,
                                                           "env": [], "icon": "A"})
            assistant = next(a for a in await host.api("/api/assistants") if a["name"] == agent_name)

            async def conversation(name):
                created = await host.api("/api/conversations", "POST", {"name": name, "assistant": {"id": assistant["id"]},
                                         "extra": {"workspace": str(root / "workspace"), "custom_workspace": True}})
                return created["id"], "/api/conversations/" + created["id"]

            crashed, route = await conversation("crash")
            await host.api(route + "/runtime/ensure", "POST")
            await host.api(route + "/messages", "POST", {"content": "WARMUP"})
            await host.idle(route)
            mark = len(frames(log))
            starts_before = len((root / "acp.pid").read_text().split())
            await host.api(route + "/messages", "POST", {"content": "CRASH_HOLD"})
            await asyncio.wait_for(model.entered.wait(), 30)
            acp = int((root / "acp.pid").read_text().split()[-1])
            natives = [int(pid) for pid in subprocess.run(["pgrep", "-P", str(acp)], capture_output=True, text=True).stdout.split()]
            os.kill(acp, signal.SIGKILL)
            if args.kill_native:
                for pid in natives:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(pid, signal.SIGKILL)
            await asyncio.sleep(1)
            model.release.set()
            await asyncio.sleep(5)
            await host.idle(route, timeout=60)
            await asyncio.sleep(3)
            after = frames(log)[mark:]
            observed["crash"] = {"killNative": args.kill_native, "nativeChildren": len(natives),
                                 "hostRequestsAfterCrash": [f["method"] for f in after if f["direction"] == "host_to_adapter" and f["method"]],
                                 "errorCodes": [f["errorCode"] for f in after if f["errorCode"] is not None],
                                 "modelRequestsForTurn": model.count("CRASH_HOLD"),
                                 "adapterRestarts": len((root / "acp.pid").read_text().split()) - starts_before}
            rows = host.rows(crashed)
            observed["crash"]["userRowsForTurn"] = sum(1 for r in rows if "CRASH_HOLD" in r["content"] and r["type"] == "text")
            observed["crash"]["tips"] = [r["content"][:400] for r in rows if r["type"] == "tips"]
            crash = observed["crash"]
            assert crash["modelRequestsForTurn"] == args.expected_model_requests, crash
            assert crash["hostRequestsAfterCrash"].count("session/prompt") == args.expected_model_requests, crash
            if args.expected_model_requests == 1:
                assert crash["adapterRestarts"] == 0, crash
                assert any("USER_AGENT_PROMPT_OUTCOME_UNKNOWN" in r["content"] for r in rows), crash
                assert any('"retryable":false' in r["content"].replace(" ", "") for r in rows if r["type"] == "tips"), crash
            return {"label": label, "observed": observed}
        finally:
            await host.stop()
            model.server.close()
            import shutil
            keep = args.output / label
            keep.mkdir(parents=True, exist_ok=True)
            shutil.copy2(root / "host.log", keep / "host.log")
            if log.exists():
                shutil.copy2(log, keep / "wire.jsonl")
            (keep / "observed.json").write_text(json.dumps(observed, indent=2) + "\n")
            if (root / "host/logs").exists():
                shutil.copytree(root / "host/logs", keep / "logs")


async def run(args):
    report = {"host": {"path": str(args.host), "sha256": hashlib.sha256(args.host.read_bytes()).hexdigest()},
              "native": str(args.aelix), "acpSource": str(args.acp_source), "model": "synthetic loopback",
              "renderer": "not run", "scenarios": [await scenario(args)]}
    return report


def main():
    if len(sys.argv) > 3 and sys.argv[1] == "tap" and sys.argv[3] == "--":
        tap(sys.argv[2], sys.argv[4:])
        return
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--host", type=Path, required=True)
    parser.add_argument("--aelix", type=Path, required=True)
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--acp-source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-model-requests", type=int, default=1)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--kill-native", action="store_true")
    args = parser.parse_args()
    report = asyncio.run(run(args))
    report["status"] = "passed"
    report["expectedModelRequests"] = args.expected_model_requests
    text = json.dumps(report, indent=2)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(text + "\n", encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
