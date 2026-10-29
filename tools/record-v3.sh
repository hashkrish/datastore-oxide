#!/bin/bash
# Records the v3 scenarios' goldens from the Java emulators (jp1 on 18100, jf1 on 18101).
set -e
cd "$(dirname "$0")/.."
names=$(ls tests/conformance/scenarios/ | grep '^v3_' | sed 's/\.scn$//')
[ $# -gt 0 ] && names="$*"
cargo run -q -p conformance --bin record-conformance -- http://localhost:18100 $names
cargo run -q -p conformance --bin record-conformance -- --firestore http://localhost:18101 $names
