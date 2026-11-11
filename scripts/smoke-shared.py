#!/usr/bin/env python3
"""Live shared-menu validation against schemas from the installed Codex CLI.
Usage: uv run --with jsonschema scripts/smoke-shared.py --config PATH [--live]
"""

import argparse, asyncio, json, os, pathlib, tempfile, subprocess
import jsonschema

parser = argparse.ArgumentParser()
parser.add_argument("--config", required=True)
parser.add_argument("--live", action="store_true")
parser.add_argument("--gateway", default="target/debug/codex-acp-gateway")
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]


class Client:
    def __init__(self, work):
        self.work = work
        self.counter = 0
        self.events = []

    async def start(self):
        env = os.environ.copy()
        env["ACP_GATEWAY_HOME"] = str(self.work / "state")
        self.process = await asyncio.create_subprocess_exec(
            str(root / "scripts/codex-acp-router"),
            str(pathlib.Path(args.config).resolve()),
            str(pathlib.Path(args.gateway).resolve()),
            stdin=-1,
            stdout=-1,
            stderr=self.log,
            env=env,
            cwd=self.work,
        )
        await self.request(
            "initialize",
            {"clientInfo": {"name": "shared_smoke", "version": "1"}},
            "InitializeResponse",
        )

    async def read(self):
        line = await asyncio.wait_for(self.process.stdout.readline(), 120)
        if not line:
            raise RuntimeError("Router EOF; see " + str(self.work / "gateway.log"))
        m = json.loads(line)
        if "method" in m:
            if "id" in m:
                # Only approve the disposable smoke-test fixture operations.
                await self.send({"id": m["id"], "result": {"decision": "accept"}})
            elif not m["method"].startswith("gateway/"):
                jsonschema.validate(m, self.notification_schema)
            self.events.append(m)
        return m

    async def send(self, m):
        self.process.stdin.write((json.dumps(m) + "\n").encode())
        await self.process.stdin.drain()

    async def request(self, method, p, schema=None):
        self.counter += 1
        ident = self.counter
        await self.send({"id": ident, "method": method, "params": p})
        while True:
            m = await self.read()
            if m.get("id") == ident and "method" not in m:
                if "error" in m:
                    raise RuntimeError(m["error"])
                if schema:
                    jsonschema.validate(
                        m["result"],
                        json.loads(
                            next(
                                (self.work / "schemas").rglob(schema + ".json")
                            ).read_text()
                        ),
                    )
                return m["result"]

    async def turn(self, tid, model, prompt, expected, effort="low"):
        start = len(self.events)
        await self.request(
            "turn/start",
            {
                "threadId": tid,
                "model": model,
                "effort": effort,
                "input": [{"type": "text", "text": prompt, "text_elements": []}],
            },
            "TurnStartResponse",
        )
        while True:
            m = await self.read()
            if m.get("method") == "turn/completed":
                assert m["params"]["threadId"] == tid, m
                assert m["params"]["turn"]["status"] == "completed", m
                break
        ev = self.events[start:]
        answers = [
            e["params"]["item"]["text"]
            for e in ev
            if e.get("method") == "item/completed"
            and e["params"]["item"]["type"] == "agentMessage"
        ]
        assert expected in "\n".join(answers), (model, answers)
        print("PASS live", model, repr("\n".join(answers)), flush=True)
        return ev

    async def close(self):
        self.process.stdin.close()
        try:
            await asyncio.wait_for(self.process.wait(), 15)
        except asyncio.TimeoutError:
            self.process.kill()
            await self.process.wait()


async def main():
    work = pathlib.Path(tempfile.mkdtemp(prefix="shared-gateway-smoke-")).resolve()
    print("Artifacts:", work, flush=True)
    (work / "fixture.txt").write_text("SHARED_FILE_91736\n")
    subprocess.run(
        ["codex", "app-server", "generate-json-schema", "--out", str(work / "schemas")],
        check=True,
        capture_output=True,
    )
    c = Client(work)
    c.log = (work / "gateway.log").open("w")
    c.notification_schema = json.loads(
        (work / "schemas/ServerNotification.json").read_text()
    )
    try:
        await c.start()
        rows = (await c.request("model/list", {}, "ModelListResponse"))["data"]
        (work / "catalog.json").write_text(json.dumps(rows, indent=2))
        counts = {
            name: len([r for r in rows if r["id"].startswith(name + "/")])
            for name in ["antigravity", "muse", "claude", "grok"]
        }
        assert (
            counts["antigravity"] == 3
            and counts["muse"] == 5
            and counts["claude"] > 0
            and counts["grok"] > 0
        ), counts
        assert all(
            "max" not in [e["reasoningEffort"] for e in r["supportedReasoningEfforts"]]
            for r in rows
            if r["id"] == "muse/muse-spark-1.2"
        )
        print("PASS shared catalog and per-model efforts", counts, flush=True)
        r = await c.request(
            "thread/start",
            {
                "cwd": str(work),
                "model": "muse/muse-spark-1.3",
                "config": {"model_reasoning_effort": "low"},
            },
            "ThreadStartResponse",
        )
        tid = r["thread"]["id"]
        await c.request(
            "thread/settings/update",
            {"threadId": tid, "model": "muse/muse-wonder-full-1.0", "effort": "max"},
        )
        r = await c.request("thread/resume", {"threadId": tid}, "ThreadResumeResponse")
        assert (
            r["model"] == "muse/muse-wonder-full-1.0" and r["reasoningEffort"] == "max"
        ), r
        print("PASS Muse max effort and resume", flush=True)
        if args.live:
            ev = await c.turn(
                tid,
                "muse/muse-spark-1.3",
                "Use a file reading tool to read fixture.txt. Reply with only its contents.",
                "SHARED_FILE_91736",
            )
            assert any(
                e.get("method") == "item/started"
                and e["params"]["item"]["type"]
                not in ("agentMessage", "userMessage", "reasoning")
                for e in ev
            ), "No Muse tool execution"
            for model in [
                "antigravity/gemini-3.8-flash",
                "claude/haiku",
                "grok/grok-4.7",
            ]:
                await c.turn(
                    tid,
                    model,
                    "What was the exact fixture content from our earlier conversation? Reply with only that content. Do not use tools.",
                    "SHARED_FILE_91736",
                )
            await c.turn(
                tid,
                "muse/muse-spark-1.3",
                "What was the fixture content? Reply with it only. Do not use tools.",
                "SHARED_FILE_91736",
            )
        await c.close()
        await c.start()
        r = await c.request("thread/resume", {"threadId": tid}, "ThreadResumeResponse")
        assert r["model"].startswith("muse/"), r
        if args.live:
            assert len(r["thread"]["turns"]) >= 5, r
            await c.turn(
                tid,
                "muse/muse-spark-1.3",
                "Repeat the fixture content only. No tools.",
                "SHARED_FILE_91736",
            )
        print("PASS gateway restart and durable shared resume", flush=True)
    finally:
        await c.close()
        c.log.close()


asyncio.run(main())
