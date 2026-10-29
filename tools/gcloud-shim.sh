#!/usr/bin/env bash
# Makes `gcloud beta emulators datastore start` run datastore-oxide instead of the Java emulator.
#
# gcloud always executes <sdk>/platform/cloud-datastore-emulator/cloud_datastore_emulator, with no
# override, so this tool builds an *overlay SDK*: a directory that symlinks every entry of the
# real SDK except
#   - `bin/` (copied: the gcloud launcher derives the SDK root from its own path),
#   - `lib/`, `lib/googlecloudsdk/` and `lib/googlecloudsdk/core/` (real directories whose entries
#     are symlinks: gcloud finds the SDK root by walking up from realpath(core/)), and
#   - the datastore emulator directory, which gets a shim that execs datastore-oxide.
# The real SDK is never modified.
#
# gcloud also refuses to launch the emulator unless `java -version` reports Java 8+. The overlay's
# top-level `gcloud` wrapper therefore adds a stub `java` to PATH when no real Java is installed.
#
# Usage: tools/gcloud-shim.sh <overlay-dir> [datastore-oxide binary] [real sdk root]
#   then: <overlay-dir>/gcloud beta emulators datastore start ...
set -euo pipefail

OVERLAY="${1:?usage: gcloud-shim.sh <overlay-dir> [binary] [sdk-root]}"
BIN="$(realpath "${2:-$(dirname "$0")/../target/release/datastore-oxide}")"
SDK="$(realpath "${3:-$(dirname "$(dirname "$(realpath "$(command -v gcloud)")")")}")"

[[ -x "$BIN" ]] || { echo "datastore-oxide binary not found: $BIN (run cargo build --release)" >&2; exit 1; }
[[ -d "$SDK/platform" ]] || { echo "not a Cloud SDK root: $SDK" >&2; exit 1; }

# mirror_chain <src> <dst> [child...]: makes <dst> a real directory of symlinks to <src>'s entries,
# recursing (as real directories) into the given chain of children.
mirror_chain() {
  local src="$1" dst="$2"
  shift 2
  mkdir -p "$dst"
  for item in "$src"/* "$src"/.[!.]*; do
    [[ -e "$item" ]] || continue
    if [[ $# -gt 0 && "$(basename "$item")" == "$1" ]]; then
      mirror_chain "$item" "$dst/$1" "${@:2}"
    else
      ln -s "$item" "$dst/"
    fi
  done
}

rm -rf "$OVERLAY"
mkdir -p "$OVERLAY/platform/cloud-datastore-emulator"
for entry in "$SDK"/* "$SDK"/.[!.]*; do
  name="$(basename "$entry")"
  case "$name" in
    bin) cp -a "$entry" "$OVERLAY/bin" ;;
    lib) mirror_chain "$entry" "$OVERLAY/lib" googlecloudsdk core ;;
    platform) ;;
    *) ln -s "$entry" "$OVERLAY/$name" ;;
  esac
done
for entry in "$SDK"/platform/*; do
  [[ "$(basename "$entry")" == cloud-datastore-emulator ]] || ln -s "$entry" "$OVERLAY/platform/"
done

shim="$OVERLAY/platform/cloud-datastore-emulator/cloud_datastore_emulator"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$BIN" > "$shim"
chmod +x "$shim"

mkdir -p "$OVERLAY/java-stub"
cat > "$OVERLAY/java-stub/java" <<'STUB'
#!/bin/sh
# Satisfies gcloud's Java version check; datastore-oxide does not use a JVM.
echo 'openjdk version "25" (stub for datastore-oxide; no JVM is used)' >&2
STUB
cat > "$OVERLAY/gcloud" <<WRAPPER
#!/bin/sh
command -v java >/dev/null 2>&1 || PATH="$OVERLAY/java-stub:\$PATH"
exec "$OVERLAY/bin/gcloud" "\$@"
WRAPPER
chmod +x "$OVERLAY/java-stub/java" "$OVERLAY/gcloud"
echo "overlay SDK ready: $OVERLAY/gcloud (emulator -> $BIN)"
