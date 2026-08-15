#!/usr/bin/env bash
# Index a .NET checkout into a fresh Aperture database, and leave it there to query.
#
#   ./clients/dotnet/index-repo.sh /path/to/some/checkout [database]
#
# The readiness file is the synchronisation, as in run-demo.sh: it appears only once the
# listener is accepting, so waiting on it is a signal rather than a race (operations §5).
#
# Everything the indexer takes is passed through, so the knobs are its own:
#
#   ./clients/dotnet/index-repo.sh ~/src/OrchardCore code --syntax-only --max-files 5000
#
# The server is left running until this script exits and the database survives it — the
# last lines say how to open a shell on it.
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "usage: $0 <path-to-checkout-or-solution> [database] [indexer flags...]" >&2
    exit 2
fi

source_path="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
shift

database="code"
if [ $# -gt 0 ] && [[ "$1" != --* ]]; then
    database="$1"
    shift
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="${APERTURE_INDEX_DIR:-/tmp/ap-index}"

# The socket is derived from the store root rather than chosen (operations §9), so
# `aperture query` finds the same server this indexer wrote to without being told where.
socket="$scratch/db/aperture.sock"

cargo build --manifest-path "$root/Cargo.toml" --bin aperture --release
aperture="$root/target/release/aperture"

# A fresh database each run: the point of indexing something large is measuring what it
# costs, and a second run over a database that already holds the answers measures dedup.
rm -rf "$scratch"
mkdir -p "$scratch"

"$aperture" --data-dir "$scratch/db" create "$database"

"$aperture" --data-dir "$scratch/db" serve --ready-file "$scratch/ready" &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT

for _ in $(seq 1 200); do
    [ -e "$scratch/ready" ] && break
    sleep 0.1
done

[ -e "$scratch/ready" ] || { echo "the server never became ready" >&2; exit 1; }

dotnet run --project "$root/clients/dotnet/Aperture.Indexer" --configuration Release -- \
    --source "$source_path" \
    --socket "$socket" \
    --database "$database" \
    "$@"

echo
echo "the database is at $scratch/db, and the server is about to stop. To ask it things:"
echo "  $aperture --data-dir $scratch/db serve &"
echo "  $aperture --data-dir $scratch/db query $database 'N where src.Module {name = N}' --limit 20 --timing"
echo "  $aperture --data-dir $scratch/db query $database 'X where X = src.SearchByName {name = \"Parse\"}' --profile"
