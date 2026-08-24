#!/bin/bash

set -euo pipefail
umask 077

readonly DEFAULT_ROOT="${HOME}/.local/lib/codex-custom"
readonly VERSION_TIMEOUT_BIN="${CODEX_CUSTOM_RELEASE_TIMEOUT_BIN:-/opt/homebrew/bin/timeout}"
readonly VERSION_TIMEOUT_SECONDS="${CODEX_CUSTOM_RELEASE_VERSION_TIMEOUT_SECONDS:-5}"

usage() {
  cat <<'EOF'
Usage:
  codex-custom-release-switch-macos.sh preflight --expected-current releases/OLD --target releases/NEW [--root PATH]
  codex-custom-release-switch-macos.sh switch    --expected-current releases/OLD --target releases/NEW [--root PATH]

The switch command uses expected-current as a compare-and-swap precondition.
Each link replacement is atomic, ordered previous-custom first and current
second; the pair is not one filesystem transaction. Repeating an already-
completed switch is safe when previous-custom still names OLD.
EOF
}

die() {
  printf '%s\n' "$1" >&2
  exit "${2:-70}"
}

json_result() {
  printf '{"status":"%s","root":"%s","previous":"%s","current":"%s"}\n' \
    "$1" "$release_root" "$2" "$3"
}

run_version_command() {
  "$VERSION_TIMEOUT_BIN" -k 2 "$VERSION_TIMEOUT_SECONDS" "$1" --version
}

validate_release_ref() {
  local name="${1#releases/}"

  [ "$name" != "$1" ] || die "release reference must be releases/NAME" 64
  [[ "$name" =~ ^[0-9A-Za-z._-]+$ ]] || die "release name contains invalid characters" 64
  [[ "$name" != *".."* ]] || die "release reference is not canonical" 64
}

