"""Host #4 scope 1 probe using real ACP and native processes.

Use ACP 7b485386 and the candidate or pin from ACP's .github/workflows/test.yml.
Build each native workspace with uv sync --all-packages --python 3.12 --locked.
Run with that workspace's Python and an isolated interpreter:

    .venv/bin/python -I -B scripts/verification/smoke_real_acp_transcript.py \
        --host target/debug/aelix-studio-host --acp-source /path/to/acp/src \
        --aelix .venv/bin/aelix --expect transcript --output /tmp/host-4-candidate

Run with --expect legacy for native 83714045. Every run uses a synthetic
loopback model and fresh HOME, Host data, native state and adapter state.
"""
import argparse
import asyncio
import contextlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from smoke_real_acp_contracts import Model  # noqa: E402
from smoke_real_acp_crash import Host, frames  # noqa: E402


META = "aelix/transcript"
DUPLICATE_PROMPT = "Identical input from Host and CLI"


def run_adapter(source, wire, arguments):
    """Record real adapter frames without putting its process outside Host supervision."""
    sys.path.insert(0, source)
    from aelix_acp.server import Server, main

    def record(direction, method=None, session_id=None, update=None, meta=None):
        update = update or {}
        with open(wire, "a", encoding="utf-8") as output:
            output.write(json.dumps({
                "direction": direction, "method": method, "sessionId": session_id,
                "sessionUpdate": update.get("sessionUpdate"), "toolCallId": update.get("toolCallId"),
                "meta": meta if meta is not None else update.get("_meta"),
            }) + "\n")

    dispatch = Server.dispatch
    send = Server.send

    async def traced_dispatch(server, method, params):
        record("host_to_adapter", method, params.get("sessionId"))
        result = await dispatch(server, method, params)
        if method == "session/load":
            record("load_result", method, params.get("sessionId"), meta=result.get("_meta"))
        return result

    async def traced_send(server, message, **kwargs):
        await send(server, message, **kwargs)
        if isinstance(message, dict) and message.get("method") == "session/update":
            params = message["params"]
            record("adapter_to_host", "session/update", params["sessionId"], params["update"])

    Server.dispatch = traced_dispatch
    Server.send = traced_send
    sys.argv = [sys.argv[0], *arguments]
    main()


def rows(root, conversation):
    with contextlib.closing(sqlite3.connect(f"file:{root / 'host/aionui-backend.db'}?mode=ro", uri=True)) as db:
        db.row_factory = sqlite3.Row
        return [dict(row) for row in db.execute(
            "SELECT * FROM messages WHERE conversation_id=? ORDER BY created_at, id", (conversation,)
        )]


def archived(root, conversation):
    with contextlib.closing(sqlite3.connect(f"file:{root / 'host/aionui-backend.db'}?mode=ro", uri=True)) as db:
        return db.execute(
            "SELECT message_id, content, hidden, acp_native_entry_id FROM acp_transcript_display_archive"
            " WHERE conversation_id=? ORDER BY archive_id", (conversation,)
        ).fetchall()


def get_session_file(root, session_id):
    with contextlib.closing(sqlite3.connect(f"file:{root / 'acp/sessions.sqlite3'}?mode=ro", uri=True)) as db:
        path, = db.execute("SELECT file FROM sessions WHERE id=?", (session_id,)).fetchone()
    return path


async def append_cli(aelix, root, path, env):
    # Same RPC entry point as ACP's candidate job, tests/test_native_runtime.py.
    with (root / "cli.log").open("wb") as log:
        cli = await asyncio.create_subprocess_exec(
            str(aelix), "--mode", "rpc", "--offline", "--no-extensions", "--session", path,
            cwd=root / "workspace", env=env, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE, stderr=log,
        )
        received = []
        try:
            for request_id in ("cli-1", "cli-2"):
                request = {"id": request_id, "type": "prompt", "message": DUPLICATE_PROMPT}
                cli.stdin.write((json.dumps(request) + "\n").encode())
                await cli.stdin.drain()
                async with asyncio.timeout(45):
                    while line := await cli.stdout.readline():
                        event = json.loads(line)
                        if event.get("id") == request_id:
                            assert event["success"], event
                        if event["type"] == "message_end" and event.get("transcript"):
                            received.append(event["transcript"])
                        if event["type"] == "agent_end":
                            break
                    else:
                        raise AssertionError("CLI exited before the turn finished")
        finally:
            cli.stdin.close()
            try:
                await asyncio.wait_for(cli.wait(), 10)
            except TimeoutError:
                cli.kill()
                await cli.wait()
        assert cli.returncode == 0
        return received


