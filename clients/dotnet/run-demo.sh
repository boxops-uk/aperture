#!/usr/bin/env bash
# Build the server, start it on a scratch socket, and run the .NET producer at it.
#
# The readiness file is the synchronisation: it appears only once the listener is
# accepting, so waiting on it is a signal rather than a race (operations §5).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="${FJORD_DEMO_DIR:-/tmp/fj-demo}"

cargo build --manifest-path "$root/Cargo.toml" --bin fjord

rm -rf "$scratch"
mkdir -p "$scratch"

# The schema the demo producer writes against, named explicitly: `create` requires
# one, and this is the file the C# side states independently in `Program.cs` — which
# is the agreement the handshake fingerprint checks.
"$root/target/debug/fjord" --data-dir "$scratch/db" create code \
    --schema "$root/schemas/code.sigla"

"$root/target/debug/fjord" --data-dir "$scratch/db" serve \
    --socket "$scratch/fjord.sock" \
    --ready-file "$scratch/ready" &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
    [ -e "$scratch/ready" ] && break
    sleep 0.1
done

[ -e "$scratch/ready" ] || { echo "the server never became ready" >&2; exit 1; }

dotnet run --project "$root/clients/dotnet/Boxops.Fjord.Demo" -- --at "$scratch/fjord.sock//code"
