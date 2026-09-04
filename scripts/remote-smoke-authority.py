#!/usr/bin/env python3
"""Isolated authority fixture for the remote macOS release smoke.

The fixture is intentionally stdlib-only.  It writes only below a new, disposable
owner home, binds numeric loopback listeners, and never logs credential payloads.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import shutil
import signal
import socket
import stat
import tempfile
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


ACCOUNT_ID = "smoke-chatgpt-account"
ACCOUNT_LABEL = "C1"
MODEL_ID = "remote-smoke-model"
SOURCE_REF_CONTEXT = b"llm-bridge.subscription-source-ref/v1\0codex-cli\0"


def _b64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def synthetic_jwt() -> str:
    header = _b64url(
        json.dumps({"alg": "none", "typ": "JWT"}, separators=(",", ":")).encode()
    )
    claims = {
        "email": "remote-smoke@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": ACCOUNT_ID,
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": "remote-smoke-user",
        },
    }
    payload = _b64url(json.dumps(claims, separators=(",", ":")).encode())
    return f"{header}.{payload}.{_b64url(b'synthetic-signature')}"


def subscription_source_ref(canonical_home: Path) -> str:
    digest = hashlib.sha256()
    digest.update(SOURCE_REF_CONTEXT)
    digest.update(ACCOUNT_ID.encode())
    digest.update(b"\0")
    digest.update(str(canonical_home).encode("utf-8"))
    return "subscription-source-v1:" + _b64url(digest.digest())


def private_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)


def private_write(path: Path, data: str) -> None:
    path.write_text(data, encoding="utf-8")
    path.chmod(0o600)


def prepare_owner_home(
    owner_home: Path, responses_port: int, primary: Path | None
) -> dict[str, str]:
    if owner_home.exists() and any(owner_home.iterdir()):
        raise ValueError(f"owner home must be new or empty: {owner_home}")
    private_dir(owner_home)
    account_home = owner_home / ".codex" / "account1"
    private_dir(account_home)
    canonical_home = account_home.resolve(strict=True)

    config_dir = owner_home / ".config"
    private_dir(config_dir)
    private_write(config_dir / "codex-accounts.tsv", f"1\t{canonical_home}\n")

    jwt = synthetic_jwt()
    auth = {
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": jwt,
            "access_token": jwt,
            "refresh_token": "synthetic-refresh-not-a-secret",
            "account_id": ACCOUNT_ID,
        },
    }
    private_write(account_home / "auth.json", json.dumps(auth, separators=(",", ":")))

    provider_url = f"http://127.0.0.1:{responses_port}/v1"
    config = f'''model = "{MODEL_ID}"
model_provider = "remote-smoke"
approval_policy = "never"
sandbox_mode = "danger-full-access"
default_permissions = ":danger-full-access"
cli_auth_credentials_store = "file"

[model_providers.remote-smoke]
name = "Remote smoke fixture"
base_url = "{provider_url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
requires_openai_auth = true
'''
    private_write(account_home / "config.toml", config)

    installed_primary = account_home / "packages" / "standalone" / "current" / "codex"
    if primary is not None:
        private_dir(installed_primary.parent)
        shutil.copyfile(primary, installed_primary)
        installed_primary.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)

    control_dir = owner_home / ".tokenmanager" / "control"
    private_dir(control_dir)
    records_file = owner_home / "remote-smoke-records.json"
    private_write(records_file, "[]\n")
    return {
        "owner_home": str(owner_home),
        "codex_home": str(canonical_home),
        "primary_binary": str(installed_primary),
        "control_socket": str(control_dir / "tokenmanager.sock"),
        "records_file": str(records_file),
        "source_ref": subscription_source_ref(canonical_home),
    }


def response_events(index: int) -> list[dict[str, Any]]:
    response_id = f"remote-smoke-response-{index}"
    events: list[dict[str, Any]] = [
        {"type": "response.created", "response": {"id": response_id}}
    ]
    if index == 1:
        events.append(
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "remote-smoke-pwd",
                    "name": "exec_command",
                    "arguments": json.dumps({"cmd": "pwd", "yield_time_ms": 10000}),
                },
            }
        )
    elif index == 3:
        events.append(
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "remote-smoke-goal-complete",
                    "name": "update_goal",
                    "arguments": json.dumps({"status": "complete"}),
                },
            }
        )
    else:
        text = (
            "remote smoke first turn complete"
            if index == 2
            else "remote smoke goal complete"
        )
        events.append(
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "id": f"remote-smoke-message-{index}",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": text, "annotations": []}
                    ],
                },
            }
        )
    events.append(
        {
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": None,
                    "output_tokens": 0,
                    "output_tokens_details": None,
                    "total_tokens": 0,
                },
            },
        }
    )
    return events


def find_tool_outputs(value: Any) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    if isinstance(value, dict):
        if value.get("type") in {"function_call_output", "custom_tool_call_output"}:
            found.append(
                {
                    "type": value.get("type"),
                    "call_id": value.get("call_id"),
                    "output": value.get("output"),
                }
            )
        for child in value.values():
            found.extend(find_tool_outputs(child))
    elif isinstance(value, list):
        for child in value:
            found.extend(find_tool_outputs(child))
    return found


class FixtureState:
    def __init__(self, source_ref: str, records_file: Path) -> None:
        now = int(time.time())
        self.snapshot = {
            "label": ACCOUNT_LABEL,
            "type": "codex-chatgpt",
            "sourceRef": source_ref,
            "fetchedAt": now,
            "ok": True,
            "rateLimit": {
                "status": "allowed",
                "meters": [
                    {
                        "id": "weekly",
                        "label": "Weekly",
                        "utilization": 0.05,
                        "resetAt": now + 86400,
                        "observedAt": now,
                        "utilizationObservedAt": now,
                        "state": "normal",
                    }
                ],
            },
        }
        self.records_file = records_file
        self.records: list[dict[str, Any]] = []
        self.response_count = 0
        self.lock = threading.Lock()

    def record_response_request(self, body: dict[str, Any], headers: Any) -> int:
        with self.lock:
            self.response_count += 1
            index = self.response_count
            turn_id = headers.get("x-codex-turn-id")
            metadata = body.get("client_metadata") or body.get("metadata")
            if turn_id is None and isinstance(metadata, dict):
                turn_id = metadata.get("turn_id") or metadata.get("turnId")
                nested = metadata.get("x-codex-turn-metadata")
                if turn_id is None and isinstance(nested, str):
                    try:
                        nested = json.loads(nested)
                    except json.JSONDecodeError:
                        nested = None
                if turn_id is None and isinstance(nested, dict):
                    turn_id = nested.get("turn_id") or nested.get("turnId")
            self.records.append(
                {
                    "exchange": index,
                    "response_id": f"remote-smoke-response-{index}",
                    "turn_id": turn_id,
                    "tool_outputs": find_tool_outputs(body.get("input", [])),
                }
            )
            private_write(self.records_file, json.dumps(self.records, indent=2) + "\n")
            return index


def handler_for(state: FixtureState, role: str) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: Any) -> None:
            return

        def send_json(self, value: Any) -> None:
            payload = json.dumps(value, separators=(",", ":")).encode()
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self) -> None:  # noqa: N802
            if role != "token" or self.path not in {"/snapshots", "/events"}:
                self.send_error(HTTPStatus.NOT_FOUND)
                return
            if self.path == "/snapshots":
                self.send_json({"accounts": [state.snapshot]})
                return
            first = (
                "event: initial\ndata: "
                + json.dumps([state.snapshot], separators=(",", ":"))
                + "\n\n"
            )
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            try:
                self.wfile.write(first.encode())
                self.wfile.flush()
                while True:
                    time.sleep(15)
                    self.wfile.write(b": keepalive\n\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                return

        def do_POST(self) -> None:  # noqa: N802
            if role != "responses" or not self.path.rstrip("/").endswith("/responses"):
                self.send_error(HTTPStatus.NOT_FOUND)
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if length <= 0 or length > 1024 * 1024:
                    raise ValueError("invalid body length")
                body = json.loads(self.rfile.read(length))
                if not isinstance(body, dict):
                    raise ValueError("request body must be an object")
            except (ValueError, json.JSONDecodeError):
                self.send_error(HTTPStatus.BAD_REQUEST)
                return
            index = state.record_response_request(body, self.headers)
            if index > 4:
                self.send_error(
                    HTTPStatus.CONFLICT, "fixture has exactly four exchanges"
                )
                return
            if index == 1:
                time.sleep(0.25)
            payload = "".join(
                f"event: {event['type']}\ndata: {json.dumps(event, separators=(',', ':'))}\n\n"
                for event in response_events(index)
            ).encode()
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    return Handler


class ControlServer(threading.Thread):
    def __init__(self, socket_path: Path) -> None:
        super().__init__(name="remote-smoke-control", daemon=True)
        self.socket_path = socket_path
        self.stop_event = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(socket_path))
        socket_path.chmod(0o600)
        self.listener.listen()
        self.listener.settimeout(0.5)

    def run(self) -> None:
        while not self.stop_event.is_set():
            try:
                connection, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                break
            with connection:
                stream = connection.makefile("rwb")
                generation = 1
                expected = [
                    ("lifecycle/begin", "active"),
                    ("lifecycle/forceRefresh", "refreshed"),
                    ("lifecycle/commit", "committed"),
                ]
                for expected_method, state_name in expected:
                    line = stream.readline(4097)
                    try:
                        request = json.loads(line)
                    except json.JSONDecodeError:
                        break
                    valid = (
                        request.get("method") == expected_method
                        and request.get("accountId") == ACCOUNT_LABEL
                    )
                    response = {
                        "ok": valid,
                        "state": state_name if valid else "rejected",
                        "generation": generation,
                    }
                    stream.write(
                        json.dumps(response, separators=(",", ":")).encode() + b"\n"
                    )
                    stream.flush()
                    if not valid:
                        break

    def close(self) -> None:
        self.stop_event.set()
        self.listener.close()


def loopback_server(
    port: int, handler: type[BaseHTTPRequestHandler]
) -> ThreadingHTTPServer:
    class Server(ThreadingHTTPServer):
        daemon_threads = True
        allow_reuse_address = False

    return Server(("127.0.0.1", port), handler)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--owner-home", type=Path)
    parser.add_argument("--primary-binary", type=Path)
    parser.add_argument("--token-manager-port", type=int, default=0)
    parser.add_argument("--responses-port", type=int, default=0)
    parser.add_argument("--ready-file", type=Path)
    return parser.parse_args()


def main() -> int:
    if not hasattr(socket, "AF_UNIX"):
        raise SystemExit("remote smoke authority requires Unix-domain sockets")
    args = parse_args()
    if args.primary_binary is not None and not args.primary_binary.is_file():
        raise SystemExit(f"primary binary does not exist: {args.primary_binary}")

    owner_home = (
        args.owner_home or Path(tempfile.mkdtemp(prefix="codex-remote-smoke-"))
    ).resolve()
    # Reserve both actual ports before embedding the Responses URL in config.toml.
    probe = loopback_server(args.responses_port, BaseHTTPRequestHandler)
    responses_port = probe.server_address[1]
    probe.server_close()
    prepared = prepare_owner_home(owner_home, responses_port, args.primary_binary)
    state = FixtureState(prepared["source_ref"], Path(prepared["records_file"]))
    responses = loopback_server(responses_port, handler_for(state, "responses"))
    token_manager = loopback_server(
        args.token_manager_port, handler_for(state, "token")
    )
    control = ControlServer(Path(prepared["control_socket"]))

    ready = {
        **prepared,
        "account_label": ACCOUNT_LABEL,
        "account_id": ACCOUNT_ID,
        "model": MODEL_ID,
        "token_manager_url": f"http://127.0.0.1:{token_manager.server_address[1]}/",
        "responses_url": f"http://127.0.0.1:{responses.server_address[1]}/v1",
    }
    if args.ready_file is not None:
        private_write(args.ready_file, json.dumps(ready, indent=2) + "\n")
    print(json.dumps(ready, separators=(",", ":")), flush=True)

    stopped = threading.Event()

    def stop(_signum: int, _frame: Any) -> None:
        stopped.set()

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    threads = [
        threading.Thread(
            target=responses.serve_forever, name="remote-smoke-responses", daemon=True
        ),
        threading.Thread(
            target=token_manager.serve_forever, name="remote-smoke-token", daemon=True
        ),
    ]
    for thread in threads:
        thread.start()
    control.start()
    stopped.wait()
    responses.shutdown()
    token_manager.shutdown()
    responses.server_close()
    token_manager.server_close()
    control.close()
    try:
        Path(prepared["control_socket"]).unlink()
    except FileNotFoundError:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