async def scenario(args):
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="host4-transcript-", dir="/tmp" if os.name != "nt" else None) as directory:
        root = Path(directory).resolve()
        for name in ("home", "agent/agents", "workspace", "host"):
            (root / name).mkdir(parents=True)
        model = Model()
        port = await model.start()
        (root / "agent/models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions", "apiKey": "synthetic-fixture-key",
            "models": [{"id": "one", "name": "one", "reasoning": False, "input": ["text"],
                        "contextWindow": 32768, "maxTokens": 1024}],
        }}}), encoding="utf-8")
        (root / "agent/settings.json").write_text(json.dumps({
            "defaultProvider": "fixture", "defaultModel": "one", "retry": {"enabled": False},
        }), encoding="utf-8")
        (root / "agent/agents/reviewer.md").write_text(
            "---\nname: reviewer\ndescription: Review\ntools: [read]\n---\nReview.\n", encoding="utf-8"
        )
        env = {key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "WINDIR", "TMP", "TEMP") if key in os.environ}
        env.update(
            HOME=str(root / "home"), USERPROFILE=str(root / "home"),
            XDG_CONFIG_HOME=str(root / "home/config"), XDG_DATA_HOME=str(root / "home/data"),
            AELIX_CODING_AGENT_DIR=str(root / "agent"),
            AELIX_SETTINGS_PATH=str(root / "agent/settings.json"), AELIX_AUTH_PATH=str(root / "agent/auth.json"),
            AIONUI_DISABLE_AUTO_UPDATE="1", AIONUI_EXTENSIONS_PATH=str(root / "extensions"),
            AIONUI_EXTENSION_STATES_FILE=str(root / "extension-states.json"),
        )
        wire = root / "wire.jsonl"
        argv = ["-I", "-B", str(Path(__file__).resolve()), "adapter", str(args.acp_source), str(wire),
                "--aelix", str(args.aelix), "--provider", "fixture", "--model", "one",
                "--permission-mode", "yolo", "--enable-agents", "--state-dir", str(root / "acp")]
        host = Host(args.host, root, env)
        try:
            await host.start()
            name = "Host transcript probe"
            await host.api("/api/agents/custom", "POST",
                           {"name": name, "command": sys.executable, "args": argv, "env": [], "icon": "A"})
            assistant = next(a for a in await host.api("/api/assistants") if a["name"] == name)
            created = await host.api("/api/conversations", "POST", {
                "name": "transcript", "assistant": {"id": assistant["id"]},
                "extra": {"workspace": str(root / "workspace"), "custom_workspace": True},
            })
            conversation = created["id"]
            route = "/api/conversations/" + conversation
            await host.api(route + "/runtime/ensure", "POST")
            await host.api(route + "/messages", "POST", {"content": "DELEGATE_NOW"})
            await host.idle(route)
            hidden = await host.api(route + "/messages", "POST", {"content": DUPLICATE_PROMPT, "hidden": True})
            await host.idle(route)
            await host.api(route + "/messages", "POST", {"content": "/help"})
            await host.idle(route)
            live = rows(root, conversation)
            hidden_row = next(row for row in live if row["id"] == hidden["msg_id"])
            assert hidden_row["hidden"]
            assert bool(hidden_row["acp_native_entry_id"]) == (args.expect == "transcript"), hidden_row
            outgoing = [frame for frame in frames(wire) if frame["direction"] == "adapter_to_host"]
            parent = next(frame for frame in outgoing if frame["sessionUpdate"] == "tool_call")
            call_id = parent["toolCallId"]
            session_file = get_session_file(root, parent["sessionId"])
            progress = [frame["meta"]["aelix/subagents"] for frame in outgoing
                        if frame["toolCallId"] == call_id and (frame["meta"] or {}).get("aelix/subagents")]
            assert progress and progress[-1]["children"][0]["state"] == "done", progress
            live_tool = next(row for row in live if row["id"] == call_id)
            assert json.loads(live_tool["content"])["_meta"]["aelix/subagents"] == progress[-1]
            await host.stop()

            model_count = len(model.requests)
            cli_keys = await append_cli(args.aelix, root, session_file, env)
            assert len(model.requests) == model_count + 2
            if args.expect == "transcript":
                assert len(cli_keys) == len({key["entryId"] for key in cli_keys}) == 4, cli_keys
            else:
                assert cli_keys == [], cli_keys

            request_count = len(model.requests)
            probe_start = len(frames(wire))
            pages = []
            history = []
            for index in range(3):
                await host.start()
                await host.api(route + "/runtime/ensure", "POST")
                pages.append((await host.api(route + "/messages"))["items"])
                history.append(rows(root, conversation))
                if index == 0:
                    await host.api(route + "/runtime/restart", "POST")
                    pages.append((await host.api(route + "/messages"))["items"])
                    history.append(rows(root, conversation))
                await host.stop()
            assert all(page == pages[0] for page in pages[1:])
            assert all(saved == history[0] for saved in history[1:])
            assert len(model.requests) == request_count

            replays = frames(wire)[probe_start:]
            assert sum(frame["direction"] == "host_to_adapter" and frame["method"] == "session/load"
                       for frame in replays) == 4, replays
            markers = [(frame["meta"] or {}).get(META) for frame in replays if frame["direction"] == "load_result"]
            assert len(markers) == 4
            assert all(bool(marker) == (args.expect == "transcript") for marker in markers), markers
            if args.expect == "transcript":
                current = history[0]
                matching = [row for row in current if json.loads(row["content"]).get("content") == DUPLICATE_PROMPT]
                assert [row["hidden"] for row in matching] == [1, 0, 0], matching
                assert [row["acp_native_entry_id"] for row in matching] == [
                    hidden_row["acp_native_entry_id"], cli_keys[0]["entryId"], cli_keys[2]["entryId"],
                ]
                for cli_entry in cli_keys:
                    assert sum(row["acp_native_entry_id"] == cli_entry["entryId"] for row in current) == 1, current
                card, = [row for row in current if row["type"] == "acp_tool_call"]
                assert card["status"] == "finish"
                assert json.loads(card["content"])["_meta"]["aelix/subagents"] == progress[-1]
                assert len([row for row in current if row["acp_transcript_source"] == "adapter"]) >= 2
                assert any(json.loads(row["content"]).get("content") == "/help" for row in current)
                archive = archived(root, conversation)
                assert {item[0] for item in archive} == {row["id"] for row in live}
                assert any(item[0] == call_id and item[1] == live_tool["content"] for item in archive)
                assert any(item[0] == hidden["msg_id"] and item[2:] == (1, hidden_row["acp_native_entry_id"])
                           for item in archive)
            else:
                assert live == history[0]
                assert archived(root, conversation) == []
                assert any(frame["sessionUpdate"] == "user_message_chunk" for frame in replays)
                assert not any((frame["meta"] or {}).get(META) for frame in replays)

            return {"mode": args.expect, "loads": len(pages), "model_requests_during_loads": 0,
                    "appended_cli_turns": 2, "visible_cli_inputs": 2 if cli_keys else 0,
                    "tool_subagents_preserved": True, "archive_rows": len(archived(root, conversation))}
        finally:
            await host.stop()
            model.server.close()
            await model.server.wait_closed()
            for name in ("host.log", "wire.jsonl", "cli.log"):
                if (root / name).exists():
                    shutil.copyfile(root / name, args.output / name)


if __name__ == "__main__":
    if sys.argv[1:2] == ["adapter"]:
        run_adapter(sys.argv[2], sys.argv[3], sys.argv[4:])
        sys.exit()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=lambda p: Path(p).resolve(), required=True)
    parser.add_argument("--aelix", type=lambda p: Path(p).resolve(), required=True)
    parser.add_argument("--acp-source", type=lambda p: Path(p).resolve(), required=True)
    parser.add_argument("--expect", choices=("transcript", "legacy"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = asyncio.run(scenario(args))
    report = json.dumps(result, indent=2)
    (args.output / "result.json").write_text(report + "\n", encoding="utf-8")
    print(report)
