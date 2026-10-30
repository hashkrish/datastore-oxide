# Architecture

How datastore-oxide is put together: the crates, the path a request takes, and how the engine
models storage, consistency and transactions. For *what* the emulator does see
`docs/reference-behavior.md` and `docs/porting-notes/`; for the production features on top see
`docs/superset-backlog.md`.

## Crates

```text
oxide-server ──► oxide-api ──► oxide-engine ──► oxide-model
     │              │  │
     │              │  └──► oxide-json
     └──────────────┴─────► oxide-proto
```

| Crate | Role | Knows about wire formats? |
|---|---|---|
| `oxide-model` | Keys, values, entities. Values compare doubles by bit pattern, as Java compares stored protos. Also `java_string_hash`. | No |
| `oxide-engine` | The datastore: storage, entity groups, eventual consistency, transactions, queries, cursors, id allocation, write cost, composite indexes and index files, point-in-time history. | No (except cursors, which are App Engine `CompiledCursor` bytes) |
| `oxide-proto` | Generated prost/tonic code for the vendored protos, plus the serialized `FILE_DESCRIPTOR_SET` (used by reflection and the REST gateway). | Yes |
| `oxide-json` | Prints JSON the way protobuf-java's `JsonFormat` does, so REST responses match Java byte for byte. | Yes |
| `oxide-api` | Everything between a proto request and the engine: validation with Java's messages, proto ↔ model conversion, the gRPC services, the REST gateway, App Engine v3, export/import, on-disk persistence. | Yes |
| `oxide-server` | The `datastore-oxide` binary: CLI parsing (`start`, `create`, `license`), the network front end, control endpoints, periodic saving, the data viewer. | Yes |

The engine is deliberately wire-agnostic: v1 gRPC, REST and App Engine v3 all convert into the
same `Key`/`Entity`/`Mutation`/`Query` types and call the same `Engine` methods. That is what
lets data written through one API be read through another.

## Request path

```text
             TCP (one port, HTTP/1.1 + h2c)
                        │
                 server::dispatch
        ┌───────────────┼──────────────────────┐
 content-type:     control path?           anything else
 application/grpc  (/, /reset, /persist,         │
        │           /shutdown, viewer)      RestGateway::handle
        │               │                   (JSON or x-protobuf →
        │           handled in              gRPC frame, in-process)
        │           oxide-server                 │
        ▼                                        ▼
   tonic Routes ◄────────────────────────────────┘
   ├─ google.datastore.v1.Datastore      (service.rs)
   ├─ google.datastore.admin.v1          (admin.rs, export.rs)
   ├─ google.longrunning.Operations      (operations.rs, superset only)
   ├─ emulator config service            (config.rs)
   ├─ App Engine CallHandler (v3)        (appengine/)
   └─ gRPC reflection (v1, v1alpha)
        │
        ▼
   convert.rs / query.rs / gql.rs   validation + proto → model
        │
        ▼
   Arc<Engine>                      oxide-engine
```

- **One port, two protocols.** `server.rs` uses hyper's auto builder, so HTTP/1.1 and HTTP/2
  cleartext share a listener, as in Java. Requests are dispatched on `content-type`.
- **REST is transcoded, not reimplemented.** `rest.rs` builds its routes from the
  `google.api.http` rules in the descriptors. It parses the body against the schema Java accepts
  (`java_schema` removes fields Java rejects unless the superset is on), frames the request as a
  gRPC call, sends it to the same tonic `Routes` in-process, and prints the response with
  `oxide-json`. Errors and quirks therefore stay consistent between gRPC and REST.
- **Control endpoints** (`/`, `/reset`, `/persist`, `/shutdown`, `/_ah/admin/quit`) and CORS
  headers live in `server.rs`, with Java's exact status codes and bodies. With the superset on,
  `/_ah/admin/datastore` serves the read-only data viewer (`viewer.rs`).
- **Validation lives in `oxide-api`**, not the engine. Each check reproduces the Java v1 layer's
  error message, and the conformance goldens record those messages
  (`tests/conformance/java/validation_*.golden` and others).

