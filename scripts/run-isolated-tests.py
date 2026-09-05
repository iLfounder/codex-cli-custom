#!/usr/bin/env python3
"""Run a test command with an isolated managed-account owner, including its children."""

import os
import subprocess
import sys
import tempfile


def run(command: list[str], env: dict[str, str]) -> int:
    try:
        code = subprocess.run(command, env=env, check=False).returncode
        return 128 - code if code < 0 else code
    except KeyboardInterrupt:
        return 130


def main() -> int:
    command = sys.argv[1:]
    if not command:
        print("expected a test command", file=sys.stderr)
        return 2
    env = os.environ.copy()
    # Preserve explicit values, even invalid ones: the owner resolver rejects them
    # instead of silently switching to the real account owner's home.
    if "CODEX_TEST_OWNER_HOME" in env:
        return run(command, env)
    with tempfile.TemporaryDirectory(prefix="codex-test-owner-") as owner_home:
        env["CODEX_TEST_OWNER_HOME"] = owner_home
        return run(command, env)


if __name__ == "__main__":
    raise SystemExit(main())