require_owner_path() {
  local path="$1"
  local kind="$2"
  local mode

  [ ! -L "$path" ] || die "$kind must not be a symlink: $path" 78
  [ -e "$path" ] || die "$kind is missing: $path" 75
  [ "$(/usr/bin/stat -f %u "$path")" = "$(/usr/bin/id -u)" ] ||
    die "$kind is not owned by the current user: $path" 78
  mode="$(/usr/bin/stat -f %Lp "$path")"
  [ $((8#$mode & 8#022)) -eq 0 ] || die "$kind is group/world writable: $path" 78
}

validate_release() {
  local ref="$1"
  local path="$release_root/$ref"
  local expected_version="${ref#releases/}"
  local link_target
  local component
  local component_path
  local resolved
  local name
  local version_output

  expected_version="${expected_version%%-*}"
  require_owner_path "$path" "release directory"
  [ -d "$path" ] || die "release is not a directory: $path" 78

  for name in codex codex-app-server codex-code-mode-host codex-responses-api-proxy; do
    [ -L "$path/$name" ] || die "release entry is not a direct symlink: $path/$name" 78
    link_target="$(/usr/bin/readlink "$path/$name")"
    case "$link_target" in
      /*|*".."*|*//*|./*|*/./*) die "release entry target is not canonical: $path/$name" 78 ;;
    esac
    component_path="$path"
    while IFS= read -r component; do
      [ -n "$component" ] || die "release entry target has an empty component" 78
      component_path="$component_path/$component"
      [ "$component_path" = "$path/$link_target" ] && break
      require_owner_path "$component_path" "release intermediate directory"
      [ -d "$component_path" ] || die "release intermediate is not a directory: $component_path" 78
    done < <(/usr/bin/printf '%s\n' "$link_target" | /usr/bin/tr '/' '\n')
    if ! resolved="$(/bin/realpath "$path/$name")"; then
      die "release entry cannot be resolved: $path/$name" 78
    fi
    case "$resolved" in
      "$path"/*) ;;
      *) die "release entry escapes its release: $path/$name" 78 ;;
    esac
    require_owner_path "$resolved" "release executable"
    [ -f "$resolved" ] && [ -x "$resolved" ] || die "release entry is not executable: $resolved" 78
  done

  if ! version_output="$(run_version_command "$path/codex")"; then
    die "codex version command failed: $ref" 78
  fi
  [ "$version_output" = "codex-cli $expected_version" ] ||
    die "codex version does not match release reference: $ref" 78
  if ! version_output="$(run_version_command "$path/codex-app-server")"; then
    die "app-server version command failed: $ref" 78
  fi
  [ "$version_output" = "codex-app-server $expected_version" ] ||
    die "app-server version does not match release reference: $ref" 78
}

read_direct_link() {
  local path="$1"
  [ -L "$path" ] || die "expected a direct symlink: $path" 78
  /usr/bin/readlink "$path"
}

atomic_link() {
  local name="$1"
  local target="$2"
  local scratch

  scratch="$(/usr/bin/mktemp -d "$release_root/.release-switch.XXXXXX")"
  if ! /bin/ln -s "$target" "$scratch/$name"; then
    /bin/rmdir "$scratch"
    return 1
  fi
  if ! /bin/mv -fh "$scratch/$name" "$release_root/$name"; then
    /usr/bin/unlink "$scratch/$name"
    /bin/rmdir "$scratch"
    return 1
  fi
  /bin/rmdir "$scratch"
}

acquire_switch_lock() {
  switch_lock="$release_root/.release-switch.lock.d"
  if ! /bin/mkdir -m 0700 "$switch_lock"; then
    die "release switch lock is already held or stale: $switch_lock" 75
  fi
  trap 'release_switch_cleanup_lock' EXIT
  trap 'exit 70' HUP INT TERM
}

release_switch_cleanup_lock() {
  if [ -n "${switch_lock:-}" ] && [ -d "$switch_lock" ]; then
    /bin/rmdir "$switch_lock" || true
  fi
}

restore_prior_links() {
  local current_now
  local previous_now

  current_now="$(/usr/bin/readlink "$release_root/current" 2>/dev/null)" || return 1
  previous_now="$(/usr/bin/readlink "$release_root/previous-custom" 2>/dev/null)" || return 1
  [ "$current_now" = "$target" ] && [ "$previous_now" = "$expected_current" ] || return 1
  atomic_link current "$expected_current" || return 1
  if ! atomic_link previous-custom "$previous_before"; then
    return 2
  fi
  [ "$(/usr/bin/readlink "$release_root/current")" = "$expected_current" ] &&
    [ "$(/usr/bin/readlink "$release_root/previous-custom")" = "$previous_before" ]
}

command_name="${1:-}"
[ "$command_name" = preflight ] || [ "$command_name" = switch ] || {
  usage >&2
  exit 64
}
shift

[[ "$VERSION_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] ||
  die "version timeout must be a positive integer" 64
[ -x "$VERSION_TIMEOUT_BIN" ] || die "version timeout executable is unavailable" 75

expected_current=""
target=""
release_root="$DEFAULT_ROOT"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --expected-current)
      [ "$#" -ge 2 ] || die "--expected-current requires a value" 64
      expected_current="$2"
      shift 2
      ;;
    --target)
      [ "$#" -ge 2 ] || die "--target requires a value" 64
      target="$2"
      shift 2
      ;;
    --root)
      [ "$#" -ge 2 ] || die "--root requires a value" 64
      release_root="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) die "unknown option: $1" 64 ;;
  esac
done

[ -n "$expected_current" ] || die "--expected-current is required" 64
[ -n "$target" ] || die "--target is required" 64
[ "$expected_current" != "$target" ] || die "expected-current and target must differ" 64
case "$release_root" in
  /*) ;;
  *) die "--root must be absolute" 64 ;;
esac
[[ "$release_root" =~ ^/[0-9A-Za-z._/-]+$ ]] || die "--root contains unsupported characters" 64

validate_release_ref "$expected_current"
validate_release_ref "$target"
require_owner_path "$release_root" "release root"
[ -d "$release_root" ] || die "release root is not a directory" 78
require_owner_path "$release_root/releases" "releases directory"
[ -d "$release_root/releases" ] || die "releases path is not a directory" 78
validate_release "$expected_current"
validate_release "$target"

current_before="$(read_direct_link "$release_root/current")"
previous_before="$(read_direct_link "$release_root/previous-custom")"

if [ "$current_before" = "$target" ] && [ "$previous_before" = "$expected_current" ]; then
  json_result alreadyApplied "$previous_before" "$current_before"
  exit 0
fi
[ "$current_before" = "$expected_current" ] ||
  die "stale current release: expected $expected_current, found $current_before" 78

if [ "$command_name" = preflight ]; then
  [ ! -e "$release_root/.release-switch.lock.d" ] &&
    [ ! -L "$release_root/.release-switch.lock.d" ] ||
    die "release switch lock is already held or stale: $release_root/.release-switch.lock.d" 75
  json_result ready "$previous_before" "$current_before"
  exit 0
fi

acquire_switch_lock

# Re-read after acquiring the lock. Another completed invocation is idempotent.
current_before="$(read_direct_link "$release_root/current")"
previous_before="$(read_direct_link "$release_root/previous-custom")"
if [ "$current_before" = "$target" ] && [ "$previous_before" = "$expected_current" ]; then
  json_result alreadyApplied "$previous_before" "$current_before"
  exit 0
fi
[ "$current_before" = "$expected_current" ] ||
  die "stale current release after lock acquisition" 78
require_owner_path "$release_root" "release root"
require_owner_path "$release_root/releases" "releases directory"
validate_release "$expected_current"
validate_release "$target"

atomic_link previous-custom "$expected_current" || die "failed to update previous-custom" 70
if ! atomic_link current "$target"; then
  if ! atomic_link previous-custom "$previous_before"; then
    die "failed to update current and failed to restore previous-custom" 70
  fi
  die "failed to update current; previous-custom restored" 70
fi

current_after="$(read_direct_link "$release_root/current")"
previous_after="$(read_direct_link "$release_root/previous-custom")"
if [ "$current_after" != "$target" ] || [ "$previous_after" != "$expected_current" ]; then
  die "post-switch link state is ambiguous; refusing to overwrite it" 70
fi

validation_error=""
if ! validation_error="$(validate_release "$current_after" 2>&1)"; then
  restore_status=0
  restore_prior_links || restore_status=$?
  case "$restore_status" in
    0) die "post-switch release validation failed; prior links restored: $validation_error" 70 ;;
    2) die "post-switch release validation failed; current restored but previous-custom restoration failed: $validation_error" 70 ;;
    *) die "post-switch release validation failed and link restoration is ambiguous: $validation_error" 70 ;;
  esac
fi
json_result switched "$previous_after" "$current_after"
