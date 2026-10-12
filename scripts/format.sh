#!/usr/bin/env bash
#
# Formatting and syntax checks, in one place.
#
#   ./scripts/format.sh          rewrite what rustfmt can, check the rest
#   ./scripts/format.sh --check  change nothing, fail if anything is off
#

set -euo pipefail
cd "$(dirname "$0")/.."

check=0
if [ "${1:-}" = "--check" ]; then
  check=1
elif [ $# -gt 0 ]; then
  echo "usage: $(basename "$0") [--check]" >&2
  exit 2
fi

for tool in cargo node; do
  command -v "$tool" > /dev/null || {
    echo "$tool is not on PATH" >&2
    exit 1
  }
done

# Collected rather than exited on, so one bad file still shows the state of the
# rest: a syntax error in the page should not hide a stale formatting of the
# crate that caused it.
bad=()

echo "== rust (rustfmt) =="
if [ "$check" = 1 ]; then
  cargo fmt --all -- --check || bad+=("rust: rustfmt wants changes - run ./scripts/format.sh")
else
  cargo fmt --all || bad+=("rust: rustfmt could not format everything")
fi

echo
echo "== javascript (node --check) =="
js=(web/*.js web/*.mjs)
for file in "${js[@]}"; do
  node --check "$file" || bad+=("js: $file")
done

echo
echo "== shell (bash -n) =="
sh=(scripts/*.sh)
for file in "${sh[@]}"; do
  bash -n "$file" || bad+=("shell: $file")
done

echo
if [ "${#bad[@]}" -gt 0 ]; then
  for line in "${bad[@]}"; do
    echo "FAIL: $line" >&2
  done
  exit 1
fi

echo "ok: rust formatting, ${#js[@]} javascript files and ${#sh[@]} shell scripts"
