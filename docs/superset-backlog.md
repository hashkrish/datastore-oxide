# Production features missing from the Java emulator

datastore-oxide matches the Java emulator, and on top of that serves production Datastore features
the emulator lacks: a **superset**, behavior that never changes a response the Java emulator gives
successfully. The superset is on by default; `start --no_superset` turns it off, leaving exactly
Java's behavior (the conformance tests run that way). This file lists the gaps: first how the
superset fills them, then Java's behavior for each (what `--no_superset` keeps), then gaps found
since that are still open.

Each entry says how Java behaves today, so the superset can be checked against it.

## Implemented (off with `--no_superset`)

With `--no_superset`, these behave exactly as Java does (the table rows below). Tests:
`crates/oxide-server/tests/superset.rs`.

| Feature | Superset behavior |
|---|---|
| Aggregation queries: `COUNT` (with `up_to`), `SUM`, `AVG`; GQL `AGGREGATE agg [AS a], … OVER (SELECT …)` and `SELECT agg [AS a], … FROM …` with `COUNT(*)`, `COUNT_UP_TO(n)`, `SUM(p)`, `AVG(p)` | `RunAggregationQuery` over gRPC and REST (`crates/oxide-api/src/aggregation.rs`). Aggregates every row of the nested query after cursors, offset and limit (no 300-row batches). SUM is an integer until a double appears or it overflows, 0 over no values; AVG is a double, null over no values; non-numeric, array and unindexed values are skipped; `a.b` reads an embedded entity. Up to 5 aggregations, unique aliases, unnamed ones get `property_1`, `property_2`, …; `up_to` must be positive; the nested query may only project `__key__` and has no `DISTINCT ON`. Same transaction rule as RunQuery (ancestor queries only, in Datastore mode). GQL requests echo the parsed query. `RunQuery` still rejects GQL aggregations, as production does. Error messages and edge cases were not checked against production. |
| Production commit limits | More than 500 mutations in a commit → 400 `cannot write more than 500 entities in a single call`; an entity whose serialized v1 proto exceeds 1,048,572 bytes (1 MiB − 4) → 400 `entity is too big`. |
| Production request fields over REST | REST requests parse against the full published schema, so `request_options` (request tags) is accepted and ignored, and `explain_options`, `property_transforms` and `find_nearest` reach the gRPC service. |
| Production filters: `OR`, `IN` with several values, `NOT_IN`, `!=`, inequalities on several properties | Structured and GQL queries (and the nested query of an aggregation). Filters are expanded into disjunctive normal form (`crates/oxide-api/src/query.rs`); each branch is checked as a query of its own and a row matches when it matches any branch (`crates/oxide-engine/src/query.rs`). `IN` matches any value; `!=` and `NOT_IN` match a non-null value outside the excluded ones (an array matches through any element) and exclude null and missing values. Inequality properties not ordered explicitly are ordered after the explicit orders, by name, in the direction of the last explicit order, and must exist. Limits: 30 disjunctions, 10 `NOT_IN` values, one `!=`/`NOT_IN` per query, `NOT_IN` not with `OR` or `IN`, the same ancestor in every disjunction. |
| Property transforms and property masks | `Mutation.property_transforms` (`increment`, `maximum`, `minimum`, `set_to_server_value: REQUEST_TIME`, `append_missing_elements`, `remove_all_from_array`) apply in order after the write, with production's numeric rules (saturating integer increments, doubles win, NaN, equivalent numbers); `MutationResult.transform_results` reports each result (null for array transforms). `Mutation.property_mask` merges the written entity into the stored one (masked paths written or removed, others kept; Java ignores the mask). `LookupRequest.property_mask` and `RunQueryRequest.property_mask` return only the referenced properties (Java returns all). Paths use dots for embedded entities, with backslash escapes. Transactions see their own earlier writes (`crates/oxide-engine/src/transform.rs`). |
| Point-in-time reads | `ReadOptions.read_time` on Lookup, RunQuery and RunAggregationQuery, and read-only transactions with `read_time`, read the committed state as of that time; responses echo `read_time`. The engine keeps an hour of committed history (production's default retention; only with the superset on), keyed by the wall-clock time of each commit; an older `read_time` → 400 `The read_time is older than the one-hour version retention period.` |
| Query explain | `explain_options` on RunQuery and RunAggregationQuery (`crates/oxide-api/src/explain.rs`). The plan summary lists one index per disjunction, as production prints them: `{"query_scope": "Kind", "properties": "(c ASC, n DESC, __key__ ASC)"}` (equality properties by name, then the query's orders). Without `analyze` the query is not run and there is no batch; with it, `execution_stats` reports results returned, duration, billed reads (one per result, at least one; aggregations one per 1000 entries) and `debug_stats` (`documents_scanned`, `index_entries_scanned`). The index names describe the emulator's plan, not production's planner. |
| Vector values and vector search | A vector value is an array of 1–2048 doubles with `meaning: 31` (`exclude_from_indexes` allowed, components never indexed); Java rejects it. `Query.find_nearest` (`crates/oxide-api/src/vector.rs`) runs after the rest of the query (filters, cursors, offset, limit) and ranks entities whose property holds a vector of the query vector's dimension by `EUCLIDEAN`, `COSINE` (1 − cosine similarity) or `DOT_PRODUCT` (larger is nearer); `distance_threshold` keeps distances ≤ it (≥ for dot product), `limit` is 1–100, and `distance_result_property` adds the distance to each result. Full or keys-only results; no other projection or `DISTINCT ON`. |
| Admin API indexes and operations | `CreateIndex`, `GetIndex`, `ListIndexes`, `DeleteIndex` manage composite indexes (`crates/oxide-api/src/admin.rs`): listed indexes are those in the index configuration files plus those created through the API (state `READY`, ids derived from the definition); created ones count as defined for `--require_indexes`; only API-created ones can be deleted. Admin operations get names (`projects/{p}/operations/{id}`) and are kept for the `google.longrunning.Operations` service (`crates/oxide-api/src/operations.rs`) over gRPC and REST (`GET /v1/projects/{p}/operations[/{id}]`, `DELETE …`, `POST …:cancel`); they are finished when returned, so cancelling has no effect. |
| Export and import like production | `output_url` names the overall metadata file (`{prefix}/{name}/{name}.overall_export_metadata`), which `ImportEntities` takes as `input_url`; `labels` are kept in the operation metadata; `gs://bucket/path` URLs are stored under `DATASTORE_OXIDE_GCS_ROOT` (default `./gcs`), and a `gs://` prefix may be exported to again (each export gets its own folder). There is no Cloud Storage client. |
| App Engine v3 extras | `Touch` (checks the keys), `DeleteCursor` (forgets the live query), `CreateIndex` / `UpdateIndex` / `DeleteIndex` (composite indexes shared with the Admin API; `CreateIndex` returns an `Integer64Proto` id). Filters `IN` with several values, `NOT_EQUAL` (9) and `NOT_IN` (10) as in the v1 superset, and inequalities on several properties. Full-text `search_query`: rows whose string values (including arrays and embedded entities) contain every word, case-insensitively. `allow_deferred`: keys after 1 MiB of response are returned in `deferred` with `in_order: false`. `read_time_epoch_micros` / `read_time_us` read as of that time and are echoed. BAD_REQUEST with a message instead of a bare UNKNOWN for an empty or invalid app id, negative `count`/`limit`, incomplete parent keys, invalid embedded entities, and `Next` on an unknown project (`crates/oxide-api/src/appengine/`). |
| Data viewer | `/_ah/admin/datastore` (`crates/oxide-server/src/viewer.rs`): HTML pages listing projects, their databases, namespaces and kinds with entity counts, and a kind's entities 50 per page with a delete button each. |

## Unknown to Java's protos (fields its descriptors do not define)

REST requests are parsed against Java's descriptors, so these fields fail with
`Payload isn't valid for request.`. gRPC requests carrying them parse, but the fields are ignored.

| Feature | Production API surface |
|---|---|
| Property transforms (`increment`, `maximum`, `minimum`, `set_to_server_value`, `append_missing_elements`, `remove_all_from_array`) (**implemented**) | `Mutation.property_transforms`, `MutationResult.transform_results` |
| Query explain (plan summary, execution stats) (**implemented**) | `RunQueryRequest.explain_options`, `RunAggregationQueryRequest.explain_options`, `*Response.explain_metrics` |
| Vector search (**implemented**) | `Query.find_nearest` |
| Request options (request tags) (**implemented**: accepted, ignored) | `request_options` on every RPC |

## Defined in Java's protos but not implemented

| Feature | Java emulator behavior |
|---|---|
| Aggregation queries: `COUNT` (with `up_to`), `SUM`, `AVG` (**implemented**; Java's behavior below is `--no_superset`) | `RunAggregationQuery` → 501 `Method google.datastore.v1.Datastore/RunAggregationQuery is unimplemented` (Datastore and Firestore-in-Datastore mode) |
| GQL aggregations (`AGGREGATE COUNT(*) OVER (...)`, `SELECT COUNT(*)`) (**implemented**, in `runAggregationQuery`) | `runQuery` → bare 500 `UNKNOWN` |
| Admin index CRUD (`ListIndexes`, `GetIndex`, `CreateIndex`, `DeleteIndex`) (**implemented**) | 501 UNIMPLEMENTED |
| Long-running operations (`GetOperation`, `ListOperations`, `CancelOperation`, `DeleteOperation`) (**implemented**) | No Operations service: REST 404, gRPC UNIMPLEMENTED; export/import return finished operations |
| Cloud Storage export/import (`gs://bucket/...`) (**implemented** as a local directory) | Treated as a local relative path |
| `ExportEntitiesResponse.output_url` pointing at the overall metadata file (**implemented**) | The bare prefix |
| Export `labels` (**implemented**) | Ignored |
| `property_mask` on Commit, Lookup and RunQuery (**implemented**) | Accepted and ignored: full entities are written and returned |
| `OR` composite filters (Datastore mode) (**implemented**) | 400 `unsupported composite property operator` |
| `IN` with several values, `NOT_IN` (Datastore mode) (**implemented**) | 400 `Filter has N properties, expected 1` |
| `!=` (Datastore mode) (**implemented**) | bare 500 |
| Point-in-time reads (`ReadOptions.read_time`, read-only transactions with `read_time`) (**implemented**) | accepted and ignored: the current data is returned |

Java's Firestore-in-Datastore mode rejects these filters too (and multiple inequality properties); the superset implements them in both modes.

## Production limits the emulator does not enforce

| Limit | Java emulator behavior |
|---|---|
| 500 mutations per commit (**implemented**) | accepted |
| 1 MiB entity size (**implemented**) | accepted |

## App Engine v3 API server

Production App Engine's datastore_v3 service does these; Java's emulator does not
(see `docs/porting-notes/appengine-v3.md`).

| Feature | Java emulator behavior |
|---|---|
| `Touch`, `DeleteCursor`, `CreateIndex`, `UpdateIndex`, `DeleteIndex` (**implemented**) | `application_error{1, "Cannot find definition for method: X"}` |
| `NOT_EQUAL`, `NOT_IN` filters (**implemented**) | bare gRPC UNKNOWN |
| `IN` with several values (**implemented**) | `Filter has N properties, expected 1` |
| Full-text `search_query` (**implemented**) | `full-text search unsupported` |
| `allow_deferred` (deferred keys on large Gets) (**implemented**) | accepted, never defers |
| `read_time_epoch_micros` / `read_time_us` reads (**implemented**) | accepted and ignored |
| Proper errors instead of uncaught exceptions (empty app id, invalid app id, negative `count`/`limit`, incomplete parent keys, `Next` on an unknown project) (**implemented**) | bare gRPC UNKNOWN |

## Development tooling

| Feature | Java emulator behavior |
|---|---|
| Data viewer at `/_ah/admin/datastore` (the old App Engine dev server's admin console) (**implemented**) | 404 `Not Found` |

## Found later, still open

| Feature | Java emulator and datastore-oxide behavior |
|---|---|
| Response timestamps: `EntityResult.create_time` / `update_time`, `MutationResult.create_time` / `update_time`, `CommitResponse.commit_time`, and `read_time` on every Lookup, RunQuery and RunAggregationQuery response | Not set (oxide sets `read_time` only for point-in-time reads). Needs a wall-clock time per stored version and per entity creation. |
