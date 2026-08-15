#!/usr/bin/env bash
# Regenerate the golden blocks the Rust client's test compares itself against.
#
# Phase 9e's acceptance criterion: the two clients produce byte-identical blocks for
# the same facts. This writes the C# side's answer; `aperture-client`'s
# `byte_identical_with_the_dotnet_client` test writes the Rust side's and compares.
#
# Run it when the wire format changes on purpose. If it changes by accident, the Rust
# test fails first and this script is how you see what moved.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$root/clients/dotnet/golden/blocks.txt"

mkdir -p "$(dirname "$out")"

dotnet run --project "$root/clients/dotnet/Aperture.Demo" -- --golden "$out"

echo
echo "now check the Rust side still agrees:"
echo "  cargo test -p aperture-client byte_identical"
