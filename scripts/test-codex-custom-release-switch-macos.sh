#!/bin/bash

set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd -P)"
switcher="$script_dir/codex-custom-release-switch-macos.sh"
root="$(/usr/bin/mktemp -d /private/tmp/codex-release-switch-test.XXXXXX)"
trap '/usr/bin/trash "$root" >/dev/null 2>&1 || true' EXIT

make_release() {
  local name="$1"
  local version="$2"
  local release="$root/releases/$name"

  /bin/mkdir -p "$release/bin"
  for binary in codex codex-app-server codex-code-mode-host codex-responses-api-proxy; do
    # The generated script must retain its own $1.
    # shellcheck disable=SC2016
    /usr/bin/printf '#!/bin/sh\ncase "$1" in\n  --version) printf "%%s\\n" "%s %s" ;;\nesac\n' \
      "$( [ "$binary" = codex ] && printf codex-cli || printf '%s' "$binary" )" "$version" \
      >"$release/bin/$binary"
    /bin/chmod 0700 "$release/bin/$binary"
    /bin/ln -s "bin/$binary" "$release/$binary"
  done
}

expect_failure() {
  local description="$1"
  shift

  if "$@" >/dev/null 2>&1; then
    echo "$description unexpectedly succeeded" >&2
    exit 1
  fi
}

/bin/mkdir -p "$root/releases"
/bin/chmod 0700 "$root" "$root/releases"
make_release old-1.0.0 1.0.0
make_release new-2.0.0 2.0.0
/bin/mv "$root/releases/old-1.0.0" "$root/releases/1.0.0-old"
/bin/mv "$root/releases/new-2.0.0" "$root/releases/2.0.0-new"
/bin/ln -s releases/1.0.0-old "$root/current"
/bin/ln -s releases/0.9.0-older "$root/previous-custom"

preflight="$("$switcher" preflight --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new)"
[[ "$preflight" == *'"status":"ready"'* ]]
[ "$(/usr/bin/readlink "$root/current")" = releases/1.0.0-old ]

switched="$("$switcher" switch --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new)"
[[ "$switched" == *'"status":"switched"'* ]]
[ "$(/usr/bin/readlink "$root/current")" = releases/2.0.0-new ]
[ "$(/usr/bin/readlink "$root/previous-custom")" = releases/1.0.0-old ]

again="$("$switcher" switch --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new)"
[[ "$again" == *'"status":"alreadyApplied"'* ]]

expect_failure "stale expected-current" "$switcher" switch --root "$root" \
  --expected-current releases/0.9.0-older --target releases/1.0.0-old
[ "$(/usr/bin/readlink "$root/current")" = releases/2.0.0-new ]

rolled_back="$("$switcher" switch --root "$root" \
  --expected-current releases/2.0.0-new --target releases/1.0.0-old)"
[[ "$rolled_back" == *'"status":"switched"'* ]]
[ "$(/usr/bin/readlink "$root/current")" = releases/1.0.0-old ]
[ "$(/usr/bin/readlink "$root/previous-custom")" = releases/2.0.0-new ]

# An inherited environment variable must not bypass the filesystem lock.
/bin/mkdir -m 0700 "$root/.release-switch.lock.d"
expect_failure "stale lock preflight" "$switcher" preflight --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new
expect_failure "inherited lock bypass" /usr/bin/env CODEX_CUSTOM_RELEASE_SWITCH_LOCKED=1 \
  "$switcher" switch --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new
[ "$(/usr/bin/readlink "$root/current")" = releases/1.0.0-old ]
[ "$(/usr/bin/readlink "$root/previous-custom")" = releases/2.0.0-new ]
/bin/rmdir "$root/.release-switch.lock.d"

# Every directory below an immutable release must remain owner-only writable.
/bin/chmod 0777 "$root/releases/2.0.0-new/bin"
expect_failure "writable release intermediate" "$switcher" preflight --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new
/bin/chmod 0700 "$root/releases/2.0.0-new/bin"

# Exact version text is insufficient if the executable reports failure.
/bin/cp "$root/releases/2.0.0-new/bin/codex" "$root/releases/2.0.0-new/bin/codex.saved"
/usr/bin/printf '%s\n' '#!/bin/sh' \
  'printf "%s\n" "codex-cli 2.0.0"' \
  'exit 9' >"$root/releases/2.0.0-new/bin/codex"
/bin/chmod 0700 "$root/releases/2.0.0-new/bin/codex"
expect_failure "nonzero version command" "$switcher" preflight --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new
/bin/mv "$root/releases/2.0.0-new/bin/codex.saved" "$root/releases/2.0.0-new/bin/codex"

# A stuck version probe must not retain the outer PM2/bootstrap lock indefinitely.
/bin/cp "$root/releases/2.0.0-new/bin/codex" "$root/releases/2.0.0-new/bin/codex.saved"
/usr/bin/printf '%s\n' '#!/bin/sh' 'sleep 30' >"$root/releases/2.0.0-new/bin/codex"
/bin/chmod 0700 "$root/releases/2.0.0-new/bin/codex"
SECONDS=0
expect_failure "stuck version command" /usr/bin/env \
  CODEX_CUSTOM_RELEASE_VERSION_TIMEOUT_SECONDS=1 \
  "$switcher" preflight --root "$root" \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new
[ "$SECONDS" -lt 5 ]
/bin/mv "$root/releases/2.0.0-new/bin/codex.saved" "$root/releases/2.0.0-new/bin/codex"

# Unsupported root characters must fail before any filesystem mutation.
expect_failure "unsafe root JSON text" "$switcher" preflight \
  --root '/private/tmp/codex-release-switch-"bad' \
  --expected-current releases/1.0.0-old --target releases/2.0.0-new

# A release that fails only after activation must restore both prior links.
make_release flaky-3.0.0 3.0.0
/bin/mv "$root/releases/flaky-3.0.0" "$root/releases/3.0.0-flaky"
counter="$root/releases/3.0.0-flaky/version-count"
/usr/bin/printf '0\n' >"$counter"
# The generated script must retain its own command substitutions and variables.
# shellcheck disable=SC2016
/usr/bin/printf '%s\n' '#!/bin/sh' \
  'counter="$(dirname "$0")/../version-count"' \
  'count="$(cat "$counter")"' \
  'count=$((count + 1))' \
  'printf "%s\n" "$count" >"$counter"' \
  'printf "%s\n" "codex-cli 3.0.0"' \
  '[ "$count" -lt 3 ]' >"$root/releases/3.0.0-flaky/bin/codex"
/bin/chmod 0700 "$root/releases/3.0.0-flaky/bin/codex"
expect_failure "post-switch validation" "$switcher" switch --root "$root" \
  --expected-current releases/1.0.0-old --target releases/3.0.0-flaky
[ "$(/usr/bin/readlink "$root/current")" = releases/1.0.0-old ]
[ "$(/usr/bin/readlink "$root/previous-custom")" = releases/2.0.0-new ]

echo "PASS: release switch CAS, bounded probes, locking, path safety, rollback, and post-switch restoration"