## The engine

`Engine` (`crates/oxide-engine/src/lib.rs`) is shared as an `Arc<Engine>` by every service.

### State and locking

All data lives in one `Mutex<State>`:

```text
State
├─ id_counter, scattered_counter      one counter for every project/kind/namespace (as in Java)
└─ projects: HashMap<project_id, Project>
     ├─ entities: BTreeMap<Key, Stored>   applied state, all databases and namespaces
     ├─ groups:   HashMap<Key, Group>     per entity group: version + queue of unapplied Jobs
     ├─ pending:  groups with unapplied jobs, in arrival order
     ├─ txns:     HashMap<u64, Txn>       open transactions
     ├─ kinds:    KindTable               Java's HashMap iteration order (decides export order)
     └─ commit_times, history, times      superset: point-in-time reads, response timestamps
```

Each engine operation runs inside `with_state`, which holds the lock for the whole operation
and also supplies the job policy's apply decision. There is no finer-grained locking: every
operation is serialized. That keeps the semantics simple and deterministic (a seeded run makes
the same choices Java does), and operations are in-memory and short. A few things sit outside
the lock: runtime `Config` (its own mutex), the transaction handle counter (an atomic that
`reset` does not clear, as in Java), and the `generation` counter that tells layers holding
per-dataset state (v3 query handles) that the data was replaced.

`Stored` holds the entity, its version, a creation sequence number (Java keeps insertion-ordered
maps, and some tie-breaks depend on it) and a `Form`, which records the hidden `__scatter__`
property and export side effects.

### Entity groups and eventual consistency

The engine copies App Engine's High Replication model as the Java emulator runs it:

1. A commit does not write `entities` directly. It bumps the group's version and appends a
   **Job** (puts and deletes with one timestamp) to that group's queue (`Project::add_job`).
2. Whether a job is applied is decided by the **job policy**. A probabilistic policy draws
   `nextInt(10000)` from a bit-exact port of `java.util.Random` (`random.rs`), seeded like
   Java, and compares the draw with the unapplied percentage. `ForceStrongConsistency` always
   applies. A new job gets its first chance right after it is queued (`maybe_roll_forward`),
   and a group's jobs always apply in order.
3. **Eventual reads** (non-ancestor queries, eventual lookups) see applied state only, and each
   read gives every pending group one more chance to apply (`groom`).
4. **Strong reads** (strong lookups, ancestor queries) apply the whole group first
   (`roll_forward`).
5. On shutdown with `--store_on_disk`, `roll_forward_all` applies everything before the final
   save, because unapplied jobs are never persisted.

The group of a key is its root entity, regardless of namespace or database. With
`--enable_entity_groups=false`, reads and deletes use the full key path instead, while puts
still use the root, as in Java (`read_group`/`write_group`).

### Transactions

A `Txn` records the groups it has *tracked* together with their versions at first touch, plus
buffered writes, read-only/XG flags, and App Engine transactional tasks:

- Touching a new group checks the group limit (25) and that every tracked group is still at its
  recorded version. Contention fails that read but leaves the transaction usable.
- When another writer bumps a tracked group, `bump_group` freezes a **snapshot** of that group's
  applied entities into the transaction, so later reads in the transaction see the data as of
  that point.
- A commit gets one commit timestamp and produces one job per written group. A failed commit
  closes the transaction.
- Every query run inside a transaction also tracks the pseudo-group `__none__:1`.
- Transactions expire after `TXN_LIFETIME` (300 s).

`docs/porting-notes/txn-semantics.md` and `firestore-mode.md` §10 are the specs this follows.
Firestore-in-Datastore mode (`with_firestore_mode`) starts strongly consistent and changes the
rules described in `firestore-mode.md`.

### Queries

`engine/src/query.rs` ports `LocalDatastoreService.runQuery`: normalize → validate → candidates
(kind, ancestor, partition) → filter (a property that is filtered or ordered on must have an
indexed value) → expand projections → stable sort → distinct → cursors → offset/limit, in batches
of at most 300. Supporting modules:

