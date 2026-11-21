#!/usr/bin/env python3
"""Exercise actual zchat CLI, MCP and zplugin artifacts against loopback Matrix.

No IM account or credentials are used. A failed assertion exits nonzero and still
prints a JSON receipt. The caller supplies artifacts built from the same source.
"""

from __future__ import annotations

import argparse
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import stat
import subprocess
import sys
import tempfile
import threading
from typing import Any
from urllib.parse import quote, unquote, urlsplit


PLAIN = "!plain:fixture.invalid"
ENCRYPTED = "!encrypted:fixture.invalid"
FORBIDDEN = "!forbidden:fixture.invalid"
UNICODE_TEXT = "你好，世界 👋 — café"
TOKEN = "loopback-fixture-token-not-a-real-credential"
MAX_FRAME = 2 * 1024 * 1024


class AcceptanceError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AcceptanceError(message)


def sha256(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def encoded(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def private_json(path: Path, value: Any) -> None:
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as stream:
        stream.write(encoded(value))
    if os.name == "posix":
        require(stat.S_IMODE(path.stat().st_mode) == 0o600, "Fixture settings must have mode 0600")


class MatrixFixture:
    """Only documented fixture routes exist; no forwarding or real-network code."""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.counts: Counter[str] = Counter()
        self.transactions: dict[tuple[str, str], tuple[Any, str]] = {}
        self.unhandled: list[str] = []
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args: Any) -> None:
                pass

            def do_GET(self) -> None:
                self.serve()

            def do_PUT(self) -> None:
                self.serve()

            def do_POST(self) -> None:
                self.serve()

            def serve(self) -> None:
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    if length < 0 or length > MAX_FRAME:
                        self.reply(413, {"errcode": "M_TOO_LARGE"})
                        return
                    body = json.loads(self.rfile.read(length)) if length else None
                    status, result = fixture.route(self.command, self.path, self.headers.get("Authorization"), body)
                    self.reply(status, result)
                except Exception as error:
                    with fixture.lock:
                        fixture.unhandled.append(f"fixture exception: {type(error).__name__}: {error}")
                    self.reply(500, {"errcode": "M_UNKNOWN", "error": "fixture failure"})

            def reply(self, status: int, value: Any) -> None:
                payload = encoded(value)
                self.send_response(status)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, name="matrix-fixture", daemon=True)

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def __enter__(self) -> "MatrixFixture":
        self.thread.start()
        return self

    def __exit__(self, *_args: Any) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def snapshot(self) -> dict[str, int]:
        with self.lock:
            result = dict(sorted(self.counts.items()))
            result["accepted_events"] = len(self.transactions)
            result["unhandled_requests"] = len(self.unhandled)
            return result

    def event(self, room: str) -> dict[str, Any]:
        encrypted = room == ENCRYPTED
        return {
            "event_id": "$fixture-history", "sender": "@alice:fixture.invalid",
            "origin_server_ts": 1700000000000,
            "type": "m.room.encrypted" if encrypted else "m.room.message",
            "content": {"algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "fixture-only"} if encrypted else {"msgtype": "m.text", "body": UNICODE_TEXT},
        }

    def route(self, method: str, url: str, authorization: str | None, body: Any) -> tuple[int, Any]:
        path = unquote(urlsplit(url).path)
        with self.lock:
            self.counts["http_requests"] += 1
            if authorization != f"Bearer {TOKEN}":
                self.counts["authentication_failures"] += 1
                return 401, {"errcode": "M_UNKNOWN_TOKEN"}
            if method == "GET" and path == "/_matrix/client/v3/account/whoami":
                self.counts["whoami"] += 1
                return 200, {"user_id": "@alice:fixture.invalid", "device_id": "ZCHAT_FIXTURE"}
            if method == "GET" and path == "/_matrix/client/v3/joined_rooms":
                self.counts["joined_rooms"] += 1
                return 200, {"joined_rooms": [PLAIN, ENCRYPTED]}
            if method == "GET" and path == "/_matrix/client/v3/sync":
                self.counts["sync"] += 1
                rooms = {}
                for room, title in [(PLAIN, "本机测试聊天室"), (ENCRYPTED, "加密房间")]:
                    rooms[room] = {"state": {"events": [{"type": "m.room.name", "state_key": "", "content": {"name": title}}]}, "timeline": {"events": [self.event(room)], "limited": False}, "unread_notifications": {"notification_count": 1}}
                return 200, {"next_batch": "fixture-next", "rooms": {"join": rooms}}
            parts = path.split("/")
            if len(parts) >= 7 and parts[1:5] == ["_matrix", "client", "v3", "rooms"]:
                room, action = parts[5:7]
                if method == "GET" and action == "messages" and room in (PLAIN, ENCRYPTED):
                    self.counts["messages"] += 1
                    return 200, {"chunk": [self.event(room)], "start": "fixture-start", "end": "fixture-end"}
                if method == "GET" and parts[6:8] == ["state", "m.room.encryption"]:
                    self.counts["encryption_state"] += 1
                    if room == PLAIN:
                        return 404, {"errcode": "M_NOT_FOUND", "error": "No encryption state event"}
                    if room == ENCRYPTED:
                        return 200, {"algorithm": "m.megolm.v1.aes-sha2"}
                    if room == FORBIDDEN:
                        return 403, {"errcode": "M_FORBIDDEN", "error": "Cannot read room state"}
                if method == "PUT" and len(parts) == 9 and parts[6:8] == ["send", "m.room.message"]:
                    self.counts["send_requests"] += 1
                    if room != PLAIN:
                        self.counts["non_plaintext_send_requests"] += 1
                        return 403, {"errcode": "M_FORBIDDEN"}
                    key = (room, parts[8])
                    previous = self.transactions.get(key)
                    if previous:
                        self.counts["duplicate_transactions"] += 1
                        if previous[0] != body:
                            return 400, {"errcode": "M_BAD_JSON", "error": "Transaction payload changed"}
                        return 200, {"event_id": previous[1]}
                    if not isinstance(body, dict) or body.get("msgtype") != "m.text" or not isinstance(body.get("body"), str):
                        return 400, {"errcode": "M_BAD_JSON"}
                    event_id = f"$fixture-send-{len(self.transactions) + 1}"
                    self.transactions[key] = (body, event_id)
                    return 200, {"event_id": event_id}
            self.unhandled.append(f"{method} {path}")
            return 404, {"errcode": "M_UNRECOGNIZED", "error": "Not a fixture route"}


class McpClient:
    def __init__(self, suite: "Suite", *flags: str) -> None:
        self.suite = suite
        self.next_id = 1
        self.replies: queue.Queue[bytes | Exception] = queue.Queue()
        self.error_log = tempfile.TemporaryFile()
        self.process = subprocess.Popen(suite.command("mcp", *flags), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.error_log, env=suite.environment)
        self.reader = threading.Thread(target=self.read_lines, name="mcp-replies", daemon=True)
        self.reader.start()

    def read_lines(self) -> None:
        assert self.process.stdout
        try:
            while line := self.process.stdout.readline(MAX_FRAME + 1):
                if len(line) > MAX_FRAME:
                    raise AcceptanceError("MCP response exceeds 2 MiB")
                self.replies.put(line)
            self.replies.put(AcceptanceError("MCP process closed stdout before a response"))
        except Exception as error:
            self.replies.put(error)

    def __enter__(self) -> "McpClient":
        return self

    def __exit__(self, exception_type: Any, *_args: Any) -> None:
        assert self.process.stdin
        self.process.stdin.close()
        try:
            code = self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            code = -1
        self.reader.join(timeout=5)
        assert self.process.stdout
        self.process.stdout.close()
        self.error_log.seek(0)
        errors = self.error_log.read(MAX_FRAME).decode("utf-8", "replace")
        self.error_log.close()
        if exception_type is None:
            require(code == 0, f"MCP process did not exit cleanly: {code}; {errors[:2000]}")

    def send(self, value: Any) -> None:
        assert self.process.stdin
        self.process.stdin.write(encoded(value) + b"\n")
        self.process.stdin.flush()

    def request(self, method: str, params: Any = None) -> dict[str, Any]:
        request_id = self.next_id
        self.next_id += 1
        request = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            request["params"] = params
        self.send(request)
        try:
            line = self.replies.get(timeout=self.suite.timeout)
        except queue.Empty as error:
            raise AcceptanceError(f"MCP timed out waiting for {method}") from error
        if isinstance(line, Exception):
            raise line
        result = json.loads(line)
        require(isinstance(result, dict) and result.get("jsonrpc") == "2.0" and result.get("id") == request_id, "MCP response must retain JSON-RPC request identity")
        return result

    def initialize(self) -> None:
        response = self.request("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "zchat-loopback-acceptance", "version": "1"}})
        result = response.get("result", {})
        require(result.get("serverInfo", {}).get("name") == "zchat", f"MCP initialize failed: {response}")
        require(isinstance(result.get("protocolVersion"), str), "MCP initialize omitted protocol version")
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def tool(self, name: str, arguments: Any) -> dict[str, Any]:
        return self.request("tools/call", {"name": name, "arguments": arguments})

    def value(self, name: str, arguments: Any) -> dict[str, Any]:
        response = self.tool(name, arguments)
        require("error" not in response, f"MCP protocol error for {name}: {response}")
        result = response.get("result", {})
        require(not result.get("isError", False), f"MCP tool failed for {name}: {result}")
        require(isinstance(result.get("structuredContent"), dict), f"MCP tool {name} omitted structuredContent")
        return result["structuredContent"]

    def denied(self, name: str, arguments: Any, code: str | int) -> None:
        response = self.tool(name, arguments)
        if isinstance(code, int):
            require(response.get("error", {}).get("code") == code, f"Expected MCP protocol error {code}, received {response}")
        else:
            result = response.get("result", {})
            actual = result.get("structuredContent", {}).get("error", {}).get("code")
            require(result.get("isError") is True and actual == code, f"Expected MCP tool error {code}, received {response}")


class Suite:
    def __init__(self, binary: Path, config: Path, timeout: float, receipt: dict[str, Any]) -> None:
        self.binary, self.config, self.timeout, self.receipt = binary, config, timeout, receipt
        self.environment = os.environ.copy()
        for key in ("http_proxy", "https_proxy", "all_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            self.environment.pop(key, None)
        self.environment["NO_PROXY"] = self.environment["no_proxy"] = "127.0.0.1,localhost,::1"

    @contextmanager
    def case(self, name: str):
        self.receipt["active_case"] = name
        yield
        self.receipt["passed_cases"].append(name)
        self.receipt.pop("active_case", None)

    def command(self, *args: str) -> list[str]:
        return [str(self.binary), "--config", str(self.config), *args]

    def cli(self, *args: str, error: str | None = None) -> Any:
        result = subprocess.run(self.command(*args), capture_output=True, timeout=self.timeout, env=self.environment)
        require(len(result.stdout) <= MAX_FRAME and len(result.stderr) <= MAX_FRAME, "CLI output exceeded fixture limit")
        if error is not None:
            require(result.returncode != 0, f"CLI command unexpectedly succeeded: {args[0]}")
            value = json.loads(result.stderr.decode("utf-8"))
            require(value.get("error", {}).get("code") == error, f"Expected CLI error {error}: {value}")
            return value
        require(result.returncode == 0, f"CLI {args[0]} failed ({result.returncode}): {result.stderr.decode('utf-8', 'replace')[:2000]}")
        return json.loads(result.stdout.decode("utf-8"))

    def invoke(self, account: str, operation: str, params: Any = None, error: str | None = None) -> Any:
        return self.cli("invoke", account, operation, "--params", json.dumps(params or {}, ensure_ascii=False), error=error)


def exercise(binary: Path, plugin: Path, timeout: float, receipt: dict[str, Any]) -> None:
    with MatrixFixture() as fixture:
        try:
            with tempfile.TemporaryDirectory(prefix="zchat-acceptance-") as temporary:
                directory = Path(temporary)
                if os.name == "posix":
                    directory.chmod(0o700)
                config, settings = directory / "config.json", directory / "matrix-settings.json"
                private_json(settings, {"homeserver": fixture.url, "access_token": TOKEN})
                suite = Suite(binary, config, timeout, receipt)
                with suite.case("cli_binary_version"):
                    version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, timeout=timeout, env=suite.environment).stdout.decode("utf-8").strip()
                    require(version.startswith("zchat "), "Unexpected executable version output")
                    receipt["binary_version"] = version
                with suite.case("cli_account_setup_private_config"):
                    account = suite.cli("accounts", "add", "fixture", "matrix", "--settings", str(settings), "--ai-read", "--ai-write")
                    require(account.get("id") == "fixture" and account.get("ai_read") is True and account.get("ai_write") is True, "Account flags were not stored")
                    if os.name == "posix":
                        mode = stat.S_IMODE(config.stat().st_mode)
                        require(mode == 0o600, f"Account configuration mode must be 0600, received {mode:o}")
                        receipt["temporary_config_mode"] = f"{mode:04o}"
                    else:
                        receipt["temporary_config_mode"] = "platform-native permissions; POSIX mode not applicable"
                    suite.cli("accounts", "add", "read_only", "matrix", "--settings", str(settings), "--ai-read")
                    suite.cli("accounts", "add", "private", "matrix", "--settings", str(settings))
                with suite.case("cli_account_list_redacts_credentials"):
                    accounts = suite.cli("accounts", "list")
                    require(len(accounts.get("accounts", [])) == 3, "Configured accounts missing")
                    require(TOKEN not in json.dumps(accounts), "Public account listing exposed fixture credentials")
                with suite.case("cli_account_status_actual_http"):
                    require(suite.invoke("fixture", "account.status").get("user_id") == "@alice:fixture.invalid", "whoami result was not returned")
                with suite.case("cli_conversation_list_sync"):
                    rooms = suite.invoke("fixture", "conversation.list").get("conversations", [])
                    require(any(room.get("id") == PLAIN and room.get("title") == "本机测试聊天室" for room in rooms), "Sync room name normalization failed")
                with suite.case("cli_message_list_unicode"):
                    messages = suite.invoke("fixture", "message.list", {"conversation_id": PLAIN}).get("messages", [])
                    require(len(messages) == 1 and messages[0].get("text") == UNICODE_TEXT, "UTF-8 message roundtrip failed")
                    require("raw" not in messages[0], "Raw provider body should be excluded by default")
                with suite.case("cli_encrypted_message_is_explicit"):
                    messages = suite.invoke("fixture", "message.list", {"conversation_id": ENCRYPTED}).get("messages", [])
                    require(len(messages) == 1 and messages[0].get("encrypted") is True and messages[0].get("text") is None, "Encrypted event was presented as plaintext")
                with suite.case("cli_human_send_loopback_only"):
                    sent = suite.invoke("fixture", "message.send", {"conversation_id": PLAIN, "text": "human fixture " + UNICODE_TEXT, "idempotency_key": "acceptance-human"})
                    require(sent.get("id") == "$fixture-send-1" and sent.get("status") == "accepted", "Send acceptance was not normalized")
                    require(fixture.snapshot().get("send_requests") == 1, "CLI send must issue exactly one fixture send")
                with suite.case("cli_encrypted_and_unknown_state_send_guard"):
                    suite.invoke("fixture", "message.send", {"conversation_id": ENCRYPTED, "text": "must not send"}, error="encryption_required")
                    suite.invoke("fixture", "message.send", {"conversation_id": FORBIDDEN, "text": "must not send"}, error="upstream_error")
                    require(fixture.snapshot().get("send_requests") == 1, "Failed encryption checks must not transmit a message")
                with suite.case("cli_native_send_respects_encryption_guard"):
                    native = {"method": "PUT", "path": f"/_matrix/client/v3/rooms/{quote(ENCRYPTED, safe='')}/send/m.room.message/native-guard", "body": {"msgtype": "m.text", "body": "must not send"}}
                    suite.invoke("fixture", "native.invoke", native, error="encryption_required")
                    require(fixture.snapshot().get("send_requests") == 1, "Native path bypassed encryption guard")
                with suite.case("dynamic_plugin_requires_explicit_trust"):
                    suite.cli("plugins", "inspect", str(plugin), error="plugin_not_trusted")
                with suite.case("dynamic_zplugin_inspect_register"):
                    manifest = suite.cli("plugins", "inspect", str(plugin), "--trust")
                    require(manifest.get("id") == "zchat.providers", "Dynamic plugin manifest does not identify providers")
                    require(any(item.get("id") == "matrix" for item in manifest.get("providers", [])), "Dynamic plugin omitted Matrix capability")
                    added = suite.cli("plugins", "add", "external", str(plugin), "--trust")
                    require(added.get("id") == "external", "Dynamic plugin was not registered")
                    receipt["dynamic_plugin_manifest"] = {"id": manifest.get("id"), "version": manifest.get("version"), "interface": manifest.get("interface")}
                with suite.case("dynamic_plugin_account_routes_real_http"):
                    suite.cli("accounts", "add", "dynamic", "matrix", "--settings", str(settings), "--plugin", "external", "--ai-read")
                    before = fixture.snapshot().get("whoami", 0)
                    require(suite.invoke("dynamic", "account.status").get("user_id") == "@alice:fixture.invalid", "Dynamic plugin invocation failed")
                    require(fixture.snapshot().get("whoami", 0) == before + 1, "Dynamic plugin did not reach loopback backend")
                    messages = suite.invoke("dynamic", "message.list", {"conversation_id": PLAIN}).get("messages", [])
                    require(messages and messages[0].get("text") == UNICODE_TEXT, "Dynamic plugin message normalization failed")
                with McpClient(suite) as mcp:
                    with suite.case("mcp_initialize_and_readonly_tool_discovery"):
                        mcp.initialize()
                        tools = mcp.request("tools/list").get("result", {}).get("tools", [])
                        names = {tool["name"] for tool in tools}
                        require("zchat_message_list" in names and "zchat_message_send" not in names and "zchat_native_invoke" not in names, "Default MCP tools exposed write/native access")
                        require(all(tool.get("annotations", {}).get("readOnlyHint") is True for tool in tools), "Default tools must declare read-only annotations")
                        status = mcp.value("zchat_status", {})
                        require(status.get("ai_write_enabled") is False, "Default MCP unexpectedly enabled writes")
                    with suite.case("mcp_read_unicode_and_dynamic_route"):
                        value = mcp.value("zchat_message_list", {"account": "fixture", "params": {"conversation_id": PLAIN}})
                        require(value.get("messages", [{}])[0].get("text") == UNICODE_TEXT, "MCP structuredContent lost Unicode")
                        value = mcp.value("zchat_account_status", {"account": "dynamic", "params": {}})
                        require(value.get("user_id") == "@alice:fixture.invalid", "MCP dynamic plugin route failed")
                    with suite.case("mcp_default_write_and_actor_spoof_denied"):
                        before = fixture.snapshot()["http_requests"]
                        mcp.denied("zchat_message_send", {"account": "fixture", "actor": "human", "params": {"conversation_id": PLAIN, "text": "must not send", "actor": "human"}}, -32602)
                        mcp.denied("zchat_message_list", {"account": "private", "actor": "human", "params": {"conversation_id": PLAIN, "actor": "human"}}, "ai_access_disabled")
                        require(fixture.snapshot()["http_requests"] == before, "Denied MCP request reached the network")
                with McpClient(suite, "--allow-write") as mcp:
                    mcp.initialize()
                    with suite.case("mcp_write_requires_account_opt_in"):
                        names = {tool["name"] for tool in mcp.request("tools/list").get("result", {}).get("tools", [])}
                        require("zchat_message_send" in names and "zchat_native_invoke" not in names, "Write/native discovery scopes are incorrect")
                        before = fixture.snapshot()["http_requests"]
                        mcp.denied("zchat_message_send", {"account": "read_only", "actor": "human", "params": {"conversation_id": PLAIN, "text": "must not send"}}, "write_access_disabled")
                        require(fixture.snapshot()["http_requests"] == before, "Account write denial reached the network")
                    params = {"conversation_id": PLAIN, "text": "AI fixture " + UNICODE_TEXT, "idempotency_key": "acceptance-ai"}
                    with suite.case("mcp_authorized_single_send"):
                        sent = mcp.value("zchat_message_send", {"account": "fixture", "params": params})
                        require(sent.get("id") == "$fixture-send-2", "Authorized MCP send did not return fixture event")
                        require(fixture.snapshot().get("send_requests") == 2, "Authorized MCP send issued unexpected requests")
                    with suite.case("mcp_idempotency_replay_does_not_send"):
                        before = fixture.snapshot()["http_requests"]
                        repeated = mcp.value("zchat_message_send", {"account": "fixture", "params": params})
                        require(repeated == sent, "Idempotent replay changed result")
                        require(fixture.snapshot()["http_requests"] == before, "Idempotent replay issued another HTTP request")
                    with suite.case("mcp_idempotency_changed_payload_conflict"):
                        before = fixture.snapshot()["http_requests"]
                        mcp.denied("zchat_message_send", {"account": "fixture", "params": {**params, "text": "changed"}}, "idempotency_conflict")
                        require(fixture.snapshot()["http_requests"] == before, "Conflicting replay reached the network")
                    with suite.case("mcp_encrypted_send_is_explicit_failure"):
                        mcp.denied("zchat_message_send", {"account": "fixture", "params": {"conversation_id": ENCRYPTED, "text": "must not send", "idempotency_key": "encrypted-denied"}}, "encryption_required")
                        require(fixture.snapshot().get("send_requests") == 2, "MCP encrypted send reached send endpoint")
                with suite.case("fixture_final_send_and_endpoint_counts"):
                    counts = fixture.snapshot()
                    require(counts.get("send_requests") == 2 and counts["accepted_events"] == 2, "Expected exactly one human and one AI send, both local")
                    require(counts.get("non_plaintext_send_requests", 0) == 0, "Application attempted plaintext send to protected room")
                    require(counts.get("authentication_failures", 0) == 0, "Fixture authentication was not preserved")
                    require(not fixture.unhandled, f"Unexpected fixture request: {fixture.unhandled}")
            require(not directory.exists(), "Temporary configuration directory was not removed")
            receipt["temporary_config_removed"] = True
        finally:
            receipt["fixture_counts"] = fixture.snapshot()
            if "directory" in locals():
                receipt["temporary_config_removed"] = not directory.exists()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--plugin", required=True, type=Path)
    parser.add_argument("--receipt", type=Path, help="Also write the JSON receipt to this file")
    parser.add_argument("--timeout", type=float, default=45, help="Per-command timeout in seconds")
    args = parser.parse_args()
    receipt: dict[str, Any] = {"schema": "zchat.acceptance.v1", "mode": "loopback_matrix_only", "started_at": utc_now(), "status": "failed", "passed_cases": [], "real_im_accounts": 0, "external_messages_sent": 0}
    try:
        require(0 < args.timeout <= 60, "Timeout must be greater than zero and at most 60 seconds")
        binary, plugin = args.binary.resolve(strict=True), args.plugin.resolve(strict=True)
        require(binary.is_file() and plugin.is_file(), "Binary and plugin must be regular files")
        receipt["artifacts"] = {"binary": {"name": binary.name, "sha256": sha256(binary)}, "plugin": {"name": plugin.name, "sha256": sha256(plugin)}}
        exercise(binary, plugin, args.timeout, receipt)
        receipt["status"] = "passed"
    except Exception as error:
        receipt["error"] = f"{type(error).__name__}: {error}".replace(TOKEN, "<fixture-token>")[:4000]
    receipt["finished_at"] = utc_now()
    receipt["passed_count"] = len(receipt["passed_cases"])
    payload = json.dumps(receipt, ensure_ascii=False, indent=2) + "\n"
    if args.receipt:
        try:
            args.receipt.parent.mkdir(parents=True, exist_ok=True)
            args.receipt.write_text(payload, encoding="utf-8")
        except OSError as error:
            receipt["status"] = "failed"
            receipt["receipt_error"] = str(error)
            payload = json.dumps(receipt, ensure_ascii=False, indent=2) + "\n"
    sys.stdout.write(payload)
    return 0 if receipt["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
