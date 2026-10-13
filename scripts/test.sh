#!/usr/bin/env bash
#
#   ./scripts/test.sh             clippy, cargo test, the wasm module builds and
#                                 imports nothing, and the page's javascript tests
#   ./scripts/test.sh --regtest   the above, then the regtest scripts
#
# The regtest scripts drive the docker stack and depend on each other's order
# (init funds the network the rest assume), so they are run one at a time.

source "$(dirname "$0")/lib.sh"

regtest=0
case "${1:-}" in
  "") ;;
  --regtest) regtest=1 ;;
  *)
    echo "usage: $(basename "$0") [--regtest]" >&2
    exit 2
    ;;
esac

# Collected rather than exited on, so one failure still shows the state of the
# rest: a broken test binary should not hide a wasm build that never happened.
bad=()
# Scripts that said they were skipping themselves, which is not the same as
# passing
skipped=()

# Both the container run and the regtest scripts start a panel on 8091, so one
# of them has to be stopped first: checked up here, so a clash is refused before
# `cargo test` rather than after it.
if [ "$regtest" = 1 ] &&
  "${COMPOSE[@]}" ps --status running --services 2>/dev/null | grep -qx regtestd; then
  echo "the web profile is running and holds port 8091." >&2
  echo "Stop it first: docker compose -f docker/docker-compose.yml --profile web down" >&2
  exit 1
fi

echo "== cargo test =="
cargo test || bad+=("cargo test")

# Warnings are errors here because nothing else would look: a lint that only
# shows up in a log is one that survives every run. `--cap-lints allow` on
# dependencies means this only denies our own.
echo
echo "== clippy =="
cargo clippy --all-targets --workspace -- -D warnings || bad+=("clippy")

# A different target. Something std-only compiles natively and fails here, and
# the page cannot load a module that was never built.
echo
echo "== wasm build =="
cargo build --release --target wasm32-unknown-unknown -p swapsite-wasm ||
  bad+=("wasm build")

# The module having no imports is what lets the page instantiate it with no js
# shim. A dependency that drags one in should fail here, not in a browser.
echo
echo "== wasm imports =="
WASM="target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
node -e '
  const fs = require("fs");
  const imports = WebAssembly.Module.imports(new WebAssembly.Module(fs.readFileSync(process.argv[1])));
  if (imports.length > 0) {
    throw new Error(`the module imports ${JSON.stringify(imports)}`);
  }
  console.log(`the module imports nothing (${fs.statSync(process.argv[1]).size} bytes)`);
' "$WASM" || bad+=("wasm imports")

# The page's own shared code. Node's test runner is built in, so this needs
# nothing installed.
echo
echo "== javascript tests =="
node --test web/*.test.mjs || bad+=("javascript tests")

if [ "$regtest" = 1 ]; then
  echo
  echo "== regtest scripts =="
  scripts=(
    regtest-init
    regtest-onchain
    regtest-btc2ln
    regtest-ln2btc
    regtest-maker
    regtest-wasm
    regtest-coordinator
    regtest-recovery
    regtest-ui
  )

  for name in "${scripts[@]}"; do
    log="$WORK/$name.log"
    if ! ./scripts/"$name".sh > "$log" 2>&1; then
      echo "FAIL: $name - log: $log" >&2
      tail -12 "$log" >&2
      bad+=("$name")
    elif skip=$(grep -m1 '^skipped:' "$log"); then
      # Exiting 0 with a reason is a skip.
      echo "skip: $name - ${skip#skipped: }"
      skipped+=("$name")
    else
      echo "ok:   $name"
    fi
  done
fi

echo
if [ "${#bad[@]}" -gt 0 ]; then
  echo "FAIL: ${bad[*]}" >&2
  exit 1
fi

if [ "$regtest" = 1 ]; then
  echo "ok: clippy, cargo test, the wasm module and ${#scripts[@]} regtest scripts"
  if [ "${#skipped[@]}" -gt 0 ]; then
    echo "note: ${#skipped[@]} skipped - ${skipped[*]}"
  fi
else
  echo "ok: clippy, cargo test, the wasm module and the javascript tests"
fi
