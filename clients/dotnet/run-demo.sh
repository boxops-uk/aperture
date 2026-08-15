#!/usr/bin/env bash
# Build the server, start it on a scratch socket, and run the .NET producer at it.
#
# The readiness file is the synchronisation: it appears only once the listener is
# accepting, so waiting on it is a signal rather than a race (operations §5).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="${APERTURE_DEMO_DIR:-/tmp/ap-demo}"

cargo build --manifest-path "$root/Cargo.toml" --bin aperture

rm -rf "$scratch"
mkdir -p "$scratch"

"$root/target/debug/aperture" --data-dir "$scratch/db" create code

"$root/target/debug/aperture" --data-dir "$scratch/db" serve \
    --socket "$scratch/aperture.sock" \
    --ready-file "$scratch/ready" &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
    [ -e "$scratch/ready" ] && break
    sleep 0.1
done

[ -e "$scratch/ready" ] || { echo "the server never became ready" >&2; exit 1; }

dotnet run --project "$root/clients/dotnet/Aperture.Demo" -- --socket "$scratch/aperture.sock"
