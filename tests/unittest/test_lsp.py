"""LSP server integration tests.

Spawns ``konform server`` as a subprocess and exercises the JSON-RPC 2.0 /
LSP 3.17 protocol: initialize handshake, push diagnostics (didOpen →
publishDiagnostics), pull diagnostics (textDocument/diagnostic), and
incremental editing (didChange).
"""

from __future__ import annotations

import contextlib
import json
import pathlib
import queue
import subprocess
import sys
import threading
import time
from typing import IO, Any, cast

import pytest

BINARY_NAME = "konform.exe" if sys.platform == "win32" else "konform"
needs_binary = pytest.mark.skipif(
    not (pathlib.Path(sys.prefix) / "bin" / BINARY_NAME).exists()
    and not (pathlib.Path(sys.prefix) / "Scripts" / BINARY_NAME).exists(),
    reason="Rust binary not compiled — run `hatch run develop` first",
)

DIRTY_SRC = "from os.path import join\n"
CLEAN_SRC = '"""Module."""\nimport os\n'


# ---------------------------------------------------------------------------
# Minimal synchronous LSP client
# ---------------------------------------------------------------------------


class _LspClient:
    """Wraps a konform-server subprocess with blocking send/receive helpers."""

    def __init__(self, proc: subprocess.Popen) -> None:  # type: ignore[type-arg]
        self._proc = proc
        self._stdin: IO[bytes] = cast(IO[bytes], proc.stdin)
        self._stdout: IO[bytes] = cast(IO[bytes], proc.stdout)
        self._inbox: queue.Queue[dict] = queue.Queue()  # type: ignore[type-arg]
        self._next_id = 1
        self._stop = threading.Event()
        t = threading.Thread(target=self._reader_loop, daemon=True)
        t.start()

    # ── wire codec ────────────────────────────────────────────────────────

    @staticmethod
    def _pack(msg: dict) -> bytes:  # type: ignore[type-arg]
        body = json.dumps(msg).encode()
        return f"Content-Length: {len(body)}\r\n\r\n".encode() + body

    def _reader_loop(self) -> None:
        while not self._stop.is_set():
            try:
                hdr = b""
                while not hdr.endswith(b"\r\n\r\n"):
                    ch = self._stdout.read(1)
                    if not ch:
                        return
                    hdr += ch
                n = int(hdr.split(b"Content-Length: ")[1].split(b"\r\n")[0])
                self._inbox.put(json.loads(self._stdout.read(n)))
            except Exception:  # noqa: BLE001
                if not self._stop.is_set():
                    return

    # ── send helpers ──────────────────────────────────────────────────────

    def _write(self, msg: dict) -> None:  # type: ignore[type-arg]
        self._stdin.write(self._pack(msg))
        self._stdin.flush()

    def request(self, method: str, params: Any = None) -> int:
        rid = self._next_id
        self._next_id += 1
        m: dict = {"jsonrpc": "2.0", "id": rid, "method": method}  # type: ignore[type-arg]
        if params is not None:
            m["params"] = params
        self._write(m)
        return rid

    def notify(self, method: str, params: Any = None) -> None:
        m: dict = {"jsonrpc": "2.0", "method": method}  # type: ignore[type-arg]
        if params is not None:
            m["params"] = params
        self._write(m)

    def respond(self, req_id: int, result: Any = None) -> None:
        self._write({"jsonrpc": "2.0", "id": req_id, "result": result})

    # ── receive helpers ───────────────────────────────────────────────────

    def wait_response(self, rid: int, timeout: float = 6.0) -> dict:  # type: ignore[type-arg]
        """Return the response with id == rid; auto-respond to server requests."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            try:
                msg = self._inbox.get(timeout=min(remaining, 0.5))
            except queue.Empty:
                continue
            if msg.get("id") == rid and "method" not in msg:
                return msg
            # Server-issued request (e.g. registerCapability) — null-respond.
            if "id" in msg and "method" in msg:
                self.respond(msg["id"])
        raise AssertionError(f"Timed out waiting for response id={rid}")

    def wait_notification(self, method: str, timeout: float = 6.0) -> dict:  # type: ignore[type-arg]
        """Return the first notification with the given method."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            try:
                msg = self._inbox.get(timeout=min(remaining, 0.5))
            except queue.Empty:
                continue
            if msg.get("method") == method:
                return msg
            if "id" in msg and "method" in msg:
                self.respond(msg["id"])
        raise AssertionError(f"Timed out waiting for notification {method!r}")

    def wait_server_request(self, method: str, timeout: float = 6.0) -> dict:  # type: ignore[type-arg]
        """Return the first server-sent request with the given method.

        Unlike ``wait_notification`` and ``wait_response``, this method does
        **not** auto-respond to the matched message so the caller can supply
        a custom response.  Other server requests that arrive first are
        auto-responded with null; other messages (notifications, responses)
        are re-queued for subsequent calls.
        """
        deadline = time.monotonic() + timeout
        saved: list[dict] = []  # type: ignore[type-arg]
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            try:
                msg = self._inbox.get(timeout=min(remaining, 0.5))
            except queue.Empty:
                continue
            if msg.get("method") == method and "id" in msg:
                # Found the target — re-queue any messages that arrived first.
                for m in saved:
                    self._inbox.put(m)
                return msg
            if "id" in msg and "method" in msg:
                # Another server request — auto-respond with null.
                self.respond(msg["id"])
            else:
                # Notification or response — save for later.
                saved.append(msg)
        for m in saved:
            self._inbox.put(m)
        raise AssertionError(f"Timed out waiting for server request {method!r}")

    # ── lifecycle ─────────────────────────────────────────────────────────

    def shutdown(self, timeout: float = 4.0) -> None:
        with contextlib.suppress(Exception):
            rid = self.request("shutdown")
            self.wait_response(rid, timeout=timeout)
            self.notify("exit")
        self._stop.set()
        with contextlib.suppress(Exception):
            self._stdin.close()
        self._proc.wait(timeout=timeout)


