"""Exercise the host release route with real ACP and native CLI processes."""

import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from smoke_real_acp_crash import Host, Model  # noqa: E402


async def native_state(aelix, host, path):
    cli = await asyncio.create_subprocess_exec(
        str(aelix), "--mode", "rpc", "--offline", "--no-extensions", "--session", path,
        cwd=host.root / "workspace", env=host.env, stdin=-1, stdout=-1, stderr=-1,
    )
    try:
        stdout, stderr = await asyncio.wait_for(cli.communicate(b'{"id":"ready","type":"get_state"}\n'), 15)
    finally:
        if cli.returncode is None:
            cli.kill()
            await cli.wait()
    if not stdout:
        return False, stderr.decode(errors="replace")
    ready = json.loads(stdout.splitlines()[0])
    assert ready["id"] == "ready" and ready["success"], ready
    return True, ""


def native_path(root, conversation_id):
    with contextlib.closing(sqlite3.connect(root / "host/aionui-backend.db")) as db:
        session_id, = db.execute("SELECT session_id FROM acp_session WHERE conversation_id=?", (conversation_id,)).fetchone()
    with contextlib.closing(sqlite3.connect(root / "acp/sessions.sqlite3")) as db:
        path, = db.execute("SELECT file FROM sessions WHERE id=?", (session_id,)).fetchone()
    return path


