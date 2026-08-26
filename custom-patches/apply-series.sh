#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
series_name=${2:-rust-v0.149.0}
series_dir="$script_dir/$series_name"
manifest="$series_dir/series.toml"
target_repo=${1:-.}

die() {
    printf 'apply-series: %s\n' "$*" >&2
    exit 1
}

git -C "$target_repo" rev-parse --git-dir >/dev/null 2>&1 \
    || die "target is not a Git repository: $target_repo"

test -f "$manifest" || die "unknown or incomplete patch series: $series_name"

test -z "$(git -C "$target_repo" status --porcelain)" \
    || die "target worktree must be clean"

base_commit=$(sed -n 's/^base_commit = "\([0-9a-f]*\)"$/\1/p' "$manifest")
final_tree=$(sed -n 's/^final_tree = "\([0-9a-f]*\)"$/\1/p' "$manifest")
test -n "$base_commit" || die "manifest has no base_commit"
test -n "$final_tree" || die "manifest has no final_tree"

current_commit=$(git -C "$target_repo" rev-parse HEAD)
test "$current_commit" = "$base_commit" \
    || die "expected base $base_commit, found $current_commit"

patch_files=$(sed -n 's/^file = "\([^"]*\)"$/\1/p' "$manifest")
test "$(printf '%s\n' "$patch_files" | sed '/^$/d' | wc -l | tr -d ' ')" = "19" \
    || die "manifest must contain exactly 19 patches"

set --
for patch_file in $patch_files; do
    test -f "$series_dir/$patch_file" || die "missing patch: $patch_file"
    expected_sha=$(awk -v file="$patch_file" '
        $0 == "file = \"" file "\"" {
            getline
            sub(/^sha256 = \"/, "")
            sub(/\"$/, "")
            print
            exit
        }
    ' "$manifest")
    test -n "$expected_sha" || die "manifest has no checksum for $patch_file"
    if command -v shasum >/dev/null 2>&1; then
        actual_sha=$(shasum -a 256 "$series_dir/$patch_file" | awk '{print $1}')
    elif command -v sha256sum >/dev/null 2>&1; then
        actual_sha=$(sha256sum "$series_dir/$patch_file" | awk '{print $1}')
    else
        die "shasum or sha256sum is required"
    fi
    test "$actual_sha" = "$expected_sha" || die "checksum mismatch: $patch_file"
    set -- "$@" "$series_dir/$patch_file"
done

git -C "$target_repo" am --3way "$@" \
    || die "git am failed; inspect the target and run 'git am --abort' to restore the base"

applied_tree=$(git -C "$target_repo" rev-parse 'HEAD^{tree}')
test "$applied_tree" = "$final_tree" \
    || die "applied tree mismatch: expected $final_tree, found $applied_tree"

printf 'Applied P001-P019; final tree %s\n' "$applied_tree"
