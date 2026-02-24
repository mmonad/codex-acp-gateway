"""
Integration tests: codex-acp-gateway in ACP proxy mode <-> Gemini CLI (ACP).

The gateway runs as a transparent WebSocket-to-stdio bridge.  Messages are
raw ACP JSON-RPC 2.0 — no Codex translation.

Run all:
    cd tests/integration && uv run pytest test_acp_proxy.py

Run one:
    cd tests/integration && uv run pytest test_acp_proxy.py -k test_initialize

Verbose with protocol trace:
    cd tests/integration && uv run pytest test_acp_proxy.py -s -v
"""

from __future__ import annotations

import pytest

from conftest import AcpProxySession


def _skip_on_capacity_error(resp: dict | None):
    """Skip the test if the agent reports a transient capacity error."""
    if resp is None:
        return
    err = resp.get("error", {})
    msg = err.get("message", "")
    if "capacity" in msg.lower() or "overloaded" in msg.lower():
        pytest.skip(f"Gemini server capacity issue: {msg}")


# ── Lifecycle ────────────────────────────────────────────────


class TestAcpLifecycle:
    """ACP initialize handshake and session creation through the proxy."""

    def test_initialize(self, acp_proxy: AcpProxySession):
        result = acp_proxy.initialize()
        # ACP initialize response must include protocolVersion
        assert "protocolVersion" in result, f"missing protocolVersion in {result}"
        assert isinstance(result["protocolVersion"], int)

    def test_initialize_has_capabilities(self, acp_proxy: AcpProxySession):
        result = acp_proxy.initialize()
        assert "agentCapabilities" in result, f"missing capabilities in {result}"

    def test_session_new(self, acp_initialized: AcpProxySession):
        session_id = acp_initialized.new_session()
        assert session_id, "session ID should be non-empty"
        assert len(session_id) > 0

    def test_multiple_sessions(self, acp_initialized: AcpProxySession):
        s1 = acp_initialized.new_session()
        s2 = acp_initialized.new_session()
        assert s1 != s2, "session IDs must be unique"


# ── Basic Prompting ──────────────────────────────────────────


class TestAcpPrompt:
    """Send prompts through the ACP proxy and verify agent responses."""

    def test_basic_prompt(self, acp_session: tuple[AcpProxySession, str]):
        proxy, session_id = acp_session
        result, notifications = proxy.prompt_or_skip(
            session_id, "What is 2+2? Reply with just the number."
        )

        # The prompt response should exist
        assert result is not None

        # Extract agent text from streaming notifications
        text = proxy.collect_agent_text(notifications)
        assert len(text) > 0, "no agent text received in notifications"
        assert "4" in text, f"expected '4' in response, got: {text!r}"

    def test_streaming_chunks(self, acp_session: tuple[AcpProxySession, str]):
        """Verify that session/update notifications stream through the proxy."""
        proxy, session_id = acp_session
        _, notifications = proxy.prompt_or_skip(
            session_id,
            "Explain what a WebSocket is in exactly 2 sentences.",
        )

        # Should have received at least one session/update notification
        updates = [n for n in notifications if n.get("method") == "session/update"]
        assert len(updates) > 0, "no session/update notifications received"

        text = proxy.collect_agent_text(notifications)
        assert len(text) > 20, f"response too short: {text!r}"

    def test_prompt_response_fields(self, acp_session: tuple[AcpProxySession, str]):
        """Verify the prompt response has expected ACP fields."""
        proxy, session_id = acp_session
        result, _ = proxy.prompt_or_skip(
            session_id, "Say hello. One word only."
        )

        # ACP PromptResponse should have at least a sessionId
        assert "sessionId" in result, f"missing sessionId in prompt result: {result}"


# ── Multi-turn ───────────────────────────────────────────────


class TestAcpMultiTurn:
    """Multi-turn conversations through the ACP proxy."""

    def test_session_memory(self, acp_session: tuple[AcpProxySession, str]):
        """Agent remembers context from a previous prompt in the same session."""
        proxy, session_id = acp_session

        # Turn 1 — establish fact
        _, notifs1 = proxy.prompt_or_skip(
            session_id,
            "My favorite number is 42. Remember that. Just acknowledge briefly.",
        )
        text1 = proxy.collect_agent_text(notifs1)
        assert len(text1) > 0, "turn 1 produced no text"

        # Turn 2 — recall
        _, notifs2 = proxy.prompt_or_skip(
            session_id,
            "What is my favorite number? Reply with just the number.",
        )
        text2 = proxy.collect_agent_text(notifs2)
        assert "42" in text2, f"agent didn't remember '42': {text2!r}"


# ── Permission / Tool Use ────────────────────────────────────


class TestAcpPermission:
    """Verify that ACP permission requests flow through the proxy.

    When the agent requests permission (e.g. to run a command), the proxy
    forwards the request to the WS client.  We auto-approve and verify
    the round-trip.
    """

    @pytest.mark.timeout(120)
    def test_permission_request_flows_through(
        self, acp_session: tuple[AcpProxySession, str]
    ):
        """Agent requests permission for a tool call; we approve it."""
        proxy, session_id = acp_session

        # Ask the agent to run a command — this should trigger a permission
        # request from the agent through the proxy.
        proxy.send_request(
            "session/prompt",
            {
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": (
                            "Run this exact shell command: echo hello-proxy\n"
                            "Just run it, nothing else."
                        ),
                    }
                ],
            },
        )

        # Collect messages, auto-approving any permission requests.
        notifications = []
        permission_seen = False
        prompt_response = None
        import time

        deadline = time.time() + 90
        while time.time() < deadline:
            msg = proxy.recv(timeout=max(0.1, deadline - time.time()))
            if msg is None:
                break

            method = msg.get("method", "")

            # Check for capacity errors
            if "error" in msg and "id" in msg:
                err_msg = msg.get("error", {}).get("message", "")
                if "capacity" in err_msg.lower() or "overloaded" in err_msg.lower():
                    pytest.skip(f"Gemini server capacity issue: {err_msg}")
                # Other error — treat as prompt response
                prompt_response = msg
                break

            # Permission request from agent — approve it
            if method == "session/request_permission" and "id" in msg:
                permission_seen = True
                options = msg.get("params", {}).get("options", [])
                # Find the first "allow" option
                allow_id = None
                for opt in options:
                    if opt.get("kind") in ("AllowOnce", "AllowAlways"):
                        allow_id = opt.get("optionId")
                        break
                if allow_id is None and options:
                    allow_id = options[0].get("optionId")

                proxy.send({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": {
                        "outcome": {
                            "type": "Selected",
                            "optionId": allow_id,
                        }
                    },
                })
                continue

            # session/update notification
            if method == "session/update":
                notifications.append(msg)
                continue

            # Prompt response (has our request ID in result)
            if "result" in msg and "id" in msg:
                prompt_response = msg
                break

            notifications.append(msg)

        assert prompt_response is not None, "prompt never completed"

        text = proxy.collect_agent_text(notifications)
        all_text = text.lower()

        # The command output should include our marker — either in agent
        # text or the permission request proved the round-trip works.
        assert permission_seen or "hello-proxy" in all_text, (
            f"expected permission request or command output. text={text!r}"
        )
