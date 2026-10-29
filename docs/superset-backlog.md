# Production features missing from the Java emulator

datastore-oxide matches the Java emulator, and on top of that serves production Datastore features
the emulator lacks: a **superset**, behavior that never changes a response the Java emulator gives
successfully. The superset is on by default; `start --no_superset` turns it off, leaving exactly
Java's behavior (the conformance tests run that way). This file lists the gaps: those implemented
so far, then those still to do.

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

## Unknown to Java's protos (fields its descriptors do not define)

REST requests are parsed against Java's descriptors, so these fields fail with
`Payload isn't valid for request.`. gRPC requests carrying them parse, but the fields are ignored.

| Feature | Production API surface |
|---|---|
| Property transforms (`increment`, `maximum`, `minimum`, `set_to_server_value`, `append_missing_elements`, `remove_all_from_array`) (**implemented**) | `Mutation.property_transforms`, `MutationResult.transform_results` |
| Query explain (plan summary, execution stats) | `RunQueryRequest.explain_options`, `RunAggregationQueryRequest.explain_options`, `*Response.explain_metrics` |
| Vector search | `Query.find_nearest` |
| Request options (request tags) (**implemented**: accepted, ignored) | `request_options` on every RPC |

## Defined in Java's protos but not implemented

| Feature | Java emulator behavior |
|---|---|
| Aggregation queries: `COUNT` (with `up_to`), `SUM`, `AVG` (**implemented**; Java's behavior below is `--no_superset`) | `RunAggregationQuery` → 501 `Method google.datastore.v1.Datastore/RunAggregationQuery is unimplemented` (Datastore and Firestore-in-Datastore mode) |
| GQL aggregations (`AGGREGATE COUNT(*) OVER (...)`, `SELECT COUNT(*)`) (**implemented**, in `runAggregationQuery`) | `runQuery` → bare 500 `UNKNOWN` |
| Admin index CRUD (`ListIndexes`, `GetIndex`, `CreateIndex`, `DeleteIndex`) | 501 UNIMPLEMENTED |
| Long-running operations (`GetOperation`, `ListOperations`, `CancelOperation`, `DeleteOperation`) | No Operations service: REST 404, gRPC UNIMPLEMENTED; export/import return finished operations |
| Cloud Storage export/import (`gs://bucket/...`) | Treated as a local relative path |
| `ExportEntitiesResponse.output_url` pointing at the overall metadata file | The bare prefix |
| Export `labels` | Ignored |
| `property_mask` on Commit, Lookup and RunQuery (**implemented**) | Accepted and ignored: full entities are written and returned |
| `OR` composite filters (Datastore mode) (**implemented**) | 400 `unsupported composite property operator` |
| `IN` with several values, `NOT_IN` (Datastore mode) (**implemented**) | 400 `Filter has N properties, expected 1` |
| `!=` (Datastore mode) (**implemented**) | bare 500 |
| Point-in-time reads (`ReadOptions.read_time`, read-only transactions with `read_time`) | accepted and ignored: the current data is returned |

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
| `Touch`, `DeleteCursor`, `CreateIndex`, `UpdateIndex`, `DeleteIndex` | `application_error{1, "Cannot find definition for method: X"}` |
| `NOT_EQUAL`, `NOT_IN` filters | bare gRPC UNKNOWN |
| `IN` with several values | `Filter has N properties, expected 1` |
| Full-text `search_query` | `full-text search unsupported` |
| `allow_deferred` (deferred keys on large Gets) | accepted, never defers |
| `read_time_epoch_micros` / `read_time_us` reads | accepted and ignored |
| Proper errors instead of uncaught exceptions (empty app id, invalid app id, negative `count`/`limit`, incomplete parent keys, `Next` on an unknown project) | bare gRPC UNKNOWN |

## Development tooling

| Feature | Java emulator behavior |
|---|---|
| Data viewer at `/_ah/admin/datastore` (the old App Engine dev server's admin console) | 404 `Not Found` |
