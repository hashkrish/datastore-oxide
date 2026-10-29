#!/usr/bin/env bash
# Vendors the googleapis protos we compile (plus their transitive imports) into proto/googleapis
# at a pinned commit. google/protobuf/* well-known types come from protoc's include dir instead.
#
# Usage: tools/vendor-googleapis.sh [commit]
set -euo pipefail

COMMIT="${1:-$(cat "$(dirname "$0")/../proto/googleapis.commit")}"
DEST="$(cd "$(dirname "$0")/.." && pwd)/proto/googleapis"
BASE="https://raw.githubusercontent.com/googleapis/googleapis/${COMMIT}"

ROOTS=(
  google/datastore/v1/datastore.proto
  google/datastore/admin/v1/datastore_admin.proto
  google/longrunning/operations.proto
)

rm -rf "$DEST"
mkdir -p "$DEST"
declare -A seen
queue=("${ROOTS[@]}")
while ((${#queue[@]})); do
  f="${queue[0]}"
  queue=("${queue[@]:1}")
  [[ -n "${seen[$f]:-}" || "$f" == google/protobuf/* ]] && continue
  seen[$f]=1
  mkdir -p "$DEST/$(dirname "$f")"
  curl -fsSL "$BASE/$f" -o "$DEST/$f"
  while read -r imp; do
    queue+=("$imp")
  done < <(sed -nE 's/^import (public )?"([^"]+)";.*/\2/p' "$DEST/$f")
done

echo "$COMMIT" > "$(dirname "$DEST")/googleapis.commit"
echo "vendored ${#seen[@]} files from googleapis@${COMMIT}"
