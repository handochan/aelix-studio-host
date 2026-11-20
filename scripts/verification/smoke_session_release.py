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
        launcher.write_text(f"""import asyncio, sys
from pathlib import Path
sys.path.insert(0, {str(args.acp_source)!r})
from aelix_acp.server import Server, ProtocolError, main
dispatch = Server.dispatch_request
async def wrapped(self, method, params):
    if method == "session/close":
        root = Path({str(root)!r})
        if (root / "fail-close").exists():
            (root / "fail-close").unlink()
            raise ProtocolError("Injected pre-dispatch refusal", -32601)
        if (root / "block-close").exists():
            (root / "close-entered").touch()
            while (root / "block-close").exists():
                await asyncio.sleep(0.01)
        result = await dispatch(self, method, params)
        if (root / "drop-close-reply").exists():
            (root / "drop-close-reply").unlink()
            (root / "close-executed").touch()
            await asyncio.Event().wait()
        return result
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

            async def open_view(route, view):
                return await host.api(route + "/runtime/ensure?view_id=" + view, "POST")

            async def renew(route, view, expected=(200,)):
                return await host.api(route + "/active-lease", "POST", {"view_id": view}, expected=expected)

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
                await open_view(failed, "failure-view")
                (root / "fail-close").touch()
                await host.api(failed + "/runtime/release", "POST", {"view_id": "failure-view"}, expected=(502,))
                failed_at = time.monotonic()
                crashed, crashed_path = await conversation("renderer gone")
                await open_view(crashed, "crash-view")
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

            async def hold_turn(route):
                model.entered.clear()
                model.release = asyncio.Event()
                await host.api(route + "/messages", "POST", {"content": "CRASH_HOLD"})
                await asyncio.wait_for(model.entered.wait(), 15)

            async def assert_writer_held(path):
                acquired, detail = await native_state(args.aelix, host, path)
                assert not acquired and "already open" in detail, detail

            async def wait_until_retired(route):
                async with asyncio.timeout(15):
                    while (await host.api(route))["runtime"]["has_task"]:
                        await asyncio.sleep(0.05)

            route, path = await conversation("first persisted turn")
            await open_view(route, "left-view")
            await open_view(route, "right-view")
            assert await release(route, "unknown-view") == "inactive"
            assert await release(route, "left-view") == "view_active"
            await assert_writer_held(path)

            # A one-shot departure remains owned by the host after the client is gone.
            await hold_turn(route)
            assert await release(route, "right-view") == "deferred"
            await renew(route, "right-view", expected=(409,))
            assert await release(route, "right-view") == "inactive"
            model.release.set()
            await wait_until_retired(route)
            saved = Path(path).read_bytes()
            acquired, detail = await native_state(args.aelix, host, path)
            assert acquired and Path(path).read_bytes() == saved, detail

            # A replacement view admitted before the turn ends supersedes the old departure.
            await open_view(route, "old-view")
            await hold_turn(route)
            assert await release(route, "old-view") == "deferred"
            await open_view(route, "replacement-view")
            await renew(route, "old-view", expected=(409,))
            assert await release(route, "old-view") == "inactive"
            model.release.set()
            await host.idle(route)
            await assert_writer_held(path)

            # A second ensure and a heartbeat contend with an admitted close.
            # They cannot finish ahead of it, and must get a fresh runtime afterwards.
            blocker = root / "block-close"
            blocker.touch()
            closing = asyncio.create_task(release(route, "replacement-view"))
            try:
                async with asyncio.timeout(10):
                    while not (root / "close-entered").exists():
                        await asyncio.sleep(0.02)
                opening = asyncio.create_task(open_view(route, "new-view"))
                late = asyncio.create_task(renew(route, "replacement-view", expected=(409,)))
                await asyncio.sleep(0.1)
                assert not opening.done() and not late.done()
            finally:
                blocker.unlink(missing_ok=True)
            assert await closing == "released"
            await opening
            await late
            assert await release(route, "replacement-view") == "inactive"
            await renew(route, "new-view")
            await assert_writer_held(path)

            # The real adapter closes its worker and drops the reply. Host must
            # discard the cached connection before the next ensure/turn.
            (root / "drop-close-reply").touch()
            await host.api(route + "/runtime/release", "POST", {"view_id": "new-view"}, expected=(502,))
            assert (root / "close-executed").exists()
            assert not (await host.api(route))["runtime"]["has_task"]
            await open_view(route, "recovered-view")
            assert native_path(root, route.rsplit("/", 1)[1]) == path
            await host.api(route + "/messages", "POST", {"content": "Continue after missing close ACK"})
            await host.idle(route)
            assert model.count("Continue after missing close ACK") == 1
            if args.studio_root:
                await release(route, "recovered-view")
                tab_route, tab_path = await conversation("Persist the browser tab turn")
                await hold_turn(tab_route)
                password = await host.api("/api/webui/reset-password", "POST")
                account = await host.api("/api/auth/internal/users/system")
                runner = await asyncio.create_subprocess_exec(
                    args.bun, "tests/manual/sessionLockRelease.mjs", str(host.port), tab_route.rsplit("/", 1)[1],
                    cwd=args.studio_root,
                    env={**os.environ, "STUDIO_RELEASE_USER": account["username"],
                         "STUDIO_RELEASE_PASSWORD": password["new_password"]},
                )
                try:
                    assert await asyncio.wait_for(runner.wait(), 50) == 0
                finally:
                    if runner.returncode is None:
                        runner.kill()
                        await runner.wait()
                await assert_writer_held(tab_path)
                model.release.set()
                await wait_until_retired(tab_route)
                acquired, detail = await native_state(args.aelix, host, tab_path)
                assert acquired, detail
                print(json.dumps({"actualTabClosedDuringTurn": True, "cliAcquiredAfterTurnWithoutBrowser": True}))
                await open_view(route, "shutdown-view")

            await host.stop()
            acquired, detail = await native_state(args.aelix, host, path)
            assert acquired, detail
            print(json.dumps({"otherViewProtected": True, "serverDeferredRelease": True,
                              "lateHeartbeatAndDepartureFenced": True, "concurrentEnsureWaits": True,
                              "sameSessionAfterLostCloseReply": True, "hostShutdownReleased": True}))
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
    parser.add_argument("--studio-root", type=Path)
    parser.add_argument("--bun", default="bun")
    asyncio.run(scenario(parser.parse_args()))
