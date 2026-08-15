#!/usr/bin/env bash
# Create a database, serve it, seed it, and measure — the whole sequence in one
# command, so a performance run is repeatable rather than remembered.
#
#   ./scripts/bench.sh                      # 10k files x 5 decls, 8 connections
#   FILES=100000 CONNS=16 ./scripts/bench.sh
#   KEEP=1 ./scripts/bench.sh               # leave the server up afterwards
#
# **Release, always.** A debug build of the executor is not the thing being measured,
# and the difference is not a constant factor you can divide out later.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Short, because a Unix socket path has about a hundred bytes to live in and a temp
# directory under a long prefix silently exceeds it.
dir="${APERTURE_BENCH_DIR:-/tmp/ap-bench}"

files="${FILES:-10000}"
decls="${DECLS:-5}"
conns="${CONNS:-8}"
runs="${RUNS:-200}"
block="${BLOCK:-1000}"

cargo build --release --manifest-path "$root/Cargo.toml" --bin aperture --example loadgen

aperture="$root/target/release/aperture"
loadgen="$root/target/release/examples/loadgen"

rm -rf "$dir"
mkdir -p "$dir"

"$aperture" --data-dir "$dir" create code

"$aperture" --data-dir "$dir" serve --ready-file "$dir/ready" >"$dir/server.log" 2>&1 &
server=$!

cleanup() {
    if [ -z "${KEEP:-}" ]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    else
        echo
        echo "server left running (pid $server) over $dir"
        echo "  $aperture --data-dir $dir query code 'F where src.File F' --format count --timing"
    fi
}
trap cleanup EXIT

for _ in $(seq 1 200); do
    [ -e "$dir/ready" ] && break
    sleep 0.1
done

[ -e "$dir/ready" ] || { echo "the server never became ready; see $dir/server.log" >&2; exit 1; }

echo "aperture bench — $files files x $decls decls, $conns connections"
echo "  data dir $dir"
echo

"$loadgen" \
    --socket "$dir/aperture.sock" \
    --database code \
    --files "$files" \
    --decls-per-file "$decls" \
    --connections "$conns" \
    --runs "$runs" \
    --block "$block"

echo
"$aperture" --data-dir "$dir" describe code | head -20