def _start_server(root: pathlib.Path) -> _LspClient:
    proc = subprocess.Popen(
        [sys.executable, "-m", "konform", "server"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        cwd=str(root),
    )
    assert proc.stdin is not None
    assert proc.stdout is not None
    return _LspClient(proc)


def _handshake(client: _LspClient, root: pathlib.Path) -> None:
    """Perform the LSP initialize / initialized handshake."""
    rid = client.request(
        "initialize",
        {
            "processId": None,
            "rootUri": root.as_uri(),
            "capabilities": {},
        },
    )
    response = client.wait_response(rid)
    assert "capabilities" in response.get("result", {}), "missing capabilities in initialize response"
    client.notify("initialized", {})


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


@needs_binary
def test_lsp_initialize_returns_capabilities(tmp_path: pathlib.Path) -> None:
    """initialize response must advertise textDocumentSync and diagnostics."""
    client = _start_server(tmp_path)
    try:
        rid = client.request(
            "initialize",
            {
                "processId": None,
                "rootUri": tmp_path.as_uri(),
                "capabilities": {},
            },
        )
        resp = client.wait_response(rid)
        caps = resp["result"]["capabilities"]
        assert "textDocumentSync" in caps, "server must advertise textDocumentSync"
        # Server advertises either pull or push diagnostics.
        assert "diagnosticProvider" in caps or "codeActionProvider" in caps
    finally:
        client.shutdown()


@needs_binary
def test_lsp_push_diagnostics_on_didopen(tmp_path: pathlib.Path) -> None:
    """Opening a dirty file triggers publishDiagnostics with KIS001."""
    dirty = tmp_path / "check.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )

        notif = client.wait_notification("textDocument/publishDiagnostics")
        diags = notif["params"]["diagnostics"]
        assert len(diags) > 0, "Expected at least one diagnostic"
        assert notif["params"]["uri"] == dirty.as_uri()
        codes = [d.get("code") for d in diags]
        assert "KIS001" in codes, f"Expected KIS001 in {codes}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_clean_file_no_diagnostics(tmp_path: pathlib.Path) -> None:
    """Opening a clean file yields an empty publishDiagnostics."""
    clean = tmp_path / "clean.py"
    clean.write_text(CLEAN_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": clean.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": CLEAN_SRC,
                }
            },
        )

        notif = client.wait_notification("textDocument/publishDiagnostics")
        assert notif["params"]["diagnostics"] == [], "clean file must yield empty diagnostics"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_pull_diagnostics(tmp_path: pathlib.Path) -> None:
    """textDocument/diagnostic pull request returns KIS001 for a dirty file."""
    dirty = tmp_path / "check.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        # Drain the push notification before issuing the pull request.
        client.wait_notification("textDocument/publishDiagnostics")

        pull_rid = client.request(
            "textDocument/diagnostic",
            {"textDocument": {"uri": dirty.as_uri()}},
        )
        resp = client.wait_response(pull_rid)
        items = resp["result"]["items"]
        assert len(items) > 0, "pull diagnostic must return violations"
        codes = [d.get("code") for d in items]
        assert "KIS001" in codes, f"Expected KIS001 in pull diagnostic codes: {codes}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_diagnostic_message_includes_help_text(tmp_path: pathlib.Path) -> None:
    """Diagnostic message includes the rule's help line (style-guide URL).

    Rule documentation is surfaced via the diagnostic bubble rather than a
    competing textDocument/hover handler, so the editor shows it as part of
    the squiggly-line popup without interfering with other LSPs' hover.
    """
    dirty = tmp_path / "check.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        notif = client.wait_notification("textDocument/publishDiagnostics")
        diags = notif["params"]["diagnostics"]
        assert diags, "expected at least one diagnostic"
        msg = diags[0]["message"]
        # The message must contain both the violation description and the help text.
        assert "KIS001" in msg, f"rule code missing from message: {msg!r}"
        assert "\n" in msg, f"message should have two lines (violation + help): {msg!r}"
        # The help line must reference the style guide.
        assert "import" in msg.lower(), f"help text missing from message: {msg!r}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_hover_returns_method_not_found(tmp_path: pathlib.Path) -> None:
    """konform does not advertise hoverProvider and returns MethodNotFound.

    This ensures konform's hover never competes with pyright/pylsp for cursor
    position events — rule documentation is delivered via the diagnostic
    message instead.
    """
    dirty = tmp_path / "check.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        client.wait_notification("textDocument/publishDiagnostics")

        hover_rid = client.request(
            "textDocument/hover",
            {
                "textDocument": {"uri": dirty.as_uri()},
                "position": {"line": 0, "character": 0},
            },
        )
        resp = client.wait_response(hover_rid)
        # Server must respond with an error (MethodNotFound), not a result.
        assert "error" in resp, f"expected an error response, got: {resp}"
        assert resp["error"]["code"] == -32601, f"expected MethodNotFound (-32601): {resp}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_diagnostics_update_on_didchange(tmp_path: pathlib.Path) -> None:
    """Editing a file (didChange full replacement) refreshes diagnostics."""
    f = tmp_path / "edit.py"
    f.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": f.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        notif1 = client.wait_notification("textDocument/publishDiagnostics")
        assert len(notif1["params"]["diagnostics"]) > 0, "dirty content must yield diagnostics"

        # Replace with clean content.
        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": f.as_uri(), "version": 2},
                "contentChanges": [{"text": CLEAN_SRC}],
            },
        )
        notif2 = client.wait_notification("textDocument/publishDiagnostics")
        assert notif2["params"]["diagnostics"] == [], "after fixing the source in-memory, diagnostics must be empty"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_per_violation_code_action_has_edit(tmp_path: pathlib.Path) -> None:
    """Per-violation quickfix code actions carry a real TextEdit, not null."""
    dirty = tmp_path / "fix.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        # Drain push diagnostics so violations are cached in the session.
        client.wait_notification("textDocument/publishDiagnostics")

        # Request code actions at line 0 (where the KIS001 violation lives).
        ca_rid = client.request(
            "textDocument/codeAction",
            {
                "textDocument": {"uri": dirty.as_uri()},
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 0},
                },
                "context": {"diagnostics": []},
            },
        )
        resp = client.wait_response(ca_rid)
        actions = resp["result"]

        quickfixes = [a for a in actions if isinstance(a, dict) and a.get("kind") == "quickfix"]
        assert quickfixes, f"Expected at least one quickfix action, got: {actions}"
        for qf in quickfixes:
            assert qf.get("edit") is not None, f"Per-violation quickfix must carry a real edit, got: {qf}"
            changes = qf["edit"].get("changes", {})
            assert dirty.as_uri() in changes, f"edit.changes must include the document URI, got: {changes.keys()}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_per_violation_code_action_edit_fixes_violation(tmp_path: pathlib.Path) -> None:
    """The TextEdit in a per-violation quickfix actually removes the bad import."""
    dirty = tmp_path / "fix.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        client.wait_notification("textDocument/publishDiagnostics")

        ca_rid = client.request(
            "textDocument/codeAction",
            {
                "textDocument": {"uri": dirty.as_uri()},
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 0},
                },
                "context": {"diagnostics": []},
            },
        )
        resp = client.wait_response(ca_rid)
        quickfixes = [a for a in resp["result"] if isinstance(a, dict) and a.get("kind") == "quickfix"]
        assert quickfixes, "Expected at least one quickfix action"

        # Inspect the TextEdits from the first quickfix.
        edits = quickfixes[0]["edit"]["changes"][dirty.as_uri()]
        assert len(edits) >= 1, f"Expected at least one TextEdit, got: {edits}"
        # At least one edit must remove the bad from-import.
        all_new_text = " ".join(e["newText"] for e in edits)

        # The fixed text must not contain the bad from-import.
        assert "from os.path import" not in all_new_text, (
            f"Fixed text must not contain the bad import: {all_new_text!r}"
        )
        # The fixed text should be a proper module import.
        assert "import" in all_new_text, f"Fixed text should contain an import: {all_new_text!r}"
    finally:
        client.shutdown()


