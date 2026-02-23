"""
Integration tests: codex-acp-gateway WebSocket transport <-> Gemini CLI (ACP mode).

Each test launches a fresh gateway with ``--listen ws://127.0.0.1:<port>`` and
connects via WebSocket instead of stdio.

Run all WS tests:
    cd tests/integration && uv run pytest test_gemini_ws.py

Run one:
    cd tests/integration && uv run pytest test_gemini_ws.py -k test_initialize_ws

Verbose with protocol trace:
    cd tests/integration && uv run pytest test_gemini_ws.py -s -v
"""

from __future__ import annotations

import pytest

from conftest import WsGatewaySession


# ── Lifecycle ────────────────────────────────────────────────


class TestLifecycleWs:
    """Initialize handshake and thread/session creation over WebSocket."""

    def test_initialize_ws(self, ws_gateway: WsGatewaySession):
        result = ws_gateway.initialize()
        assert "userAgent" in result
        assert "codex-acp-gateway" in result["userAgent"]

    def test_thread_start_ws(self, ws_initialized_gateway: WsGatewaySession):
        thread_id = ws_initialized_gateway.thread_start()
        assert thread_id
        assert len(thread_id) > 10  # UUID

    def test_multiple_threads_ws(self, ws_initialized_gateway: WsGatewaySession):
        gw = ws_initialized_gateway
        t1 = gw.thread_start()
        t2 = gw.thread_start()
        assert t1 != t2, "thread IDs must be unique"


# ── Basic Prompting ──────────────────────────────────────────


class TestBasicPromptWs:
    """Simple text prompts with streaming responses over WebSocket."""

    def test_basic_prompt_ws(self, ws_thread: tuple[WsGatewaySession, str]):
        gw, thread_id = ws_thread
        events = gw.turn_start(thread_id, "What is 2+2? Reply with just the number.")

        text = "".join(events["deltas"])
        assert events["turn_completed"] is not None, "turn did not complete"
        assert len(text) > 0, "no agent text received"
        assert "4" in text, f"expected '4' in response, got: {text!r}"
        assert events["turn_completed"]["turn"]["status"] == "completed"

    def test_streaming_chunks_ws(self, ws_thread: tuple[WsGatewaySession, str]):
        """Verify multi-chunk streaming for a longer response."""
        gw, thread_id = ws_thread
        events = gw.turn_start(
            thread_id,
            "Explain what a protocol gateway is in exactly 3 sentences.",
        )

        text = "".join(events["deltas"])
        assert events["turn_completed"] is not None
        assert len(text) > 50, f"response too short for 3 sentences: {text!r}"
        assert len(events["deltas"]) >= 1


# ── Tool Use ─────────────────────────────────────────────────


class TestToolUseWs:
    """Tests that exercise ACP tool calls over WebSocket."""

    @pytest.mark.timeout(120)
    def test_command_execution_with_approval_ws(
        self, ws_thread: tuple[WsGatewaySession, str]
    ):
        """Agent runs a shell command; approval is auto-accepted."""
        gw, thread_id = ws_thread
        events = gw.turn_start(
            thread_id,
            "Run this exact shell command: echo hello-from-ws\n"
            "Just run it and show the output, nothing else.",
        )

        text = "".join(events["deltas"])
        cmd_output = "".join(events["output_deltas"])
        assert events["turn_completed"] is not None, "turn did not complete"

        assert len(events["approvals"]) > 0, "expected an approval request"

        all_text = text + cmd_output
        assert "hello-from-ws" in all_text, (
            f"command marker not found. text={text!r} output={cmd_output!r}"
        )


# ── Multi-turn ───────────────────────────────────────────────


class TestMultiTurnWs:
    """Multi-turn conversations within a single thread/session over WebSocket."""

    def test_session_memory_ws(self, ws_thread: tuple[WsGatewaySession, str]):
        """Agent remembers context from a previous turn."""
        gw, thread_id = ws_thread

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


# ── Multi-Client (WebSocket-specific) ────────────────────────


class TestMultiClient:
    """Multiple WebSocket clients connected to the same gateway simultaneously.

    Each WS client gets its own ACP agent subprocess — they operate fully
    independently.
    """

    def test_two_clients_independent(self):
        """Two WS clients have independent sessions and agents."""
        gw1 = WsGatewaySession.start()
        try:
            gw2 = gw1.connect_another()
            try:
                # Both clients initialize independently
                r1 = gw1.initialize()
                r2 = gw2.initialize()
                assert "userAgent" in r1
                assert "userAgent" in r2

                # Both create threads
                t1 = gw1.thread_start()
                t2 = gw2.thread_start()
                assert t1 != t2, "thread IDs should differ across clients"

                # Prompt each independently
                e1 = gw1.turn_start(
                    t1, "Say 'hello from client one'. Nothing else."
                )
                e2 = gw2.turn_start(
                    t2, "Say 'hello from client two'. Nothing else."
                )

                text1 = "".join(e1["deltas"])
                text2 = "".join(e2["deltas"])

                assert e1["turn_completed"] is not None, "client 1 turn did not complete"
                assert e2["turn_completed"] is not None, "client 2 turn did not complete"
                assert "one" in text1.lower() or "1" in text1, (
                    f"client 1 response unexpected: {text1!r}"
                )
                assert "two" in text2.lower() or "2" in text2, (
                    f"client 2 response unexpected: {text2!r}"
                )
            finally:
                gw2.close()
        finally:
            gw1.close()

    def test_client_disconnect_does_not_affect_other(self):
        """Disconnecting one WS client doesn't crash the gateway or the other client."""
        gw1 = WsGatewaySession.start()
        try:
            gw2 = gw1.connect_another()

            # Both initialize
            gw1.initialize()
            gw2.initialize()

            t1 = gw1.thread_start()
            t2 = gw2.thread_start()

            # Client 2 prompts then disconnects
            e2 = gw2.turn_start(t2, "Say 'ack'. Nothing else.")
            assert e2["turn_completed"] is not None
            gw2.close()

            # Client 1 should still work fine
            e1 = gw1.turn_start(
                t1, "What is 3+3? Reply with just the number."
            )
            text1 = "".join(e1["deltas"])
            assert e1["turn_completed"] is not None, "client 1 turn failed after client 2 disconnect"
            assert "6" in text1, f"expected '6' in response: {text1!r}"
        finally:
            gw1.close()