async def scenario(args):
    with tempfile.TemporaryDirectory(prefix="studio-release-") as directory:
        root = Path(directory).resolve()
        for name in ("home", "agent", "workspace", "host"):
            (root / name).mkdir()
        model = Model()
        port = await model.start()
        settings = root / "agent/settings.json"
        settings.write_text(json.dumps({"defaultProvider": "fixture", "defaultModel": "one"}))
        (root / "agent/models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions", "apiKey": "synthetic-fixture",
            "models": [{"id": "one", "name": "one", "contextWindow": 32768, "maxTokens": 1024}],
        }}}))
        env = {key: os.environ[key] for key in ("PATH", "TMP", "TEMP") if key in os.environ}
        env.update(HOME=str(root / "home"), AELIX_CODING_AGENT_DIR=str(root / "agent"),
                   AELIX_SETTINGS_PATH=str(settings), AIONUI_DISABLE_AUTO_UPDATE="1",
                   AIONUI_EXTENSIONS_PATH=str(root / "extensions"), AIONUI_IDLE_SCAN_INTERVAL_SECS="1")
        if args.idle_seconds is not None:
            env["AIONUI_IDLE_TIMEOUT_SECS"] = str(args.idle_seconds)
        launcher = root / "adapter.py"
        launcher.write_text(f"""import sys
from pathlib import Path
sys.path.insert(0, {str(args.acp_source)!r})
from aelix_acp.server import Server, ProtocolError, main
dispatch = Server.dispatch_request
async def wrapped(self, method, params):
    if method == "session/close":
        marker = Path({str(root / 'fail-close')!r})
        if marker.exists():
            marker.unlink()
            raise ProtocolError("Injected close failure", -32603)
    return await dispatch(self, method, params)
Server.dispatch_request = wrapped
main()
""")
        host = Host(args.host, root, env)
        try:
            await host.start()
            await host.api("/api/agents/custom", "POST", {"name": "Release fixture", "command": sys.executable,
                "args": [str(launcher), "--aelix", str(args.aelix), "--provider", "fixture",
                         "--model", "one", "--state-dir", str(root / "acp")], "env": [], "icon": "A"})
            assistant = next(entry for entry in await host.api("/api/assistants") if entry["name"] == "Release fixture")

            async def conversation(name):
                created = await host.api("/api/conversations", "POST", {
                    "name": name, "assistant": {"id": assistant["id"]},
                    "extra": {"workspace": str(root / "workspace"), "custom_workspace": True},
                })
                route = "/api/conversations/" + created["id"]
                await host.api(route + "/runtime/ensure", "POST")
                await host.api(route + "/messages", "POST", {"content": name})
                await host.idle(route)
                return route, native_path(root, created["id"])

            async def renew(route, view):
                await host.api(route + "/active-lease", "POST", {"view_id": view})

            async def release(route, view):
                result = await host.api(route + "/runtime/release", "POST", {"view_id": view})
                return result["outcome"]

            async def await_idle_cleanup(route, path, started):
                async with asyncio.timeout((args.idle_seconds or 600) + 110):
                    while (await host.api(route))["runtime"]["has_task"]:
                        await asyncio.sleep(1)
                async with asyncio.timeout(15):
                    while not (await native_state(args.aelix, host, path))[0]:
                        await asyncio.sleep(0.2)
                return round(time.monotonic() - started, 2)

            if args.idle_only:
                failed, failed_path = await conversation("release failure")
                await renew(failed, "failure-view")
                (root / "fail-close").touch()
                await host.api(failed + "/runtime/release", "POST", {"view_id": "failure-view"}, expected=(502,))
                failed_at = time.monotonic()
                crashed, crashed_path = await conversation("renderer gone")
                client = await asyncio.create_subprocess_exec(
                    sys.executable, "-c",
                    "import sys,time,urllib.request;"
                    "urllib.request.urlopen(urllib.request.Request(sys.argv[1],"
                    "data=b'{\"view_id\":\"crash-view\"}',headers={'Content-Type':'application/json'})).close();"
                    "print('ready',flush=True);time.sleep(120)",
                    f"http://127.0.0.1:{host.port}{crashed}/active-lease", stdout=-1,
                )
                try:
                    assert await asyncio.wait_for(client.stdout.readline(), 5) == b"ready\n"
                finally:
                    client.kill()
                    await client.wait()
                crash_at = time.monotonic()
                assert not (await native_state(args.aelix, host, failed_path))[0]
                assert not (await native_state(args.aelix, host, crashed_path))[0]
                print("Waiting for the idle scanner to recover both native writers", flush=True)
                recovered = await asyncio.gather(await_idle_cleanup(failed, failed_path, failed_at),
                                                 await_idle_cleanup(crashed, crashed_path, crash_at))
                print(json.dumps({"failedReleaseRecoveredSeconds": recovered[0],
                                  "clientDeathRecoveredSeconds": recovered[1]}))
                return

            route, path = await conversation("first persisted turn")
            await renew(route, "left-view")
            await renew(route, "right-view")
            assert await release(route, "left-view") == "view_active"
            assert not (await native_state(args.aelix, host, path))[0]
            await host.api(route + "/messages", "POST", {"content": "CRASH_HOLD"})
            await asyncio.wait_for(model.entered.wait(), 15)
            assert await release(route, "right-view") == "busy"
            model.release.set()
            await host.idle(route)
            before = Path(path).read_bytes()
            assert await release(route, "right-view") == "released"
            acquired, detail = await native_state(args.aelix, host, path)
            assert acquired, detail
            assert Path(path).read_bytes() == before
            assert await release(route, "right-view") == "inactive"
            await host.api(route + "/runtime/ensure", "POST")
            assert not (await native_state(args.aelix, host, path))[0]
            await host.stop()
            acquired, detail = await native_state(args.aelix, host, path)
            assert acquired, detail
            print(json.dumps({"otherViewProtected": True, "activeTurnProtected": True,
                              "cliImmediatelyReacquired": True, "historyPreserved": True,
                              "hostShutdownReleased": True}))
        except Exception:
            log = root / "host.log"
            if log.exists():
                print("\n".join(log.read_text(errors="replace").splitlines()[-60:]), file=sys.stderr)
            raise
        finally:
            model.release.set()
            await host.stop()
            model.server.close()
            await model.server.wait_closed()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=Path, required=True)
    parser.add_argument("--aelix", type=Path, required=True)
    parser.add_argument("--acp-source", type=Path, required=True)
    parser.add_argument("--idle-only", action="store_true")
    parser.add_argument("--idle-seconds", type=int)
    asyncio.run(scenario(parser.parse_args()))
