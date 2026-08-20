#!/usr/bin/env bash
# Build the WebAssembly shell and put it where the site can import it.
#
# Three steps and a number: cargo compiles the `cdylib`, `wasm-bindgen` writes
# the JS glue and the `.d.ts` beside it, and `wasm-opt` shrinks what is left.
# The byte size is printed because it is the cost model — the artifact is
# downloaded before a reader sees anything.
#
# `wasm/` is its own workspace, so this is the only way it is ever built: no
# `cargo build` at the root reaches it, on purpose.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out=${1:-"$root/web/src/wasm"}
profile=${PROFILE:-release}

command -v wasm-bindgen >/dev/null || {
    echo "wasm-bindgen is not on PATH: cargo install wasm-bindgen-cli --locked" >&2
    exit 1
}

# **The versions have to match exactly.** The CLI reads a schema the crate
# encodes into the module, and a mismatch fails with a message about a section
# rather than about versions.
crate_version=$(cd "$root/wasm" && cargo tree -p wasm-bindgen --depth 0 2>/dev/null |
    head -1 | sed -E 's/^wasm-bindgen v([0-9.]+).*/\1/')
cli_version=$(wasm-bindgen --version | awk '{print $2}')
if [[ "$crate_version" != "$cli_version" ]]; then
    echo "wasm-bindgen crate is $crate_version but the CLI is $cli_version;" >&2
    echo "install the matching CLI: cargo install wasm-bindgen-cli --version $crate_version" >&2
    exit 1
fi

echo "==> cargo build --$profile --target wasm32-unknown-unknown"
(cd "$root/wasm" && cargo build "--$profile" --target wasm32-unknown-unknown)

artifact="$root/wasm/target/wasm32-unknown-unknown/$profile/fjord_wasm.wasm"
echo "==> wasm-bindgen --target web --out-dir ${out#"$root"/}"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" "$artifact"

module="$out/fjord_wasm_bg.wasm"
if command -v wasm-opt >/dev/null; then
    echo "==> wasm-opt -Oz"
    wasm-opt -Oz "$module" -o "$module"
else
    echo "==> wasm-opt not on PATH — shipping unoptimised (install binaryen to shrink it)"
fi

printf '==> %s: %s bytes\n' "${module#"$root"/}" "$(wc -c <"$module")"
