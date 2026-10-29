#!/usr/bin/env bash
# Moves data between the Java emulator and datastore-oxide through Admin export/import, both ways,
# and checks that lookups and queries on the imported copy return the same entities.
#
# Usage: tests/differential/export-interop.sh <java-host:port> <oxide-host:port> <host-dir> <java-dir>
#   <host-dir> is a directory shared with the Java emulator, which sees it as <java-dir> (for
#   example a container volume: tools/javaemu.sh mounts $WORK at /work). Both emulators must run
#   in memory (--testing) and get fresh projects (interop-*), so they may already hold other data.
set -euo pipefail

JAVA="$1" OXIDE="$2" HOST_DIR="$3" JAVA_DIR="$4"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
run=$(date +%s)

post() { curl -sS -X POST -H 'content-type: application/json' --data-binary "$3" "http://$1$2"; }

# Every value type (the value_types scenario's entity) plus hierarchy, namespaces, ids.
seed=$(python3 - "$ROOT" <<'EOF'
import sys
text = open(sys.argv[1] + "/tests/conformance/scenarios/value_types.scn").read()
print(text.split("POST /v1/projects/p:commit\n", 1)[1].split("\n\n", 1)[0])
EOF
)
more='{"mode":"NON_TRANSACTIONAL","mutations":[
 {"upsert":{"key":{"path":[{"kind":"K","name":"a"},{"kind":"C","id":"7"}]},"properties":{"v":{"integerValue":"1"},"s":{"stringValue":"x","excludeFromIndexes":true}}}},
 {"upsert":{"key":{"partitionId":{"namespaceId":"ns1"},"path":[{"kind":"K","name":"n"}]},"properties":{"arr":{"arrayValue":{"values":[{"integerValue":"1"},{"stringValue":"u","excludeFromIndexes":true}]}}}}}]}'
keys='{"keys":[{"path":[{"kind":"T","name":"all"}]},{"path":[{"kind":"K","name":"a"},{"kind":"C","id":"7"}]},{"partitionId":{"namespaceId":"ns1"},"path":[{"kind":"K","name":"n"}]}]}'

# Lookup and query results without versions, with key projects replaced and maps sorted.
canonical() {
  python3 -c '
import json, sys
d = json.load(sys.stdin)
def strip(v):
    if isinstance(v, dict):
        return {k: strip(x) for k, x in sorted(v.items()) if k not in ("version", "cursor", "endCursor", "projectId")}
    if isinstance(v, list):
        return [strip(x) for x in v]
    return v
print(json.dumps(strip(d), indent=1, sort_keys=True))'
}

check() { # check <name> <src> <src-project> <dst> <dst-project>
  local a b
  a=$(post "$2" "/v1/projects/$3:lookup" "$keys" | canonical)
  b=$(post "$4" "/v1/projects/$5:lookup" "$keys" | canonical)
  if [ "$a" != "$b" ]; then echo "FAIL $1: lookups differ"; diff <(echo "$a") <(echo "$b") | head -40; exit 1; fi
  a=$(post "$2" "/v1/projects/$3:runQuery" '{"query":{"kind":[{"name":"K"}]}}' | canonical)
  b=$(post "$4" "/v1/projects/$5:runQuery" '{"query":{"kind":[{"name":"K"}]}}' | canonical)
  if [ "$a" != "$b" ]; then echo "FAIL $1: queries differ"; diff <(echo "$a") <(echo "$b") | head -40; exit 1; fi
  echo "ok   $1"
}

# oxide -> Java
src=interop-o$run dst=interop-j$run
post "$OXIDE" "/v1/projects/$src:commit" "$seed" > /dev/null
post "$OXIDE" "/v1/projects/$src:commit" "$more" > /dev/null
post "$OXIDE" "/v1/projects/$src:export" "{\"outputUrlPrefix\":\"$HOST_DIR/$src\"}" > /dev/null
meta=$(cd "$HOST_DIR/$src" && ls -d */*.overall_export_metadata | head -1)
post "$JAVA" "/v1/projects/$dst:import" "{\"inputUrl\":\"$JAVA_DIR/$src/$meta\"}" > /dev/null
check "oxide export -> Java import" "$OXIDE" "$src" "$JAVA" "$dst"

# Java -> oxide
src=interop-j2$run dst=interop-o2$run
post "$JAVA" "/v1/projects/$src:commit" "$seed" > /dev/null
post "$JAVA" "/v1/projects/$src:commit" "$more" > /dev/null
post "$JAVA" "/v1/projects/$src:export" "{\"outputUrlPrefix\":\"$JAVA_DIR/$src\"}" > /dev/null
meta=$(cd "$HOST_DIR/$src" && ls -d */*.overall_export_metadata | head -1)
post "$OXIDE" "/v1/projects/$dst:import" "{\"inputUrl\":\"$HOST_DIR/$src/$meta\"}" > /dev/null
check "Java export -> oxide import" "$JAVA" "$src" "$OXIDE" "$dst"
