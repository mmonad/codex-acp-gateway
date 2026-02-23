"""Shared fixtures for codex-acp-gateway integration tests."""

from __future__ import annotations

import json
import os
import select
import socket
import subprocess
import sys
import threading
import time
from typing import Optional

import pytest
import websocket as ws_client

# ── Paths ────────────────────────────────────────────────────

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
GATEWAY_BIN = os.path.join(REPO_ROOT, "target", "debug", "codex-acp-gateway")
GATEWAY_BIN_RELEASE = os.path.join(REPO_ROOT, "target", "release", "codex-acp-gateway")


def _gateway_bin() -> str:
    """Return path to the gateway binary, preferring release if available."""
    for path in (GATEWAY_BIN_RELEASE, GATEWAY_BIN):
        if os.path.exists(path):
            return path
    pytest.skip(
        f"Gateway binary not found. Run: cargo build -p codex-acp-gateway",
        allow_module_level=True,
    )


# ── ANSI helpers (for -s / captured output) ──────────────────

GREEN = "\033[32m"
RED = "\033[31m"
YELLOW = "\033[33m"
CYAN = "\033[36m"
DIM = "\033[2m"
BOLD = "\033[1m"
RESET = "\033[0m"


def _log(msg: str, color: str = ""):
    sys.stderr.write(f"{color}{msg}{RESET}\n")
    sys.stderr.flush()


def _log_send(msg: dict):
    compact = json.dumps(msg, separators=(",", ":"))
    if len(compact) > 120:
        compact = compact[:117] + "..."
    _log(f"  >>> {compact}", DIM)


def _log_recv(msg: dict):
    method = msg.get("method", "")
    if "result" in msg:
        keys = (
            list(msg["result"].keys())
            if isinstance(msg.get("result"), dict)
            else "scalar"
        )
        _log(f"  <<< response id={msg.get('id')} (result keys: {keys})", DIM)
    elif "error" in msg:
        _log(
            f"  <<< error id={msg.get('id')}: {msg['error'].get('message', '')}",
            RED,
        )
    elif method:
        params = msg.get("params", {})
        extra = ""
        if "delta" in params:
            delta = params["delta"]
            if len(delta) > 60:
                delta = delta[:57] + "..."
            extra = f" delta={repr(delta)}"
        elif "item" in params:
            extra = f" item.type={params['item'].get('type', '?')}"
        _log(f"  <<< {method}{extra}", DIM)


# ── Networking helpers ───────────────────────────────────────


