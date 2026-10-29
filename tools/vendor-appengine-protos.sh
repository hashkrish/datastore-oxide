#!/usr/bin/env bash
# Vendors the App Engine v3 datastore protos (and their imports) from GoogleCloudPlatform/
# appengine-java-standard (Apache-2.0) into proto/appengine at a pinned commit.
#
# Usage: tools/vendor-appengine-protos.sh [commit]
set -euo pipefail

COMMIT="${1:-$(cat "$(dirname "$0")/../proto/appengine.commit")}"
DEST="$(cd "$(dirname "$0")/.." && pwd)/proto/appengine"
BASE="https://raw.githubusercontent.com/GoogleCloudPlatform/appengine-java-standard/${COMMIT}"

rm -rf "$DEST"
mkdir -p "$DEST"
for f in datastore_v3_bytes action entity_bytes snapshot task queue retry target group; do
  curl -fsSL "$BASE/protobuf/api/$f.proto" -o "$DEST/$f.proto"
done
curl -fsSL "$BASE/LICENSE" -o "$DEST/LICENSE"
echo "$COMMIT" > "$DEST/../appengine.commit"
echo "vendored appengine-java-standard@$COMMIT into $DEST"
