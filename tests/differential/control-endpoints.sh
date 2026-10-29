#!/usr/bin/env bash
# Sends the same HTTP requests to two running emulators and diffs status line, headers and body.
# Exercises everything except the shutdown endpoints (see shutdown cases at the end, opt-in).
# Header lines are sorted (their order is not part of the contract).
#
# Usage: tests/differential/control-endpoints.sh <java-host:port> <oxide-host:port> [--shutdown]
set -uo pipefail

JAVA="$1" OXIDE="$2"
REQUESTS=(
  "GET /"
  "GET /?x=1"
  "POST /"
  "HEAD /"
  "GET /nope"
  "POST /nope"
  "GET /reset"
  "POST /reset"
  "GET /persist"
  "POST /persist"
  "GET /shutdown"
  "GET /_ah/admin/quit"
  "GET /_ah/admin/datastore"
  "OPTIONS /v1/projects/p:lookup"
  "OPTIONS /v1/projects/p:lookup|Origin: http://example.com"
  "OPTIONS /|Origin: http://x|Access-Control-Request-Method: POST"
  "OPTIONS /|Origin: http://x|Access-Control-Request-Headers: content-type,x-goog-api-client"
  "OPTIONS /|Access-Control-Request-Headers: a"
  "GET /|Origin: http://x"
  "GET /|Access-Control-Request-Headers: a"
  "GET /nope|Origin: http://x|Access-Control-Request-Headers: a,b"
  "POST /reset|Origin: http://x"
  "POST /persist|Origin: http://x"
)
[[ "${3:-}" == --shutdown ]] && REQUESTS+=("POST /shutdown")

fetch() { # base method path [header]
  local hdr=()
  [[ -n "${4:-}" ]] && IFS='|' read -ra hs <<< "$4" && for h in "${hs[@]}"; do hdr+=(-H "$h"); done
  local m=(-X "$2"); [[ "$2" == HEAD ]] && m=(-I)
  curl -s -i --max-time 5 "${m[@]}" "${hdr[@]}" "http://$1$3" | tr -d '\r' | sed -E '/^date:/Id' |
    awk 'NR==1 {print; next} !body && /^$/ {body=1; n=asort(h); for (i=1;i<=n;i++) print h[i]; print; next}
         !body {h[++k]=$0; next} {print} END {if (!body) {n=asort(h); for (i=1;i<=n;i++) print h[i]}}'
}

fail=0
for r in "${REQUESTS[@]}"; do
  IFS='|' read -r req extra <<< "$r"
  read -r method path <<< "$req"
  extra="${r#"$req"}"; extra="${extra#|}"
  a="$(fetch "$JAVA" "$method" "$path" "$extra")"
  b="$(fetch "$OXIDE" "$method" "$path" "$extra")"
  if [[ "$a" == "$b" ]]; then
    echo "ok    $r"
  else
    fail=1
    echo "DIFF  $r"
    diff <(echo "$a") <(echo "$b") | sed 's/^/      /'
  fi
done
exit $fail