@needs_binary
def test_lsp_did_change_configuration_updates_rules(tmp_path: pathlib.Path) -> None:
    """workspace/didChangeConfiguration triggers rule re-configuration + re-lint.

    Flow:
    1. Open a file with a KIS001 violation → publishDiagnostics lists KIS001.
    2. Send workspace/didChangeConfiguration.
    3. Server sends workspace/configuration to the client; we respond with
       select=["KPT"] (which excludes KIS rules).
    4. Server applies the settings and re-lints → publishDiagnostics is now
       empty (no KPT patterns defined in the tmp workspace).
    """
    dirty = tmp_path / "cfg_test.py"
    dirty.write_text(DIRTY_SRC)

    client = _start_server(tmp_path)
    try:
        _handshake(client, tmp_path)

        # Step 1: open dirty file and verify KIS001 is reported.
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": dirty.as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": DIRTY_SRC,
                }
            },
        )
        notif1 = client.wait_notification("textDocument/publishDiagnostics")
        codes1 = [d.get("code") for d in notif1["params"]["diagnostics"]]
        assert "KIS001" in codes1, f"Expected KIS001 before config change: {codes1}"

        # Step 2: signal that editor configuration has changed.
        client.notify("workspace/didChangeConfiguration", {"settings": {}})

        # Step 3: intercept the server's workspace/configuration request and
        # respond with select=["KPT"] so that KIS rules are excluded.
        cfg_req = client.wait_server_request("workspace/configuration")
        assert cfg_req["params"]["items"][0]["section"] == "konform", (
            f"Server must request the 'konform' section, got: {cfg_req['params']}"
        )
        client.respond(cfg_req["id"], [{"select": ["KPT"]}])

        # Step 4: server re-lints with the new settings; KIS001 must be gone.
        notif2 = client.wait_notification("textDocument/publishDiagnostics")
        codes2 = [d.get("code") for d in notif2["params"]["diagnostics"]]
        assert "KIS001" not in codes2, f'KIS001 must be absent after select=["KPT"] is applied: {codes2}'
    finally:
        client.shutdown()
