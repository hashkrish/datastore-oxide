# datastore-oxide

A Rust drop-in replacement for the Google Cloud Datastore emulator (`cloud_datastore_emulator`,
the Java emulator behind `gcloud beta emulators datastore`).
An independent project, not affiliated with or endorsed by Google (see [Trademarks](#trademarks)).

It serves everything the Java emulator serves, on one port:

- the Datastore v1 API over gRPC and REST (lookups, structured and GQL queries, transactions,
  id allocation), with the Java emulator's results, cursors, versions, errors and simulated
  eventual consistency;
- Firestore-in-Datastore mode (`--firestore_in_datastore_mode`);
- the Admin v1 API's export/import in the managed-export format (data moves to and from the Java
  emulator this way);
- composite-index bookkeeping (`index.yaml` / `datastore-indexes*.xml`, `--require_indexes`);
- the App Engine v3 API server (`apphosting.tools.devappserver2.CallHandler`) used by App Engine
  development servers;
- the `EmulatorConfig` service, the control endpoints (`/`, `/reset`, `/persist`, `/shutdown`) and
  CORS;
- an on-disk store (`--store_on_disk`, `--storage_file`).

Every behavior is checked against the Java emulator: its recorded responses are replayed by
`cargo test` (see `docs/reference-behavior.md`, including the known gaps). On top of that, it
serves production Datastore features the Java emulator lacks (listed in
`docs/superset-backlog.md`); `start --no_superset` turns them off, leaving exactly the Java
emulator's responses. The superset adds:

- aggregation queries (`COUNT` with `up_to`, `SUM`, `AVG`; structured or GQL);
- `OR`, multi-value `IN`, `NOT_IN`, `!=` and inequalities on several properties;
- property transforms (`increment`, `maximum`, `minimum`, server timestamps, array transforms)
  and property masks on commits, lookups and queries;
- point-in-time reads (`read_time`, an hour back);
- query explain (plan summary and execution statistics);
- vector values and vector search (`find_nearest`);
- production's commit limits (500 mutations, 1 MiB entities);
- Admin API index management, a long-running Operations service, export/import with
  production's `output_url`, labels and `gs://` URLs (stored in a local directory);
- App Engine v3 extras (`Touch`, `DeleteCursor`, index calls, `IN`/`NOT_EQUAL`/`NOT_IN`,
  full-text `search_query`, deferred Gets, read times, proper errors);
- a data viewer at `/_ah/admin/datastore`.

## Usage

```sh
cargo build --release
./target/release/datastore-oxide start --testing --port=8081
export DATASTORE_EMULATOR_HOST=localhost:8081
```

The binary accepts the same actions and flags as the Java emulator (`create`, `start`, `help`,
`license`; `--host`, `--port`, `--consistency`, `--store_on_disk`, `--testing`, …), plus
`start --no_superset` (above).

### Container image

```sh
podman build -t datastore-oxide .        # or docker
podman run --rm -p 8081:8081 datastore-oxide                     # stores data in /data/project
podman run --rm -p 8081:8081 datastore-oxide start --host=0.0.0.0 --port=8081 --testing
```

Tagged releases publish binaries (Linux and macOS, x86_64 and arm64) and
`ghcr.io/<owner>/datastore-oxide` images (`.github/workflows/release.yml`).

### With gcloud

gcloud always runs `<sdk>/platform/cloud-datastore-emulator/cloud_datastore_emulator` and requires
Java on `PATH`. `tools/gcloud-shim.sh` builds an overlay SDK that routes that to datastore-oxide
without modifying your installed SDK:

```sh
tools/gcloud-shim.sh ~/.cache/oxide-sdk
~/.cache/oxide-sdk/gcloud beta emulators datastore start --project=demo
```

## Performance

Measured on one machine, both emulators in the same `eclipse-temurin:21-jre` container behind
rootless podman, `--testing`, 16 concurrent gRPC clients for 10 s, 1000 entities
(`cargo run --release -p bench -- <host:port> <workload>`):

| Workload | Java emulator | datastore-oxide |
|---|---|---|
| Non-transactional upserts | 8,568 ops/s | 21,154 ops/s |
| Lookups | 20,101 ops/s | 21,199 ops/s |
| Queries (`n = ?`, ~10 results) | 1,270 ops/s | 2,090 ops/s |
| Transactions (begin, lookup, commit) | 4,514 ops/s | 6,670 ops/s |
| Start to healthy (container) | 1.35 s | 0.29 s |

Natively the binary answers its health check about 20 ms after launch
(`tools/bench/startup.sh`). Lookups are bound by the container's port forwarding.

### Large datasets

Queries scan every entity of the kind, so their cost grows linearly with the data; lookups and
commits don't. Measured natively (Intel Core Ultra 7 155H, `--testing`, superset on), 10 s per
workload, `n = ?` queries matching about 10 entities at every size
(`cargo run --release -p bench -- <host:port> <workload> <concurrency> 10 <entities>`):

| Entities | Query p50, 1 client | Queries/s, 1 client | Queries/s, 16 clients (p50) | Lookups/s, 16 clients | Commits/s, 16 clients | Memory (RSS) |
|---|---|---|---|---|---|---|
| 1,000 | 0.24 ms | 3,639 | 4,866 (3.2 ms) | 40,927 | 38,774 | 41 MB |
| 10,000 | 1.9 ms | 491 | 526 (30 ms) | 42,550 | 39,591 | 61 MB |
| 100,000 | 35 ms | 28 | 28 (532 ms) | 40,016 | 37,897 | 257 MB |
| 1,000,000 | 446 ms | 2 | 3 (5.8 s) | 38,496 | 36,412 | 2.4 GB |

Memory is from v0.3.3, which stores entities about 2.3× more compactly than earlier versions;
the throughput columns predate v0.3.1 (back-to-back runs show v0.3.3 about 20% faster on
queries and on par for lookups and commits).

Queries hold the store's single lock while they scan: more clients don't add query throughput,
and other requests wait. With 4 clients querying 100,000 entities, 16 concurrent lookup clients
drop from 40,922 to 151 lookups/s (p99 0.7 ms → 286 ms). Datasets up to about 10,000 entities
per kind behave like small ones.

## Provenance

Emulator behavior was derived from black-box probes of Google's Cloud Datastore emulator (recorded
as the goldens under `tests/`) and from the Apache-2.0 sources of appengine-java-standard (see
`NOTICE`). Contributions must not use decompiled or disassembled emulator code.