| Module | What it does |
|---|---|
| `index.rs` | Index values and the ordering between value types |
| `cursor.rs` | Cursors encoded exactly as Java encodes them (`CompiledCursor` / `IndexPostfix`) |
| `pseudo.rs` | `__kind__`, `__property__`, `__namespace__`, `__entity_group__`, plus Firestore mode's `__collection__` / `__document__` |
| `composite.rs` | Which composite index a query needs (`CompositeIndexManager` port) |
| `index_files.rs` | `index.yaml` / `datastore-indexes*.xml`, query history, auto-generation, `--require_indexes` |
| `cost.rs` | `indexUpdates` write cost and `__scatter__` |
| `transform.rs` | Superset partial writes: property masks and transforms |

The API layer adds features that run on the engine's query results: GQL parsing (`gql.rs`),
aggregations (`aggregation.rs`), vector search (`vector.rs`) and explain (`explain.rs`). The last
three are superset only.

### Ids and versions

Sequential auto ids come from the shared counter. With the scattered policy, ids come from a
second counter mapped by bit reversal into `[2^52, 2^52 + 2^51)` (`to_scattered_id`). Commit
timestamps are per project and increase monotonically. `__entity_group__` versions are offset by
the startup time so they keep increasing across restarts.

## Persistence

`--store_on_disk` (the default unless `--testing` is given) keeps the data in memory and saves an
`Engine::take_snapshot` with `persist.rs`:

- Format: 8-byte magic `OXDS\0\0\0\x01`, then a protobuf message with the id counters and, for
  each project, its v1 entities with their versions in creation order and its kinds in Java's
  iteration order.
- Writes go to a temp file, which is fsynced and renamed over the target, so a crash leaves the
  old file or the new one, never a partial one.
- Saves happen every 30 s if anything changed (`OXIDE_STORE_DELAY_MS` overrides the interval),
  on `POST /persist`, and on shutdown. Loading happens once at startup.
- Java's `local_db.bin` cannot be read. Data moves between the two through Admin export/import
  (`export.rs`: LevelDB logs of v3 `EntityProto`s, the same files production writes).

## Superset gating

The superset (production features the Java emulator lacks) is on by default and turned off with
`--no_superset`. Gating happens at construction time, not in scattered runtime checks:

- `DatastoreService::with_superset`, `appengine::Service::with_superset`, `RestGateway::new(superset)`
- the Operations service and the operation store are only registered when the superset is on
- `Engine::with_point_in_time` (history for `read_time` reads)
- the viewer routes in `handle_control`

The rule (`docs/superset-backlog.md`): the superset must never change a successful response from
the Java emulator. It may only turn Java's errors or ignored fields into production behavior.
The golden tests run with `--no_superset`.

## App Engine v3

`appengine/` serves `apphosting.tools.devappserver2.CallHandler`: v3 datastore calls wrapped in
a generic envelope, running against the same `Engine`. `wire.rs` parses requests as leniently as
Java's proto1 runtime does, `appid.rs` maps app ids (`s~p`, `e~p`, `dev~p`) to project ids,
`entity.rs`/`v3.rs` convert between v3 `EntityProto` and the model, `query.rs` holds query
snapshots for `Next`, and `actions.rs` delivers transactional tasks to the caller's callback
host on commit. See `docs/porting-notes/appengine-v3.md`.

## How correctness is checked

Each layer is checked against the Java emulator, using recorded output rather than reading Java
internals (see the README's Provenance section):

| Layer | Evidence |
|---|---|
| CLI | `tests/golden/cli` (recorded by `tools/record-cli-goldens.sh`) |
| REST/gRPC behavior | `tests/conformance/scenarios` + recorded Java responses in `tests/conformance/java*`, run by `tools/conformance` |
| Startup flags, index files, restarts | `tests/conformance/projects` |
| Live comparison | `tests/differential` against a running Java emulator (`tools/javaemu.sh`) |
| Client libraries | `tests/clients` |
| Performance | `tools/bench` |

Scenario generators (`tools/scenario-gen`) produce the query, transaction, project and v3
scenarios.
