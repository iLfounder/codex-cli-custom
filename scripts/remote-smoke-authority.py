#!/usr/bin/env python3
"""Isolated authority fixture for the remote macOS release smoke.

The fixture is intentionally stdlib-only.  It writes only below a new, disposable
owner home, binds numeric loopback listeners, and never logs credential payloads.

With --hold-first-response, poll first_request_url until observed, disconnect the
remote client, then POST release_first_response_url. The hold expires after 120s;
the default 0.25s delay alone does not establish that a disconnect happened.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
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
SECOND_ACCOUNT_ID = "smoke-chatgpt-account-2"
MODEL_ID = "remote-smoke-model"
SOURCE_REF_CONTEXT = b"llm-bridge.subscription-source-ref/v1\0codex-cli\0"
RESPONSE_SEQUENCE = (
    "pwd",
    "first_turn_complete",
    "get_goal",
    "update_goal",
    "goal_complete",
)
# Repeated conflicts must fail the smoke instead of hiding a resync livelock.
MAX_GOAL_RESYNCS = 3
SCENARIO_SEQUENCES = {
    "goal": RESPONSE_SEQUENCE,
    "approval": RESPONSE_SEQUENCE,
    "account-switch": ("account_c1_complete", "account_c2_complete"),
}


def scenario_accounts(scenario: str) -> dict[str, str]:
    if scenario not in SCENARIO_SEQUENCES:
        raise ValueError("unknown fixture scenario")
    accounts = {ACCOUNT_LABEL: ACCOUNT_ID}
    if scenario == "account-switch":
        accounts["C2"] = SECOND_ACCOUNT_ID
    return accounts


def _b64url(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def synthetic_jwt(account_id: str = ACCOUNT_ID) -> str:
    header = _b64url(
        json.dumps({"alg": "none", "typ": "JWT"}, separators=(",", ":")).encode()
    )
    claims = {
        "email": "remote-smoke@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account_id,
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": "remote-smoke-user",
        },
    }
    payload = _b64url(json.dumps(claims, separators=(",", ":")).encode())
    return f"{header}.{payload}.{_b64url(b'synthetic-signature')}"


def subscription_source_ref(canonical_home: Path, account_id: str = ACCOUNT_ID) -> str:
    digest = hashlib.sha256()
    digest.update(SOURCE_REF_CONTEXT)
    digest.update(account_id.encode())
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
    owner_home: Path,
    responses_port: int,
    primary: Path | None,
    *,
    scenario: str = "goal",
    code_mode_host: Path | None = None,
) -> dict[str, Any]:
    accounts = scenario_accounts(scenario)
    if code_mode_host is not None and primary is None:
        raise ValueError("code-mode host binary requires a primary binary")
    if owner_home.exists() and any(owner_home.iterdir()):
        raise ValueError(f"owner home must be new or empty: {owner_home}")
    private_dir(owner_home)
    account_home = owner_home / ".codex" / "account1"
    private_dir(account_home)
    canonical_home = account_home.resolve(strict=True)

    config_dir = owner_home / ".config"
    private_dir(config_dir)
    chatgpt_url = f"http://127.0.0.1:{responses_port}"
    provider_url = f"{chatgpt_url}/v1"
    permissions = (
        'approval_policy = "on-request"\n'
        'default_permissions = ":read-only"\n'
        'approvals_reviewer = "user"\n'
        if scenario == "approval"
        else 'approval_policy = "never"\n'
        'sandbox_mode = "danger-full-access"\n'
        'default_permissions = ":danger-full-access"\n'
    )
    config = f'''model = "{MODEL_ID}"
model_provider = "remote-smoke"
chatgpt_base_url = "{chatgpt_url}"
{permissions}cli_auth_credentials_store = "file"

[model_providers.remote-smoke]
name = "Remote smoke fixture"
base_url = "{provider_url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
requires_openai_auth = true
'''
    if scenario == "account-switch":
        config = (
            'model_context_window = 100000\nmodel_reasoning_effort = "medium"\n'
            + config
        )
    catalog_rows = []
    source_refs = {}
    for label, account_id in accounts.items():
        home = owner_home / ".codex" / f"account{label[1:]}"
        private_dir(home)
        home = home.resolve(strict=True)
        catalog_rows.append(f"{label[1:]}\t{home}\n")
        source_refs[label] = subscription_source_ref(home, account_id)
        jwt = synthetic_jwt(account_id)
        auth = {
            "auth_mode": "chatgpt",
            "last_refresh": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "tokens": {
                "id_token": jwt,
                "access_token": jwt,
                "refresh_token": "synthetic-refresh-not-a-secret",
                "account_id": account_id,
            },
        }
        private_write(home / "auth.json", json.dumps(auth, separators=(",", ":")))
        private_write(home / "config.toml", config)
    private_write(config_dir / "codex-accounts.tsv", "".join(catalog_rows))

    installed_primary = account_home / "packages" / "standalone" / "current" / "codex"
    if primary is not None:
        private_dir(installed_primary.parent)
        shutil.copyfile(primary, installed_primary)
        installed_primary.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)
    installed_host = installed_primary.with_name("codex-code-mode-host")
    if code_mode_host is not None:
        shutil.copyfile(code_mode_host, installed_host)
        installed_host.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)

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
        "source_ref": source_refs[ACCOUNT_LABEL],
        "source_refs": source_refs,
        "scenario": scenario,
        "account_slots": list(accounts),
        "code_mode_host_binary": str(installed_host)
        if code_mode_host is not None
        else None,
    }


def tool_result_output(tool_outputs: list[dict[str, Any]], call_id: str) -> str:
    outputs = [
        item["output"] for item in tool_outputs if item.get("call_id") == call_id
    ]
    if len(outputs) != 1 or not isinstance(outputs[0], str):
        raise ValueError(f"expected one tool result for {call_id}")
    return outputs[0]


def goal_tool_result(
    tool_outputs: list[dict[str, Any]], call_id: str
) -> dict[str, Any]:
    result = json.loads(tool_result_output(tool_outputs, call_id))
    if (
        not isinstance(result, dict)
        or not isinstance(result.get("goalId"), str)
        or not result["goalId"]
        or type(result.get("revision")) is not int
        or result["revision"] < 1
        or not isinstance(result.get("goal"), dict)
    ):
        raise ValueError(f"expected a versioned goal result for {call_id}")
    return result


def goal_call_ids(attempt: int) -> tuple[str, str]:
    if attempt == 0:
        return "remote-smoke-goal-read", "remote-smoke-goal-complete"
    return (
        f"remote-smoke-goal-read-resync-{attempt}",
        f"remote-smoke-goal-complete-retry-{attempt}",
    )


def response_phase(
    index: int, tool_outputs: list[dict[str, Any]], scenario: str
) -> str:
    sequence = SCENARIO_SEQUENCES[scenario]
    if index < 1:
        raise ValueError("invalid fixture exchange")
    if scenario == "account-switch" or index <= 4:
        if index > len(sequence):
            raise ValueError("fixture response sequence is already complete")
        return sequence[index - 1]
    if index > 5 + 2 * MAX_GOAL_RESYNCS:
        raise ValueError("goal resync limit exceeded")
    if index % 2 == 0:
        return "update_goal_retry"
    attempt = (index - 5) // 2
    read_id, update_id = goal_call_ids(attempt)
    previous = goal_tool_result(tool_outputs, read_id)
    output = tool_result_output(tool_outputs, update_id)
    conflict = re.fullmatch(
        r"goal revision conflict; call get_goal to resync "
        r'\(current_goal_id=Some\(("[^"\\]+")\), current_revision=([0-9]+)\)',
        output,
    )
    if conflict is not None:
        if (
            attempt >= MAX_GOAL_RESYNCS
            or json.loads(conflict[1]) != previous["goalId"]
            or int(conflict[2]) <= previous["revision"]
        ):
            raise ValueError("unexpected goal revision conflict")
        return "get_goal_resync"
    completed = goal_tool_result(tool_outputs, update_id)
    if (
        completed["goalId"] != previous["goalId"]
        or completed["goal"].get("status") != "complete"
        or completed["revision"] <= previous["revision"]
    ):
        raise ValueError("expected successful update_goal before final response")
    return "goal_complete"


def response_events(
    index: int,
    tool_outputs: list[dict[str, Any]],
    *,
    scenario: str = "goal",
) -> list[dict[str, Any]]:
    phase = response_phase(index, tool_outputs, scenario)
    if phase == "first_turn_complete" and not any(
        item.get("call_id") == "remote-smoke-pwd" for item in tool_outputs
    ):
        raise ValueError("expected pwd tool result before completing the first turn")
    response_id = f"remote-smoke-response-{index}"
    events: list[dict[str, Any]] = [
        {"type": "response.created", "response": {"id": response_id}}
    ]
    if phase == "pwd":
        arguments = {"cmd": "pwd", "yield_time_ms": 10000}
        if scenario == "approval":
            arguments.update(
                {
                    "sandbox_permissions": "require_escalated",
                    "justification": "Isolated approval smoke",
                }
            )
        events.append(
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": "remote-smoke-pwd",
                    "name": "exec_command",
                    "arguments": json.dumps(arguments),
                },
            }
        )
    elif phase in {"get_goal", "update_goal", "get_goal_resync", "update_goal_retry"}:
        arguments: dict[str, Any] = {}
        updating = phase in {"update_goal", "update_goal_retry"}
        attempt = (index - 4) // 2 if updating else (index - 3) // 2
        read_id, update_id = goal_call_ids(attempt)
        if updating:
            goal = goal_tool_result(tool_outputs, read_id)
            original = goal_tool_result(tool_outputs, "remote-smoke-goal-read")
            if (
                goal["goal"].get("status") != "active"
                or goal["goalId"] != original["goalId"]
            ):
                raise ValueError("expected an active goal before update_goal")
            if attempt:
                previous = goal_tool_result(tool_outputs, goal_call_ids(attempt - 1)[0])
                if goal["revision"] <= previous["revision"]:
                    raise ValueError("resync must return a newer goal revision")
            arguments = {
                "expected_goal_id": goal["goalId"],
                "expected_revision": goal["revision"],
                "status": "complete",
            }
        events.append(
            {
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "call_id": update_id if updating else read_id,
                    "name": "update_goal" if updating else "get_goal",
                    "arguments": json.dumps(arguments),
                },
            }
        )
    else:
        text = (
            "remote smoke first turn complete"
            if phase == "first_turn_complete"
            else "remote smoke goal complete"
        )
        if scenario == "account-switch":
            text = f"remote smoke account C{index} complete"
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
    def __init__(
        self,
        source_ref: str,
        records_file: Path,
        *,
        hold_first_response: bool = False,
        scenario: str = "goal",
        source_refs: dict[str, str] | None = None,
    ) -> None:
        self.scenario = scenario
        self.accounts = scenario_accounts(scenario)
        if source_refs is None:
            source_refs = {ACCOUNT_LABEL: source_ref}
        if set(source_refs) != set(self.accounts):
            raise ValueError(
                "fixture account source references do not match the scenario"
            )
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
        self.snapshots = [
            {**self.snapshot, "label": label, "sourceRef": source_refs[label]}
            for label in self.accounts
        ]
        self.records_file = records_file
        self.records: list[dict[str, Any]] = []
        self.response_count = 0
        self.completed_response_count = 0
        self.lock = threading.Lock()
        self.first_request_observed = threading.Event()
        self.first_response_release = threading.Event()
        if not hold_first_response:
            self.first_response_release.set()

    def record_response_request(self, body: dict[str, Any], headers: Any) -> int:
        with self.lock:
            self.response_count += 1
            index = self.response_count
            account_slot = None
            if self.scenario == "account-switch":
                for label, account_id in self.accounts.items():
                    if (
                        headers.get("ChatGPT-Account-Id") == account_id
                        and headers.get("Authorization")
                        == f"Bearer {synthetic_jwt(account_id)}"
                    ):
                        account_slot = label
                        break
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
            tool_outputs = find_tool_outputs(body.get("input", []))
            try:
                phase = response_phase(index, tool_outputs, self.scenario)
            except (ValueError, TypeError, KeyError):
                phase = "unexpected"
            self.records.append(
                {
                    "exchange": index,
                    "phase": phase,
                    "response_id": f"remote-smoke-response-{index}",
                    "turn_id": turn_id,
                    "account_slot": account_slot,
                    # Record comparisons only, never header or credential values.
                    "auth_diagnostics": {
                        "authorization_present": bool(headers.get("Authorization")),
                        "account_header_present": bool(
                            headers.get("ChatGPT-Account-Id")
                        ),
                        "matching_auth_slots": [
                            label
                            for label, account_id in self.accounts.items()
                            if headers.get("Authorization")
                            == f"Bearer {synthetic_jwt(account_id)}"
                        ],
                        "matching_account_slots": [
                            label
                            for label, account_id in self.accounts.items()
                            if headers.get("ChatGPT-Account-Id") == account_id
                        ],
                    }
                    if self.scenario == "account-switch"
                    else None,
                    "tool_outputs": (
                        [] if self.scenario == "account-switch" else tool_outputs
                    ),
                }
            )
            private_write(self.records_file, json.dumps(self.records, indent=2) + "\n")
            if index == 1:
                self.first_request_observed.set()
            if self.scenario == "account-switch" and (
                index > len(self.accounts) or account_slot != f"C{index}"
            ):
                raise ValueError("request account did not match the fixture sequence")
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
            path = self.path.partition("?")[0].rstrip("/")
            if role == "responses" and path in {
                "/connectors/directory/list",
                "/connectors/directory/list_workspace",
            }:
                self.send_json({"apps": [], "nextToken": None})
                return
            if role == "responses" and path == "/api/codex/ps/mcp":
                # This empty Apps mock is stateless and has no server-initiated SSE stream.
                self.send_error(HTTPStatus.METHOD_NOT_ALLOWED)
                return
            if (
                role == "responses"
                and path == "/.well-known/oauth-authorization-server/mcp"
            ):
                base_url = f"http://127.0.0.1:{self.server.server_address[1]}"
                self.send_json(
                    {
                        "authorization_endpoint": f"{base_url}/oauth/authorize",
                        "token_endpoint": f"{base_url}/oauth/token",
                        "scopes_supported": [""],
                    }
                )
                return
            if role == "responses" and self.path == "/fixture/first-request":
                with state.lock:
                    status = {
                        "scenario": state.scenario,
                        "available_account_slots": list(state.accounts),
                        "observed": state.first_request_observed.is_set(),
                        "released": state.first_response_release.is_set(),
                        "response_count": state.response_count,
                        "completed_response_count": state.completed_response_count,
                        "phases": [record["phase"] for record in state.records],
                        "turn_ids": [record["turn_id"] for record in state.records],
                        "account_slots": [
                            record["account_slot"] for record in state.records
                        ],
                    }
                self.send_json(status)
                return
            if role != "token" or self.path not in {"/snapshots", "/events"}:
                self.send_error(HTTPStatus.NOT_FOUND)
                return
            if self.path == "/snapshots":
                self.send_json({"accounts": state.snapshots})
                return
            first = (
                "event: initial\ndata: "
                + json.dumps(state.snapshots, separators=(",", ":"))
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
            if role == "responses" and self.path == "/fixture/release-first-response":
                if not state.first_request_observed.is_set():
                    self.send_error(
                        HTTPStatus.CONFLICT, "first request has not been observed"
                    )
                    return
                state.first_response_release.set()
                self.send_json({"released": True})
                return
            path = self.path.partition("?")[0].rstrip("/")
            is_apps_mcp = path == "/api/codex/ps/mcp"
            if role != "responses" or not (is_apps_mcp or path.endswith("/responses")):
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
            if is_apps_mcp:
                method = body.get("method")
                if method == "notifications/initialized":
                    self.send_response(HTTPStatus.ACCEPTED)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if method == "initialize":
                    params = body.get("params", {})
                    if not isinstance(params, dict):
                        self.send_error(HTTPStatus.BAD_REQUEST)
                        return
                    result = {
                        "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "remote-smoke-apps", "version": "1.0.0"},
                    }
                elif method == "tools/list":
                    result = {"tools": []}
                elif method == "resources/list":
                    result = {"resources": []}
                elif method == "resources/templates/list":
                    result = {"resourceTemplates": []}
                elif method == "ping":
                    result = {}
                else:
                    self.send_json(
                        {
                            "jsonrpc": "2.0",
                            "id": body.get("id"),
                            "error": {"code": -32601, "message": "method not found"},
                        }
                    )
                    return
                self.send_json(
                    {"jsonrpc": "2.0", "id": body.get("id"), "result": result}
                )
                return
            try:
                index = state.record_response_request(body, self.headers)
                events = response_events(
                    index,
                    find_tool_outputs(body.get("input", [])),
                    scenario=state.scenario,
                )
            except (ValueError, TypeError, KeyError):
                self.send_error(
                    HTTPStatus.CONFLICT, "request does not match fixture tool sequence"
                )
                return
            if index == 1:
                if not state.first_response_release.wait(timeout=120):
                    self.send_error(
                        HTTPStatus.GATEWAY_TIMEOUT, "first response was not released"
                    )
                    return
                time.sleep(0.25)
            payload = "".join(
                f"event: {event['type']}\ndata: {json.dumps(event, separators=(',', ':'))}\n\n"
                for event in events
            ).encode()
            self.send_response(HTTPStatus.OK)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

            # Count only a validated fixture sequence whose SSE was fully sent.
            self.wfile.flush()
            with state.lock:
                state.completed_response_count += 1

    return Handler


class ControlServer(threading.Thread):
    def __init__(
        self, socket_path: Path, account_labels: tuple[str, ...] = (ACCOUNT_LABEL,)
    ) -> None:
        super().__init__(name="remote-smoke-control", daemon=True)
        self.socket_path = socket_path
        self.account_labels = account_labels
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
                account_label = None
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
                    if account_label is None:
                        account_label = request.get("accountId")
                    valid = (
                        request.get("method") == expected_method
                        and account_label in self.account_labels
                        and request.get("accountId") == account_label
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
    parser.add_argument("--code-mode-host-binary", type=Path)
    parser.add_argument("--scenario", choices=tuple(SCENARIO_SEQUENCES), default="goal")
    parser.add_argument("--token-manager-port", type=int, default=0)
    parser.add_argument("--responses-port", type=int, default=0)
    parser.add_argument("--ready-file", type=Path)
    parser.add_argument(
        "--hold-first-response",
        action="store_true",
        help="wait up to 120 seconds for POST /fixture/release-first-response before the first response",
    )
    return parser.parse_args()


def main() -> int:
    if not hasattr(socket, "AF_UNIX"):
        raise SystemExit("remote smoke authority requires Unix-domain sockets")
    args = parse_args()
    if args.primary_binary is not None and not args.primary_binary.is_file():
        raise SystemExit(f"primary binary does not exist: {args.primary_binary}")
    if args.code_mode_host_binary is not None:
        if args.primary_binary is None:
            raise SystemExit("--code-mode-host-binary requires --primary-binary")
        if not args.code_mode_host_binary.is_file():
            raise SystemExit("code-mode host binary does not exist")

    owner_home = (
        args.owner_home or Path(tempfile.mkdtemp(prefix="codex-remote-smoke-"))
    ).resolve()
    # Reserve both actual ports before embedding the Responses URL in config.toml.
    probe = loopback_server(args.responses_port, BaseHTTPRequestHandler)
    responses_port = probe.server_address[1]
    probe.server_close()
    prepared = prepare_owner_home(
        owner_home,
        responses_port,
        args.primary_binary,
        scenario=args.scenario,
        code_mode_host=args.code_mode_host_binary,
    )
    state = FixtureState(
        prepared["source_ref"],
        Path(prepared["records_file"]),
        hold_first_response=args.hold_first_response,
        scenario=args.scenario,
        source_refs=prepared["source_refs"],
    )
    responses = loopback_server(responses_port, handler_for(state, "responses"))
    token_manager = loopback_server(
        args.token_manager_port, handler_for(state, "token")
    )
    control = ControlServer(Path(prepared["control_socket"]), tuple(state.accounts))

    ready = {
        **prepared,
        "account_label": ACCOUNT_LABEL,
        "account_id": ACCOUNT_ID,
        "model": MODEL_ID,
        "token_manager_url": f"http://127.0.0.1:{token_manager.server_address[1]}/",
        "responses_url": f"http://127.0.0.1:{responses.server_address[1]}/v1",
        "first_request_url": f"http://127.0.0.1:{responses.server_address[1]}/fixture/first-request",
        "release_first_response_url": f"http://127.0.0.1:{responses.server_address[1]}/fixture/release-first-response",
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