def _find_free_port() -> int:
    """Find an available TCP port on localhost."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# ── Shared stderr reader ─────────────────────────────────────


def _start_stderr_reader(proc, stderr_lines: list) -> threading.Thread:
    """Spawn a daemon thread that drains the process stderr."""

    def _read():
        for line in proc.stderr:
            decoded = line.decode().rstrip()
            stderr_lines.append(decoded)
            if any(kw in decoded for kw in ["ERROR", "WARN", "panic"]):
                _log(f"  [gw] {decoded}", YELLOW)

    t = threading.Thread(target=_read, daemon=True)
    t.start()
    return t


# ── Protocol mixin ───────────────────────────────────────────


class _ProtocolMixin:
    """Codex protocol helpers shared by stdio and WebSocket sessions.

    Subclasses must implement ``send(msg)`` and ``recv(timeout)`` for their
    transport layer.
    """

    _next_id: int
    stderr_lines: list

    def send(self, msg: dict):
        raise NotImplementedError

    def recv(self, timeout: float = 30) -> Optional[dict]:
        raise NotImplementedError

    # ── protocol helpers ──────────────────────────────────────

    def send_request(self, method: str, params: dict | None = None) -> int:
        """Send a request and return its ID."""
        req_id = self._next_id
        self._next_id += 1
        msg: dict = {"id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)
        return req_id

    def send_notification(self, method: str, params: dict | None = None):
        msg: dict = {"method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)

    def recv_until_response(
        self, request_id: int, timeout: float = 30
    ) -> tuple[Optional[dict], list[dict]]:
        """Read messages until we get the response for *request_id*.

        Returns ``(response, collected_notifications)``.
        """
        notifications: list[dict] = []
        deadline = time.time() + timeout
        while time.time() < deadline:
            remaining = deadline - time.time()
            msg = self.recv(timeout=max(0.1, remaining))
            if msg is None:
                break
            if msg.get("id") == request_id and ("result" in msg or "error" in msg):
                return msg, notifications
            notifications.append(msg)
        return None, notifications

    def collect_turn(self, timeout: float = 90, auto_approve: bool = True) -> dict:
        """Collect all notifications for a turn until ``turn/completed``.

        Returns a dict with categorised events.
        """
        result: dict = {
            "deltas": [],
            "thoughts": [],
            "items_started": [],
            "items_completed": [],
            "output_deltas": [],
            "approvals": [],
            "plan_updates": [],
            "turn_completed": None,
            "other": [],
        }

        deadline = time.time() + timeout
        while time.time() < deadline:
            remaining = deadline - time.time()
            msg = self.recv(timeout=max(0.1, remaining))
            if msg is None:
                break

            method = msg.get("method", "")
            params = msg.get("params", {})

            # Response to turn/start — skip
            if "result" in msg and msg.get("id"):
                continue

            if method == "item/agentMessage/delta":
                result["deltas"].append(params.get("delta", ""))

            elif method == "item/reasoning/summaryTextDelta":
                result["thoughts"].append(params.get("delta", ""))

            elif method == "item/started":
                result["items_started"].append(params.get("item", {}))

            elif method == "item/completed":
                result["items_completed"].append(params.get("item", {}))

            elif method == "item/commandExecution/outputDelta":
                result["output_deltas"].append(params.get("delta", ""))

            elif method in (
                "item/commandExecution/requestApproval",
                "item/fileChange/requestApproval",
            ):
                result["approvals"].append(msg)
                if auto_approve:
                    self.send({"id": msg["id"], "result": {"decision": "accept"}})

            elif method == "turn/plan/updated":
                result["plan_updates"].append(params)

            elif method == "turn/started":
                pass  # expected

            elif method == "turn/completed":
                result["turn_completed"] = params
                break

            else:
                result["other"].append(msg)

        return result

    # ── high-level actions ────────────────────────────────────

    def initialize(self) -> dict:
        """Perform the full initialize handshake. Returns the result."""
        req_id = self.send_request(
            "initialize",
            {"clientInfo": {"name": "test-client", "version": "0.0.1"}},
        )
        resp, _ = self.recv_until_response(req_id, timeout=30)
        assert resp is not None, "initialize timed out"
        assert "result" in resp, f"initialize failed: {resp.get('error', {})}"
        self.send_notification("initialized")
        time.sleep(0.3)
        return resp["result"]

    def thread_start(self) -> str:
        """Create a new thread. Returns the thread ID."""
        req_id = self.send_request("thread/start", {})
        resp, _ = self.recv_until_response(req_id, timeout=15)
        assert resp is not None, "thread/start timed out"
        assert "result" in resp, f"thread/start failed: {resp.get('error', {})}"
        return resp["result"]["thread"]["id"]

    def turn_start(self, thread_id: str, text: str, **kwargs) -> dict:
        """Start a turn with a text prompt. Returns collected turn events."""
        self.send_request(
            "turn/start",
            {
                "threadId": thread_id,
                "input": [{"type": "text", "text": text}],
            },
        )
        return self.collect_turn(**kwargs)

    def turn_interrupt(self, thread_id: str) -> Optional[dict]:
        req_id = self.send_request("turn/interrupt", {"threadId": thread_id})
        resp, _ = self.recv_until_response(req_id, timeout=10)
        return resp


# ── GatewaySession (stdio) ───────────────────────────────────


class GatewaySession(_ProtocolMixin):
    """Manages a gateway subprocess via stdio (NDJSON on stdin/stdout)."""

    def __init__(self, proc: subprocess.Popen):
        self.proc = proc
        self._next_id = 1
        self.stderr_lines: list = []
        self._stderr_thread: Optional[threading.Thread] = None

    @classmethod
    def start(
        cls,
        agent_cmd: str = "gemini",
        agent_args: list[str] | None = None,
        log_level: str = "info",
    ) -> GatewaySession:
        if agent_args is None:
            agent_args = ["--experimental-acp"]

        cmd = [
            _gateway_bin(),
            "--agent-cmd",
            agent_cmd,
            "--log-level",
            log_level,
            "--",
            *agent_args,
        ]

        proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        session = cls(proc)
        session._stderr_thread = _start_stderr_reader(proc, session.stderr_lines)

        # Give the gateway + agent a moment to initialise
        time.sleep(2)
        return session

    def close(self):
        self.proc.stdin.close()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()

    # ── transport I/O ─────────────────────────────────────────

    def send(self, msg: dict):
        """Send a Codex protocol message (NDJSON line)."""
        _log_send(msg)
        line = json.dumps(msg, separators=(",", ":")) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()

    def recv(self, timeout: float = 30) -> Optional[dict]:
        """Read one NDJSON message from gateway stdout."""
        ready, _, _ = select.select([self.proc.stdout], [], [], timeout)
        if ready:
            raw = self.proc.stdout.readline()
            if raw:
                msg = json.loads(raw.decode().strip())
                _log_recv(msg)
                return msg
        return None


# ── WsGatewaySession (WebSocket) ─────────────────────────────


class WsGatewaySession(_ProtocolMixin):
    """Manages a gateway via WebSocket transport.

    Each instance owns one WebSocket connection. If ``_owns_proc`` is True
    (default when using ``start()``), closing the session also terminates the
    gateway subprocess.
    """

    def __init__(
        self,
        ws: ws_client.WebSocket,
        proc: Optional[subprocess.Popen] = None,
        port: int = 0,
    ):
        self.ws = ws
        self.proc = proc
        self.port = port
        self._owns_proc = proc is not None
        self._next_id = 1
        self.stderr_lines: list = []
        self._stderr_thread: Optional[threading.Thread] = None

    @classmethod
    def start(
        cls,
        agent_cmd: str = "gemini",
        agent_args: list[str] | None = None,
        log_level: str = "info",
    ) -> WsGatewaySession:
        """Start a gateway process in WS mode and connect one client."""
        if agent_args is None:
            agent_args = ["--experimental-acp"]

        port = _find_free_port()
        listen_url = f"ws://127.0.0.1:{port}"

        cmd = [
            _gateway_bin(),
            "--listen",
            listen_url,
            "--agent-cmd",
            agent_cmd,
            "--log-level",
            log_level,
            "--",
            *agent_args,
        ]

        proc = subprocess.Popen(cmd, stderr=subprocess.PIPE)

        stderr_lines: list = []
        stderr_thread = _start_stderr_reader(proc, stderr_lines)

        # Retry WebSocket connection until the gateway is ready.
        # Using an actual WS handshake instead of a raw TCP probe avoids
        # triggering spurious "handshake failed" warnings in the gateway.
        ws = None
        for _attempt in range(30):
            try:
                ws = ws_client.create_connection(listen_url, timeout=2)
                break
            except (ConnectionRefusedError, ConnectionResetError, OSError):
                time.sleep(0.3)

        if ws is None:
            proc.kill()
            proc.wait()
            raise RuntimeError(
                f"Gateway did not start listening on {listen_url} within 10s. "
                f"stderr: {stderr_lines[-5:]}"
            )

        session = cls(ws=ws, proc=proc, port=port)
        session.stderr_lines = stderr_lines
        session._stderr_thread = stderr_thread
        return session

    def connect_another(self) -> WsGatewaySession:
        """Open a second WebSocket connection to the same gateway process.

        The returned session does NOT own the subprocess — closing it only
        closes the WebSocket connection.
        """
        ws = ws_client.create_connection(
            f"ws://127.0.0.1:{self.port}", timeout=30,
        )
        session = WsGatewaySession(ws=ws, proc=None, port=self.port)
        # Share stderr for logging visibility
        session.stderr_lines = self.stderr_lines
        return session

    def close(self):
        try:
            self.ws.close()
        except Exception:
            pass
        if self._owns_proc and self.proc is not None:
            try:
                self.proc.terminate()
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    # ── transport I/O ─────────────────────────────────────────

    def send(self, msg: dict):
        """Send a Codex protocol message as a WebSocket Text frame."""
        _log_send(msg)
        # One JSON per frame — no newline (unlike NDJSON)
        self.ws.send(json.dumps(msg, separators=(",", ":")))

    def recv(self, timeout: float = 30) -> Optional[dict]:
        """Receive one JSON-RPC message from the WebSocket."""
        self.ws.settimeout(timeout)
        try:
            data = self.ws.recv()
            if data:
                msg = json.loads(data)
                _log_recv(msg)
                return msg
        except ws_client.WebSocketTimeoutException:
            return None
        except (ws_client.WebSocketConnectionClosedException, ConnectionError):
            return None
        return None


# ── Pytest Fixtures: Stdio ───────────────────────────────────


@pytest.fixture
def gateway():
    """Yield a started stdio GatewaySession, closed automatically after the test."""
    gw = GatewaySession.start()
    yield gw
    gw.close()


@pytest.fixture
def gateway_debug():
    """Same as gateway but with debug logging."""
    gw = GatewaySession.start(log_level="debug")
    yield gw
    gw.close()


@pytest.fixture
def initialized_gateway(gateway):
    """A stdio gateway that has already completed the initialize handshake."""
    gateway.initialize()
    return gateway


@pytest.fixture
def thread(initialized_gateway) -> tuple[GatewaySession, str]:
    """An initialized stdio gateway with one thread already created.

    Returns ``(gateway, thread_id)``.
    """
    thread_id = initialized_gateway.thread_start()
    return initialized_gateway, thread_id


# ── Pytest Fixtures: WebSocket ───────────────────────────────


@pytest.fixture
def ws_gateway():
    """Yield a started WsGatewaySession, closed automatically after the test."""
    gw = WsGatewaySession.start()
    yield gw
    gw.close()


@pytest.fixture
def ws_gateway_debug():
    """Same as ws_gateway but with debug logging."""
    gw = WsGatewaySession.start(log_level="debug")
    yield gw
    gw.close()


@pytest.fixture
def ws_initialized_gateway(ws_gateway):
    """A WS gateway that has already completed the initialize handshake."""
    ws_gateway.initialize()
    return ws_gateway


@pytest.fixture
def ws_thread(ws_initialized_gateway) -> tuple[WsGatewaySession, str]:
    """An initialized WS gateway with one thread already created.

    Returns ``(gateway, thread_id)``.
    """
    thread_id = ws_initialized_gateway.thread_start()
    return ws_initialized_gateway, thread_id
