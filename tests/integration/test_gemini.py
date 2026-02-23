"""
Integration tests: codex-acp-gateway <-> Gemini CLI (ACP mode).

Each test launches a fresh gateway subprocess connected to ``gemini --experimental-acp``.

Run all:
    cd tests/integration && uv run pytest

Run one:
    cd tests/integration && uv run pytest -k test_basic_prompt

Verbose with protocol trace:
    cd tests/integration && uv run pytest -s -v
"""

from __future__ import annotations

import pytest

from conftest import GatewaySession


# ── Lifecycle ────────────────────────────────────────────────


class TestLifecycle:
    """Initialize handshake and thread/session creation."""

    def test_initialize(self, gateway: GatewaySession):
        result = gateway.initialize()
        assert "userAgent" in result
        assert "codex-acp-gateway" in result["userAgent"]

    def test_thread_start(self, initialized_gateway: GatewaySession):
        thread_id = initialized_gateway.thread_start()
        assert thread_id
        assert len(thread_id) > 10  # UUID

    def test_multiple_threads(self, initialized_gateway: GatewaySession):
        gw = initialized_gateway
        t1 = gw.thread_start()
        t2 = gw.thread_start()
        assert t1 != t2, "thread IDs must be unique"


# ── Basic Prompting ──────────────────────────────────────────


class TestBasicPrompt:
    """Simple text prompts with streaming responses."""

    def test_basic_prompt(self, thread: tuple[GatewaySession, str]):
        gw, thread_id = thread
        events = gw.turn_start(thread_id, "What is 2+2? Reply with just the number.")

        text = "".join(events["deltas"])
        assert events["turn_completed"] is not None, "turn did not complete"
        assert len(text) > 0, "no agent text received"
        assert "4" in text, f"expected '4' in response, got: {text!r}"
        assert events["turn_completed"]["turn"]["status"] == "completed"

    def test_streaming_chunks(self, thread: tuple[GatewaySession, str]):
        """Verify multi-chunk streaming for a longer response."""
        gw, thread_id = thread
        events = gw.turn_start(
            thread_id,
            "Explain what a protocol gateway is in exactly 3 sentences.",
        )

        text = "".join(events["deltas"])
        assert events["turn_completed"] is not None
        assert len(text) > 50, f"response too short for 3 sentences: {text!r}"
        # Should have received at least one chunk
        assert len(events["deltas"]) >= 1


# ── Tool Use ─────────────────────────────────────────────────


class TestToolUse:
    """Tests that exercise ACP tool calls (fs, terminal)."""

    @pytest.mark.timeout(180)
    def test_read_file(self, thread: tuple[GatewaySession, str]):
        """Agent reads a file and reports contents (via fs or shell tools)."""
        gw, thread_id = thread
        events = gw.turn_start(
            thread_id,
            "What is the package name in Cargo.toml? Be brief — just the name.",
            timeout=150,
        )

        text = "".join(events["deltas"])

        # Should have triggered at least one tool interaction — either a
        # tool call item/started, an approval request, or command output.
        has_tool_activity = (
            any(i.get("type") != "agentMessage" for i in events["items_started"])
            or len(events["approvals"]) > 0
            or len(events["output_deltas"]) > 0
            or len(events["items_completed"]) > 0
        )
        assert has_tool_activity, "expected some tool activity (items, approvals, or output)"

        assert events["turn_completed"] is not None, "turn did not complete"
        assert len(text) > 0, "no agent text"
        assert "codex-acp-gateway" in text.lower(), (
            f"agent didn't identify the package name: {text!r}"
        )

    @pytest.mark.timeout(120)
    def test_command_execution_with_approval(
        self, thread: tuple[GatewaySession, str]
    ):
        """Agent runs a shell command; approval is auto-accepted."""
        gw, thread_id = thread
        events = gw.turn_start(
            thread_id,
            "Run this exact shell command: echo hello-from-gateway\n"
            "Just run it and show the output, nothing else.",
        )

        text = "".join(events["deltas"])
        cmd_output = "".join(events["output_deltas"])
        assert events["turn_completed"] is not None, "turn did not complete"

        # Should have had at least one approval request
        assert len(events["approvals"]) > 0, "expected an approval request"

        # The marker should appear in either the agent text or command output
        all_text = text + cmd_output
        assert "hello-from-gateway" in all_text, (
            f"command marker not found. text={text!r} output={cmd_output!r}"
        )


# ── Multi-turn ───────────────────────────────────────────────


class TestMultiTurn:
    """Multi-turn conversations within a single thread/session."""

    def test_session_memory(self, thread: tuple[GatewaySession, str]):
        """Agent remembers context from a previous turn."""
        gw, thread_id = thread

        # Turn 1 — establish fact
        e1 = gw.turn_start(
            thread_id,
            "My favorite color is blue. Remember that. "
            "Just acknowledge briefly.",
        )
        assert e1["turn_completed"] is not None, "turn 1 did not complete"

        # Turn 2 — recall
        e2 = gw.turn_start(
            thread_id,
            "What is my favorite color? Reply with just the color name.",
        )
        text2 = "".join(e2["deltas"])
        assert e2["turn_completed"] is not None, "turn 2 did not complete"
        assert "blue" in text2.lower(), (
            f"agent didn't remember 'blue': {text2!r}"
        )

    def test_two_threads_independent(
        self, initialized_gateway: GatewaySession
    ):
        """Two threads operate independently."""
        gw = initialized_gateway
        t1 = gw.thread_start()
        t2 = gw.thread_start()

        e1 = gw.turn_start(t1, "Say 'hello from thread one'. Nothing else.")
        text1 = "".join(e1["deltas"])

        e2 = gw.turn_start(t2, "Say 'hello from thread two'. Nothing else.")
        text2 = "".join(e2["deltas"])

        assert e1["turn_completed"] is not None
        assert e2["turn_completed"] is not None
        assert "one" in text1.lower() or "1" in text1
        assert "two" in text2.lower() or "2" in text2
