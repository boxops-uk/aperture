#!/usr/bin/env bash
# Index a .NET checkout into a fresh *Glean* database, the same walk that
# index-repo.sh puts into Fjord.
#
#   ./clients/dotnet/index-repo-glean.sh /path/to/some/checkout [database] [indexer flags...]
#
# Two phases, two clocks, and they are reported separately because they are separately
# interesting:
#
#   emit  — the Roslyn walk, writing Glean JSON batches (one file per block). This is the
#           half that corresponds to a Fjord run's walk *and* its write, minus the
#           interning: no fact has an id yet.
#   load  — `glean write`, which parses each batch, interns every nested fact bottom-up
#           and assigns the ids. This is the half that corresponds to Fjord's server.
#
# The honest total for this side is emit + load. Saying it as one number would hide that
# Glean's pipeline has a file in the middle and Fjord's does not, which is a real
# difference between them and not a detail of the harness.
#
# Environment:
#   GLEAN_BIN     the glean CLI (default: the optimised build under ~/glean)
#   GLEAN_LIB     where its shared libraries live (default: ~/.hsthrift/lib[64])
#   GLEAN_DIR     scratch root (default: /tmp/fj-glean)
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "usage: $0 <path-to-checkout-or-solution> [database] [indexer flags...]" >&2
    exit 2
fi

source_path="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
shift

database="fjbench"
if [ $# -gt 0 ] && [[ "$1" != --* ]]; then
    database="$1"
    shift
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scratch="${GLEAN_DIR:-/tmp/fj-glean}"
schema="$root/clients/dotnet/glean"

glean="${GLEAN_BIN:-$HOME/glean/.build/opt/dist-newbuild/build/x86_64-linux/ghc-9.6.7/glean-0.2.0.0/x/glean/build/glean/glean}"
export LD_LIBRARY_PATH="${GLEAN_LIB:-$HOME/.hsthrift/lib:$HOME/.hsthrift/lib64}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

[ -x "$glean" ] || { echo "no glean CLI at $glean (set GLEAN_BIN)" >&2; exit 1; }

# A fresh database and a fresh batch directory each run, for the reason index-repo.sh
# gives: a second run over a database that already holds the answers measures dedup.
rm -rf "$scratch"
mkdir -p "$scratch/json"

echo "=== emit"
emit_start=$(date +%s)
dotnet run --project "$root/clients/dotnet/Boxops.Fjord.Indexer" --configuration Release -- \
    --source "$source_path" \
    --glean-out "$scratch/json" \
    "$@"
emit_end=$(date +%s)

batches=$(find "$scratch/json" -name '*.json' | wc -l)
volume=$(du -sh "$scratch/json" | cut -f1)

echo
echo "=== load: $batches batch file(s), $volume of JSON"

# One invocation, and the glob rather than xargs to get it: every `glean write` process
# opens the database, builds an inventory from its schema and warms its own lookup cache,
# so several invocations would be measuring process startup as well — and xargs splits at
# ~128 kB of arguments, which for six thousand paths is five of them. A glob is one
# argument list, bounded by ARG_MAX (~2 MB here, against ~600 kB of paths).
#
# -j (--maxConcurrency) is Glean's own knob for how many batches it parses and writes at
# once. Its default is 20 and its external indexer driver uses the same; 8 is this box's
# core count, and the Fjord side of the comparison is given --jobs 8 for the same
# reason.
#
# --finish seals the database when the last batch lands, which is what makes the
# `complete` state — and therefore the query timings taken afterwards — comparable with
# a Fjord database that has been through `fjord finish`.
load_start=$(date +%s)
# `create` rather than a create followed by a write: the CLI's create takes the batches
# too, so it is one process that makes the database, writes every batch and seals it.
"$glean" --db-root "$scratch/db" --schema "dir:$schema" \
    create --db "$database/0" -j 8 --finish \
    "$scratch"/json/*.json
load_end=$(date +%s)

echo
echo "emit   $((emit_end - emit_start))s"
echo "load   $((load_end - load_start))s"
echo "total  $((load_end - emit_start))s"
echo "db     $(du -sh "$scratch/db" | cut -f1)   json $volume"
echo
echo "to ask it things:"
echo "  export LD_LIBRARY_PATH=$LD_LIBRARY_PATH"
echo "  $glean --db-root $scratch/db --schema dir:$schema shell --db $database/0"
echo "  $glean --db-root $scratch/db --schema dir:$schema query --db $database/0 \\"
echo "      'fjbench.SearchByName { name = \"Parse\" }' --stats -"
