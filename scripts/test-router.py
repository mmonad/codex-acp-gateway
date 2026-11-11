#!/usr/bin/env python3
"""Offline router checks: ID isolation, handoffs, and switch failure handling."""

import copy
import importlib.machinery
import os
import pathlib
import tempfile
import unittest
import uuid
from unittest.mock import patch

module = importlib.machinery.SourceFileLoader(
    "gateway_router", str(pathlib.Path(__file__).with_name("codex-acp-router"))
).load_module()


class FakeBackend:
    def __init__(self):
        self.calls = []
        self.reject = False

    async def start(self):
        pass

    async def call(self, method, params):
        self.calls.append((method, copy.deepcopy(params)))
        if method == "thread/start":
            return {"thread": {"id": str(uuid.uuid4())}}
        if method == "thread/settings/update" and self.reject:
            raise RuntimeError("Unsupported effort")
        return {}

    async def send(self, msg):
        self.calls.append(("reply", msg))


class RouterTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        with patch.dict(os.environ, {"ACP_GATEWAY_HOME": self.tmp.name}):
            self.router = module.Router(
                {
                    "default_agent": "a",
                    "agents": {
                        "a": {"command": "unused", "model": "one"},
                        "b": {"command": "unused", "model": "two"},
                    },
                },
                "unused",
            )
        self.router.backend = {n: FakeBackend() for n in ("a", "b")}
        self.tid = str(uuid.uuid4())
        self.thread = {
            "id": self.tid,
            "cwd": self.tmp.name,
            "active": "a",
            "model": "a/one",
            "bindings": {"a": {"id": "private-a", "seen": 2}},
            "history": [
                {"role": "user", "text": "remember TOKEN"},
                {"role": "assistant", "text": "TOKEN"},
            ],
            "turns": [],
            "busy": False,
        }
        self.router.threads[self.tid] = self.thread
        self.router.bindings[("a", "private-a")] = self.tid

    def tearDown(self):
        self.tmp.cleanup()

    async def test_picker_tolerates_inherited_openai_tier_but_rejects_tier_only(self):
        await self.router.request(
            "thread/settings/update",
            {
                "threadId": self.tid,
                "model": "b/two",
                "effort": "low",
                "serviceTier": "fast",
            },
        )
        self.assertEqual(self.thread["model"], "b/two")
        self.assertNotIn("serviceTier", self.router.backend["b"].calls[-1][1])
        with self.assertRaises(ValueError):
            await self.router.request(
                "thread/settings/update", {"threadId": self.tid, "serviceTier": "fast"}
            )

    async def test_rejected_switch_preserves_active_agent(self):
        self.router.backend["b"].reject = True
        with self.assertRaises(RuntimeError):
            await self.router.select(
                self.thread, {"model": "b/two", "effort": "invalid"}
            )
        self.assertEqual(self.thread["active"], "a")
        self.assertEqual(self.thread["model"], "a/one")

    async def test_handoff_contains_only_unseen_text(self):
        await self.router.request(
            "turn/start",
            {
                "threadId": self.tid,
                "model": "b/two",
                "input": [{"type": "text", "text": "repeat it"}],
            },
        )
        call = self.router.backend["b"].calls[-1]
        self.assertEqual(call[0], "turn/start")
        self.assertIn("TOKEN", call[1]["input"][0]["text"])
        self.assertEqual(call[1]["input"][1]["text"], "repeat it")
        self.assertNotEqual(call[1]["threadId"], self.tid)

    async def test_approval_ids_do_not_collide_across_agents(self):
        with patch.object(module, "emit") as emit:
            for n in ("a", "b"):
                await self.router.event(
                    n,
                    {
                        "method": "item/commandExecution/requestApproval",
                        "id": 1,
                        "params": {},
                    },
                )
            ids = [c.args[0]["id"] for c in emit.call_args_list]
        self.assertNotEqual(*ids)
        for ident in ids:
            await self.router.dispatch({"id": ident, "result": {"decision": "decline"}})
        for n in ("a", "b"):
            self.assertEqual(self.router.backend[n].calls[-1][1]["id"], 1)

    async def test_busy_switch_is_rejected_without_creating_thread(self):
        self.thread["busy"] = True
        with self.assertRaises(ValueError):
            await self.router.select(self.thread, {"model": "b/two"})
        self.assertEqual(self.router.backend["b"].calls, [])

    async def test_internal_thread_notifications_are_not_exposed(self):
        with patch.object(module, "emit") as emit:
            await self.router.event(
                "b",
                {"method": "thread/started", "params": {"thread": {"id": "private-b"}}},
            )
            emit.assert_not_called()

    def test_non_reasoning_model_drops_inherited_effort(self):
        self.router.models = {"b/two": {"supportedReasoningEfforts": []}}
        q = self.router.normalize_effort(
            {
                "effort": "high",
                "config": {"model_reasoning_effort": "high"},
                "collaborationMode": None,
            },
            "b/two",
        )
        self.assertNotIn("effort", q)
        self.assertNotIn("model_reasoning_effort", q["config"])


if __name__ == "__main__":
    unittest.main()
