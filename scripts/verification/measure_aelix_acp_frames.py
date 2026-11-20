#!/usr/bin/env python3
"""Measure standard ACP stdin/stdout on an installed Preview adapter/native pair.

Pass the directory containing Studio's receipt.json and installed venvs, and the
pinned Studio scripts/studio-preview/fixtures/loopback.py. Only byte counts,
bundle identity and wire error codes are printed. All session state is temporary.
"""
import argparse
import asyncio
import base64
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import zlib

MIB = 1024 * 1024
MAX_UNRESIZED_TOOL_IMAGE = (4 * MIB + MIB // 2) * 3 // 4 - 3


def png(size):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    marker = b"\x89PNG\r\n\x1a\n"
    header = chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0))
    pixels = chunk(b"IDAT", zlib.compress(bytes(4)))
    end = chunk(b"IEND", b"")
    padding = size - len(marker + header + pixels + end) - 12
    return marker + header + chunk(b"tEXt", b"note\0" + b"A" * (padding - 5)) + pixels + end


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


class Wire:
    def __init__(self, process):
        self.process = process
        self.id = 0

    async def request(self, method, params):
        self.id += 1
        sent = encode({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        self.process.stdin.write(sent + b"\n")
        await self.process.stdin.drain()
        stats = {"stdinBytes": len(sent), "stdoutMaxBytes": 0}
        async with asyncio.timeout(90):
            while True:
                line = await self.process.stdout.readline()
                if not line:
                    raise RuntimeError("ACP stdout closed before the response")
                message = json.loads(line)
                size = len(line.rstrip(b"\r\n"))
                stats["stdoutMaxBytes"] = max(stats["stdoutMaxBytes"], size)
                update = message.get("params", {}).get("update", {})
                kind = update.get("sessionUpdate")
                if kind:
                    stats[kind] = max(stats.get(kind, 0), size)
                for block in update.get("rawOutput", {}).get("content", []):
                    if block.get("type") == "image":
                        stats["toolImageBytes"] = len(base64.b64decode(block["data"], validate=True))
                        stats["imageToolResultFrameBytes"] = size
                if message.get("id") == self.id:
                    if "error" in message:
                        stats["rpcError"] = message["error"]["code"]
                    return message.get("result"), stats


async def measure(bundle, fixture_module, root):
    spec = importlib.util.spec_from_file_location("preview_fixture", fixture_module)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    fixture = module.ToolFixture(root)
    await fixture.start()
    models = root / "agent/models.json"
    model_data = json.loads(models.read_bytes())
    for model in model_data["providers"]["fixture"]["models"]:
        model["input"].append("image")
    models.write_text(json.dumps(model_data))
    adapter = bundle / "adapter/bin/aelix-acp"
    native = bundle / "native/bin/aelix"
    process = await asyncio.create_subprocess_exec(
        adapter, "--aelix", native, "--state-dir", root / "adapter-state", "--offline",
        "--permission-mode", "yolo", "--provider", "fixture", "--model", "one",
        env=fixture.env, cwd=root / "workspace",
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.DEVNULL, limit=64 * MIB,
    )
    try:
        wire = Wire(process)
        initialization, _ = await wire.request("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        assert initialization["agentInfo"]["name"] == "aelix-acp"
        base = {"cwd": str(root / "workspace"), "mcpServers": []}
        opened, _ = await wire.request("session/new", base)
        sid = {"sessionId": opened["sessionId"]}
        image = {"type": "image", "mimeType": "image/png", "data": base64.b64encode(png(8 * MIB)).decode()}
        resource = {"type": "resource", "resource": {
            "uri": "file:///synthetic.txt", "mimeType": "text/plain", "text": "",
        }}
        # The pinned adapter counts the line separator as part of its inbound
        # limit. Fill the legal remaining bytes with ASCII resource data.
        envelope = {"jsonrpc": "2.0", "id": wire.id + 1, "method": "session/prompt",
                    "params": {**sid, "prompt": [resource]}}
        full_resource_size = 16 * MIB - 1 - len(encode(envelope))
        resource["resource"]["text"] = "a" * (full_resource_size - 4096)
        (root / "workspace/probe.png").write_bytes(png(MAX_UNRESIZED_TOOL_IMAGE))
        text = {"type": "text", "text": "Probe 한글 with \"quote\" and \\backslash."}
        measurements = {}
        for name, blocks in (("text", [text]), ("maxPromptImage", [image]), ("maxEmbeddedResource", [resource]),
                             ("imageToolResult", [text])):
            if name == "imageToolResult":
                fixture.next_tool = ("read", {"path": "probe.png"})
            outcome, stats = await wire.request("session/prompt", {**sid, "prompt": blocks})
            assert outcome and outcome["stopReason"] == "end_turn", (name, stats)
            measurements[name] = stats
        envelope["id"] = wire.id + 1
        resource["resource"]["text"] = ""
        resource["resource"]["text"] = "a" * (16 * MIB - 1 - len(encode(envelope)))
        _, measurements["embeddedResourceAtAdapterLimit"] = await wire.request("session/prompt", {**sid, "prompt": [resource]})
        _, measurements["loadReplay"] = await wire.request("session/load", {**base, **sid})
        assert "rpcError" not in measurements["loadReplay"], measurements["loadReplay"]
        assert measurements["imageToolResult"]["toolImageBytes"] == MAX_UNRESIZED_TOOL_IMAGE
        receipt = json.loads((bundle / "receipt.json").read_bytes())
        return {
            "target": receipt["target"], "acp": receipt["acp"]["commit"],
            "acpWheelSha256": receipt["acpWheelSha256"], "native": receipt["native"]["sourceTag"]["commit"],
            "nativeWheels": receipt["native"]["wheels"], "measurements": measurements,
        }
    finally:
        process.stdin.close()
        try:
            await asyncio.wait_for(process.wait(), 10)
        except TimeoutError:
            process.kill()
            await process.wait()
        await fixture.close()


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__ + " Requires Python 3.11+.")
    cli.add_argument("--bundle", type=Path, required=True)
    cli.add_argument("--fixture", type=Path, required=True)
    args = cli.parse_args()
    with tempfile.TemporaryDirectory(prefix="host32-frames-") as temporary:
        report = asyncio.run(measure(args.bundle.resolve(), args.fixture.resolve(), Path(temporary)))
    print(json.dumps(report, ensure_ascii=False, indent=2))