## License

Apache-2.0 (`LICENSE`, `NOTICE`). `datastore-oxide license` also prints the licenses of the
compiled-in third-party crates (`tools/gen-licenses.py` regenerates that list).

## Trademarks

datastore-oxide is an independent project. It is not affiliated with, endorsed by, or sponsored by
Google LLC. Google, Google Cloud, Cloud Datastore, Firestore and App Engine are trademarks of
Google LLC; they are used here only to describe what this software is compatible with.

## Layout

| Path | Contents |
|---|---|
| `crates/oxide-model` | Keys, values, entities |
| `crates/oxide-engine` | Storage, versions, id allocation, write cost |
| `crates/oxide-api` | v1 validation/conversion, gRPC services, REST gateway, App Engine v3 API server (`appengine/`) |
| `crates/oxide-json` | Java-compatible JSON printing |
| `crates/oxide-proto` | Generated types/services: googleapis Datastore v1, Admin v1, longrunning; App Engine v3 datastore; the emulator config service and CallHandler envelope |
| `crates/oxide-server` | The `datastore-oxide` binary |
| `proto/googleapis` | Vendored googleapis protos (`tools/vendor-googleapis.sh`, pinned in `proto/googleapis.commit`) |
| `proto/appengine` | Vendored App Engine v3 datastore protos from appengine-java-standard (`tools/vendor-appengine-protos.sh`, pinned in `proto/appengine.commit`) |
| `proto/emulator` | The emulator config service and the App Engine CallHandler envelope, written for this project |
| `tests/golden/cli` | Java CLI behavior recorded by `tools/record-cli-goldens.sh` |
| `tests/conformance` | REST scenarios and the Java emulator's recorded responses; `projects/` holds cases that need their own emulator and project directory (index files, flags, restarts) |
| `tests/clients` | Smoke tests with official client libraries |
| `tests/differential` | Scripts that compare a running Java emulator with datastore-oxide |
| `tools/conformance` | Scenario runner (REST and App Engine v3 `V3` steps), `record-conformance`, `record-projects` |
| `tools/bench` | Load generator and startup timer |
| `tools/scenario-gen` | Generators for the query, transaction, project and v3 scenarios |
| `docs/architecture.md` | Crates, request path, engine internals (storage, consistency, transactions), persistence |
| `docs/reference-behavior.md` | Observed Java emulator behavior |
| `docs/porting-notes` | Observed Java emulator behavior and the App Engine SDK code it shares (queries, transactions, indexes, export format, Firestore mode, App Engine v3) |
| `docs/superset-backlog.md` | Production features missing from the Java emulator: what is implemented (off with `--no_superset`) and what is left |

## Testing

```sh
cargo test                     # unit tests, CLI goldens, REST conformance (no Java needed)
SCENARIO=ids cargo test -p oxide-server --test conformance   # one scenario
CASE=index_require cargo test -p oxide-server --test projects # one project case

# Differential checks against the real Java emulator (needs podman):
PODMAN_ARGS="--name jp1 -p 18100:8081" tools/javaemu.sh start --host=0.0.0.0 --port=8081 --testing &
tools/jreq POST /v1/projects/p:runQuery '{"query":{}}'   # ad-hoc request (BASE=host:port)
python3 tools/conformance-view.py txn_basic --errors   # scenario steps next to Java's responses
tools/record-cli-goldens.sh    # re-record CLI goldens
cargo run -p conformance --bin record-conformance -- http://localhost:8081 [scenario...]
cargo run -p conformance --bin record-conformance -- --firestore http://localhost:8082 [scenario...]   # Java with --firestore_in_datastore_mode
cargo run -p conformance --bin record-projects [case...]   # starts its own Java containers
tools/record-v3.sh [scenario...]   # App Engine v3 scenarios, both modes (Java on 18100 and 18101)
DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_crud.py
DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_query.py
DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_txn.py
tests/differential/control-endpoints.sh <java-host:port> <oxide-host:port>
tests/differential/export-interop.sh <java-host:port> <oxide-host:port> <shared-dir> <java-path-of-shared-dir>
```
