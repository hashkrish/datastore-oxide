# Java Cloud Datastore emulator: `--firestore_in_datastore_mode` and `--enable_entity_groups`

What changes compared with the default Datastore mode (entity groups on). Everything
that is not listed here behaves exactly as in Datastore mode (`docs/reference-behavior.md`,
`docs/porting-notes/txn-semantics.md`, `docs/porting-notes/query-semantics.md`).

Evidence:
- Black-box probes against Java emulators in containers:
  - 18120: FS mode, `--enable_entity_groups=false`
  - 18121: DS mode, `--enable_entity_groups=false`
  - 18122: FS mode
  - 18123: FS mode, not `--testing`, `--consistency=0.5`
- The recorded goldens: `tests/conformance/java-firestore/*.golden` and `tests/conformance/projects/entity_groups_off*`.

Markers:
- **[probe]**: observed black-box.
- **[golden]**: visible in the recorded goldens.
- **[unverified]**: inferred but not checked.

---

## 0. Summary

Firestore mode differs from Datastore mode in exactly three ways:

| difference | effect in FS mode |
|---|---|
| entity-group limit | no 25-entity-group limit per transaction |
| queries in transactions | non-ancestor queries are allowed in transactions, and they track the group of every matching entity |
| strong consistency | always starts strongly consistent; `__entity_group__` is not a pseudo-kind; `__collection__`/`__document__` are; GET/PATCH `/v1/config` always report `forceStrongConsistency: true`; logs "Type: VNext" |

Nothing else differs: validation, GQL, allocateIds/reserveIds, export/import and the config service behave identically in
both modes (identical goldens). The transaction model is the same optimistic, per-entity-group model as Datastore mode.
Entity groups stay on by default in **both** modes, because `--enable_entity_groups` defaults to `true` whatever the mode.

`--enable_entity_groups=false` makes lookups, deletes, ancestor-query tracking, FS query tracking and `__entity_group__`
use the full key path as the group. **Puts do not**: they group by the root element unconditionally. The result is a
mixed, partly broken model (§7).

---

## 1. Configuration

### 1.1 CLI
- `--firestore_in_datastore_mode` is a flag. Help text: `Runs the emulator in Cloud Firestore in Datastore Mode.
  --consistency flag is ignored.`
- `--enable_entity_groups=true|false` takes a value. Help text (typo included): `If this option is set, the Datastore
  emulator with simulate entity groups for concurrency.` **The default is `true` in both modes.**
- At startup the emulator logs `Running %s emulator.` **[probe]**:
  - `INFO: Running CLOUD_FIRESTORE_IN_DATASTORE_MODE emulator.`
  - `INFO: Running CLOUD_DATASTORE emulator.`
- `--consistency` (default 0.9) is still parsed and its percentage `(1-c)*100` is stored. It is ignored at startup, but
  **it becomes live after a PATCH** (§3.2). `--testing` sets c=1.0, so the stored percentage is 0.
- The log line `Local Datastore initialized: \n\tType: %s\n\tStorage: %s` shows `VNext` in FS mode and
  `High Replication` otherwise **[probe]**.

### 1.2 Forced strong consistency
- In FS mode the configuration's `forceStrongConsistency` can never become false: every config update re-forces it.
- It determines the startup job policy, which pseudo-kinds exist (`__entity_group__` in DS mode; `__collection__` and
  `__document__` in FS mode) and what the config service reports (`{forceStrongConsistency: true}` instead of
  `probabilityJobPolicy`).
- The pseudo-kinds are fixed at startup. A later PATCH never changes them in either mode.

---

## 2. Transaction model in Firestore mode (Q1)

### 2.1 Same model as Datastore mode
- The model is optimistic, with versions per **entity group**: root element by default, full path with
  `--enable_entity_groups=false` for everything except puts (§7). Read-write and unspecified transactions are optimistic;
  read-only ones are read-only. No locks are taken, nobody waits, there is no lock timeout, and none of this depends on
  the mode. v1 transactions may always span multiple groups.
- **Conflict rule (unchanged)**:
  - A txn records a group's version when it first tracks the group.
  - Whenever it tracks a **new** group, it checks every already-tracked group. Commit (if the txn is dirty) checks every
    tracked group. Any moved version → ABORTED (HTTP 409, status `ABORTED`), message
    `too much contention on these datastore entities. please try again.`
  - A non-dirty commit (no mutations, every read-only txn) never conflicts **[golden txn_isolation 21-23; probe N]**.
  - The group version is bumped by every write job on the group: non-transactional writes, and every group tracked by a
    committing dirty txn, including groups it only read.
- **What is tracked** (FS additions in bold):
  - lookups: the group of each key;
  - ancestor queries: the ancestor's group;
  - writes: the root group for puts, the key's group for deletes;
  - **non-ancestor queries in a txn: the group of every entity in the filtered and sorted result list, before limit, offset and
    cursors are applied**.
    - Evidence:
      - **[probe E]**: limit 1, a write to the second match aborts the commit.
      - **[probe O]**: an entity skipped by offset is tracked.
      - **[probe K]**: keys-only queries track too.
      - **[probe H]**: a query on child kind `C` conflicts with a write to its root `K:a`, so the granularity is the group.
    - Entities that start to match only later (phantoms) are not tracked **[golden txn_isolation 21-23]**.
  - Every query run with a transaction (FS: any query; DS: ancestor queries) also tracks the pseudo-group `__none__:1` (§2.4).
- **Read-time aborts**:
  - When tracking a new group finds that an earlier group moved, the read itself returns 409 ABORTED with the same message.
    That applies to a lookup, an ancestor query, an FS non-ancestor query, or the `__none__` lookup behind a query.
  - **The txn stays usable.** A later lookup of an already-tracked key still succeeds **[probe G]**. Only a failed **commit** leaves a
    "transaction closed" placeholder. This is the same in DS mode; `txn-semantics.md` §1.4 step 5 (from the App Engine SDK) does not apply to the emulator.

### 2.2 Reads inside a transaction
- Lookups and ancestor queries are unchanged:
  - they read the txn's snapshot if one exists, else live data;
  - after tracking, a forced roll-forward happens, which is a no-op under the forced-strong policy;
  - the txn's own buffered writes are never visible.
  - Reads that use the snapshot see the data as it was just before the first outside write to **any** group the
    txn tracks at its recorded version [probe snap].
- **FS non-ancestor queries in a txn read live data, never the snapshot.** Only ancestor queries read the snapshot, and
  there is no roll-forward on the queried groups.
  - So within one txn a query can return newer data than a lookup **[probe snap]**:
    1. Query `K v<5` → `a v=1`.
    2. Lookup `a` → `v=1`.
    3. An outside write sets `a=2`.
    4. The same query → `a v=2 version 4`, with no abort because `a` and `__none__` are already tracked.
    5. Lookup `a` → still `v=1 version 2` (snapshot).
    6. The non-dirty commit succeeds.
  - **[golden txn_isolation 17]**: the txn query sees the latest committed values (`K:a v=11` written in step 8).
- There is no snapshot at begin. `readTime` (on `ReadOptions` or `TransactionOptions.readOnly.readTime`) is ignored in both modes
  **[golden unsupported 14-16, txn_limits 8-9, 35]**.

### 2.3 Versions, commit timestamps, read-only, previousTransaction
- Versions, commit timestamps and `commitVersion` are unchanged:
  - one `lastCommitTimestamp` per app;
  - one timestamp per commit;
  - a no-op re-put returns the old version;
  - `commitVersion` = the highest mutation version **[golden txn_limits 15]**: 25 identical re-puts return `4`, the new entity
    returns `5`, and `commitVersion` is `5`.
- `previousTransaction`, read-only txns, "Cannot retry a read-only transaction.", "Cannot modify entities in a read-only
  transaction." and single-use/new transactions are all unchanged **[golden txn_failures 25-31, txn_limits 2-11, 23-34]**.
- Transaction lifetime and expiry are unchanged. Handles are `0x11` + LE fixed64 from one process-wide counter. It **starts at 1**,
  not 0 as `txn-semantics.md` §1.2 says, and survives `/reset`.

### 2.4 The `__none__:1` pseudo-group (applies to both modes; FS hits it far more often)
- A query reports `snapshotVersion` when it ran with a transaction.
  That covers an explicit `transaction`, `newTransaction`, or the ad-hoc READ_ONLY txn that is begun for a non-eventual,
  non-`__property__` query that has an ancestor or HAS_PARENT.
  - It behaves as if the query also read key `__none__:1` (same project/database) in that transaction:
    `snapshotVersion` is the **live** last commit timestamp at that moment (§1.5 of the txn notes) [probe none,
    golden txn_queries].
  - Without a transaction, `snapshotVersion` stays 0 and is omitted.
- The txn therefore **tracks the group `__none__:1`**:
  - Paths containing kind `__none__` do not count toward the 25-group limit.
  - It does run the "check all trackers" step the first time, which is one source of read-time aborts.
  - The commit of a dirty txn creates a job for every tracked group, **including `__none__:1`**, and bumps it.
  - So two read-write txns that both ran a query conflict: whichever commits writes second fails with ABORTED, even if their
    data is disjoint. **[probe none]** on 18101 (FS) and 18121 (DS, ancestor queries): T1 and T2 each ran an ancestor query on
    disjoint roots, T2 committed a write, then T1's write commit → 409.
  - In FS mode every query in a read-write txn has this effect.
  - The repo has no `__none__` handling; check whether the DS model reproduces this.

### 2.5 Per-mode summary
| | DS mode | FS mode |
|---|---|---|
| groups per txn | ≤25 (visible groups) → 400 `operating on too many entity groups in a single transaction.` | unlimited **[golden txn_limits 15, 17; txn_isolation 34, 35]** |
| non-ancestor query with txn | 400 `Only ancestor queries are allowed inside transactions.` | allowed; tracks all matching groups; reads live data; reports `snapshotVersion` |
| conflict granularity | entity group | entity group (same) |

---

## 3. Eventual consistency and job policy (Q2)

### 3.1 At startup
FS mode always starts strongly consistent: every write job applies immediately and no random numbers are drawn, so global queries, eventual lookups and eventual ancestor queries all see every write at once.
`--consistency` has no effect at this point.

### 3.2 PATCH `/v1/config` (and the gRPC UpdateConfig) — the real behaviour
Observed:
1. The request is merged into the stored config; in FS mode `forceStrongConsistency` stays true in the stored config.
   A `probabilityJobPolicy` in the request still overwrites the stored seed and percentage.
2. The active policy becomes forced-strong only if the **request** sets `jobPolicy.forceStrongConsistency: true`;
   otherwise it becomes probabilistic with the stored seed and percentage.
3. The response reports the stored config, so it always says `forceStrongConsistency: true` in FS mode.

So in FS mode **every PATCH whose body does not set `jobPolicy.forceStrongConsistency: true`** (including `{}` and id-policy-only
bodies) **installs a probabilistic policy** with the stored seed and percentage, while it keeps reporting forced strong consistency:
- **[golden consistency 1-16, consistency_seeded 1-7]**: after the PATCH to seed 1 / pct 100 (or seed 42 / pct 50), FS shows exactly the same
  stale global queries, eventual reads and RNG-driven visibility as DS. Only the PATCH and GET response bodies differ.
- **[probe cfg, 18123, `--consistency=0.5`]**:
  - At startup the reported policy is forced and 20 non-transactional writes are all visible.
  - After `PATCH {"idAllocationPolicy":{"policy":"SEQUENTIAL"}}` only 13 of 20 new writes are visible: the stored 50% from
    `--consistency` became live.
  - After `PATCH {"jobPolicy":{"forceStrongConsistency":true}}` all 20 are visible.
- Validation is unchanged:
  - pct > 100 → 400 `Unapplied job percentage must be <= 100.00 (received 150.000000)` **[golden consistency 32]**;
  - the config field has already been merged by then (both modes).
- The id policy updates normally **[golden consistency 27-30]**.

### 3.3 snapshotVersion
The rule is the same in both modes: present iff the query ran with a transaction (explicit, new, or the ad-hoc one for strong
ancestor/HAS_PARENT queries). Value = the `__none__:1` lookup = live `lastCommitTimestamp`.
- FS non-ancestor queries **without** a transaction report no `snapshotVersion` **[probe A]**.
- FS non-ancestor queries **in** a transaction do **[golden txn_limits 20, txn_isolation 17, 21]**.

---

## 4. Query differences (Q3)
- **Validation and messages** are identical in both modes:
  - multiple inequalities → `Only one inequality filter per query is supported. Encountered both v and w`;
  - OR → `unsupported composite property operator`;
  - multi-value IN → `Filter has 2 properties, expected 1`;
  - NOT_IN → `Unrecognized PropertyFilter.op: NOT_IN`;
  - `!=` → bare 500;
  - first sort property ≠ inequality property → same message;
  - GQL variants → same messages.
  - **[golden txn_isolation 36-43; query_* all identical]**. FS mode does **not** enable OR/IN/NOT_IN/!= or multiple inequalities.
- **Only mode-dependent validation**: `Only ancestor queries are allowed inside transactions.` (DS only).
- **Kindless queries**:
  - same results;
  - in an FS txn a kindless non-ancestor query is allowed and tracks every entity's group **[probe I]**;
  - `kind is required for non-__key__ filters` is unchanged.
- **`__entity_group__`**: not registered in FS mode, so it acts as an ordinary kind that nobody can write (reserved name):
  - query → 200, empty batch (`endCursor "CgA="`, `MORE_RESULTS_AFTER_LIMIT`) **[golden query_pseudo 10; probe D]**;
  - lookup `<root>/__entity_group__:1` → `missing`, `version` = `lastCommitTimestamp` (no `__version__`)
    **[golden query_pseudo 15, txn_failures 46]**;
  - DS returns 400 `queries not supported on __entity_group__` and `found {__version__}`.
- **New FS-only pseudo-kinds `__collection__` and `__document__`**. In DS mode they are ordinary empty kinds
  **[probe: 18121 → empty batch]**.
  - Both require a `__key__ HAS_PARENT` filter (errors: `HAS_PARENT requires a key value or null`,
    `duplicate ancestor or parent constraint`). The REST
    enum `"op":"HAS_PARENT"` is accepted by the Java emulator in both modes.
  - Otherwise → 400 `queries on __collection__ must specify a parent` / `queries on __document__ must specify a parent`.
    HAS_ANCESTOR is not enough **[probe C, 6]**.
  - Like `__kind__`, they only allow `__key__` filters with `<,<=,>,>=,=` and only an ascending `__key__` order.
  - `__collection__` with `HAS_PARENT <parent key or null>`:
    - scans all entities of the app (every namespace) that descend from the parent (or from the partition root when the parent
      is null);
    - truncates each one to `parentLen+1` elements;
    - maps the last element `Kind:id|name` to `<parent>/__collection__:"Kind"`;
    - dedupes by key and returns key-only entities in key order.
    - Example **[probe]**: null parent → `__collection__:K`; parent `K:a` → `K:a/__collection__:"D"`.
  - `__document__` with `HAS_PARENT <.../__collection__:"Kind">`:
    - The parent must be a pseudo path whose last kind is `__collection__`, else 400 `parent key must have kind
      __collection__`. It must have a name, else `__collection__ key must have a name`.
    - Returns one entity per distinct child `<collparent>/__collection__:"Kind"/__document__:<id|name>`, where that is
      `parentLen+1` of every entity of that kind under the parent.
    - Properties:
      - `document_key` (keyValue: the real key) is always present.
      - If the entity itself exists (not only descendants):
        - `document` (entityValue, v3 meaning ENTITY_PROTO, the entity without its key);
        - `created` and `updated`, both = the entity's stored version.
      - "Missing" documents, which only have descendants, carry only `document_key` **[probe: `K:x` exists only as parent of
        `K:x/C:7` → only document_key]**.
  - These queries report `snapshotVersion`: like ancestor queries, HAS_PARENT queries run in an ad-hoc read-only txn.
  - **[unverified]**: namespace/database scoping of the scan. Also
    projections, cursors and limits on these kinds.
- **Ancestor queries, `__kind__`, `__property__`, `__namespace__`, cursors, projections, batches, GQL**: identical **[golden]**.

---

## 5. Commit and write differences (Q4)
Only the group-count limit differs. Everything else is identical **[golden: commit_errors, commit_partial, index_updates, ids,
versions, validation_* all byte-identical]**:
- Mutation versions, `indexUpdates`, `commitVersion`, error messages, the 500-mutation limit, `__scatter__`, auto ids (both
  SEQUENTIAL and SCATTERED, which the id policy still switches) are unchanged.
- No 25-group limit in FS **[golden txn_limits 15; txn_isolation 35: 30 groups commit at one version]**.
- In FS, a dirty commit also bumps every group that the txn's queries tracked, and `__none__:1` (§2.4).

## 6. Lookup, allocateIds, export, import (Q5)
- **Lookup**:
  - no FS difference except `__entity_group__` keys (§4) and the absent 25-group limit for txn lookups
    **[golden txn_limits 17; txn_isolation 34]**;
  - eventual and strong lookups behave identically under the forced policy.
- **allocateIds/reserveIds, export/import, `/reset`, databases, REST routing**: identical goldens.

---

## 7. `--enable_entity_groups` (Q6)
- **Default `true` in both modes.** Effective defaults:
  - DS: per-root groups, 25-group limit;
  - FS: per-root groups, no limit.
- With `false`, the group is the full key path instead of the root element, except for puts. The mode does not
  change this:

| operation | group used with `=false` |
|---|---|
| lookup (txn tracking, snapshot, roll-forward, `__entity_group__` group) | full key path |
| delete (non-txn job + version bump; txn tracking) | full key path |
| ancestor query (txn tracking, roll-forward) | full **ancestor** path |
| FS non-ancestor txn query tracking | full path of each result |
| **put** (non-txn job + version bump; txn tracking; commit job) | **root element, always** |

Consequences (identical in DS and FS) **[probe eg, eg2 on 18120 FS and 18121 DS]**:
- Reads of different entities under one root no longer conflict with each other's *deletes*:
  - lookup child `a`, outside write to sibling `b`, commit `a` → 200 (on: 409).
- **A put of a child never conflicts with a reader of that child**, because the put bumps the root group and the reader tracked the
  child path:
  - lookup child, outside **put** of child, commit elsewhere → 200;
  - lookup child, outside **delete** of child → 409;
  - P3 is a lost update: read child v=3, outside put v=4, txn put v=99 → commits, final 99 (on: 409).
- The reverse direction conflicts:
  - an ancestor query on root `R:r` tracks `[R:r]`, and any put of a descendant bumps `[R:r]`, so the commit → 409;
  - lookup root, outside put of child, then lookup child in the txn → the new group triggers the check → 409 at read time.
- Puts in one txn of 26 children under one root → one root group → 200, even in DS mode. 26 child **lookups** → 26 groups →
  400 `operating on too many entity groups...` in DS **[probe cfg]**. FS has no limit.
- `__entity_group__` lookup (DS only, since FS does not register it):
  - the group is `[root, __entity_group__:1]`, which nothing ever writes, so `__version__` stays at the emulator start-time base
    (1793120490850000 before and after a root write) **[probe 18121]**;
  - the 3-element `child/__entity_group__:1` → bare 500 **[probe 18121; unverified whether DS with groups on gives the same]**.
- Eventual consistency (only after a probabilistic PATCH, or in DS):
  - put jobs live on the root group;
  - strong lookups and ancestor queries roll forward only the full-path group, so a strong lookup of a child can miss an
    unapplied put to that child;
  - delete jobs live on the child group, so a put and a delete of the same child can apply out of order.
  - **[unverified]** black-box; follows from the table.
- Non-transactional multi-entity commits are unaffected: the v1 layer applies mutations one at a time, so versions are per mutation
  either way **[golden consistency 2]**.

---

## 8. Everything else (Q7)
- **Error codes and the database id** are unchanged. CONCURRENT_TRANSACTION → 409 ABORTED. BAD_REQUEST → 400 INVALID_ARGUMENT.
  The database id is echoed and routed the same way **[golden databases, rest_routing identical]**.
- **GQL**: identical, including aggregation and the `GROUP BY` errors **[golden query_gql*]**.
- **Startup log**: see §1.1. The banner lines after "API endpoint" are identical.
- **Transaction handles** differ in every golden only because the shared counter had a different value when each recording
  was made (FS run ~1200-1300, DS run ~6-60). That is not a mode difference; replay must keep treating the handle as opaque.

---

## 9. Per-step explanation of every differing golden step

Legend:
- **H**: only the transaction handle bytes differ (counter state, §8).
- **CFG**: FS reports `{forceStrongConsistency: true}` instead of the probabilistic policy (§1.2, §3.2). Behaviour after the PATCH
  is the same as DS.
- **EG**: FS has no 25-group limit (§2.5).
- **NAQ**: FS allows non-ancestor queries in txns (§2.1, §2.5).
- **SHIFT**: a version or snapshotVersion one higher in FS, because an earlier step committed in FS but failed in DS (or the reverse).
- **PK**: `__entity_group__` is not registered in FS (§4).

| scenario | step | request | DS | FS | why |
|---|---|---|---|---|---|
| consistency | 1 | PATCH probability seed 1 pct 100 | echoes probability policy | forceStrongConsistency | CFG (steps 3-16 identical: the policy really is pct 100) |
| consistency | 17 | beginTransaction | handle | handle | H |
| consistency | 26, 27, 29, 31, 33 | PATCH (probability / id-only / `{}`) | probabilityJobPolicy seed 1 | forceStrongConsistency | CFG (id policy SCATTERED/SEQUENTIAL applied in both) |
| consistency | 34 | GET /v1/config | probabilityJobPolicy | forceStrongConsistency | CFG |
| consistency_seeded | 1, 8 | PATCH probability | echo | forceStrongConsistency | CFG (steps 2-7 identical: seed 42 / 50% RNG really applies) |
| query_pseudo | 10 | query `__entity_group__` | 400 `queries not supported on __entity_group__` | 200 empty batch | PK |
| query_pseudo | 15 | lookup `P:a/__entity_group__:1` | found `__version__` = start-time micros + group version | missing, version 32 (= lastCommitTimestamp) | PK |
| txn_basic | 2, 3, 18, 20, 23, 26, 29, 31, 35 | beginTransaction | handle | handle | H |
| txn_contention | 2, 9, 10, 16, 20, 24, 28, 32, 36, 41 | beginTransaction | handle | handle | H (all contention outcomes identical: same group model) |
| txn_failures | 2, 11, 17, 21, 25, 30, 32, 37, 39, 42, 45 | beginTransaction | handle | handle | H |
| txn_failures | 46 | txn lookup `K:a/__entity_group__:1` | found `__version__` | missing, version 10 | PK |
| txn_limits | 2, 5, 8, 12, 14, 16, 19 | beginTransaction | handle | handle | H |
| txn_limits | 15 | commit 26 root groups | 400 too many entity groups | 200: 25× v4 (identical re-puts of step 13), 1× v5, commitVersion 5 | EG |
| txn_limits | 17 | txn lookup 26+ root keys | 400 too many entity groups | 200 all missing, version 5 | EG (+SHIFT) |
| txn_limits | 20 | non-ancestor query in txn | 400 Only ancestor queries… | 200 `K:a`, snapshotVersion 5 | NAQ (snapshotVersion from `__none__` lookup, §3.3) |
| txn_limits | 21, 22 | ancestor queries in txn | snapshotVersion 4 | snapshotVersion 5 | SHIFT (step 15 committed in FS) |
| txn_limits | 23, 26, 28 | lookup / runQuery with newTransaction | handle (+v5 / snap 5) | handle (+v6 / snap 6) | H + SHIFT |
| txn_limits | 24, 25, 31, 32, 35 | commits and lookups | versions 5 / 6 | 6 / 7 | SHIFT |
| unsupported | 14, 16 | begin readOnly.readTime / lookup newTransaction readTime | handle | handle | H (readTime ignored in both) |
| txn_isolation | 2, 6, 10, 16, 20, 24, 26, 29, 30, 33 | beginTransaction | handle | handle | H |
| txn_isolation | 17 | `K v>2` query in txn t4 | 400 Only ancestor queries… | 200 `K:d` (v=4, version 5), `P:r/K:a` (v=11, version 7), `P:r/K:b` (v=20, version 6), `K:c` (v=31, version 9), snapshotVersion 9 | NAQ: live data; tracks groups `K:d`, `P:r`, `K:c` + `__none__:1` |
| txn_isolation | 19 | t4 commits `K:e` after outside write to `K:d` (step 18) | 200 v11 | 409 ABORTED contention | the query tracked group `K:d`, whose version moved (§2.1). In DS t4 tracked nothing (its query failed) |
| txn_isolation | 21 | `K v>100` in txn t5 | 400 Only ancestor queries… | 200 empty, snapshotVersion 10 | NAQ; nothing matched, so only `__none__:1` is tracked |
| txn_isolation | 22 | outside insert `K:f v=500` (phantom) | v12 | v11 | SHIFT (step 19 failed in FS) |
| txn_isolation | 23 | t5 commits `K:g` | 200 v13 | 200 v12 | phantom not detected (only matching entities are tracked); SHIFT |
| txn_isolation | 25, 31, 32 | commits | v14 / 15 / 16 | v13 / 14 / 15 | SHIFT (blind-write steps 29-32 behave the same in both: last committer wins; neither aborts because neither read) |
| txn_isolation | 34 | txn lookup of 30 root keys | 400 too many entity groups | 200 all missing, version 15 | EG |
| txn_isolation | 35 | commit 30 groups | 400 too many entity groups | 200 all v16, indexUpdates 186 | EG |
| txn_isolation | 36-43 | OR / multi-ineq / IN / NOT_IN / != / order / GQL | identical errors | identical errors | not mode dependent (§4) |

The allowlist entries in `tests/conformance/allowlist-firestore.txt` for query_pseudo 15 and txn_failures 46 describe the
DS behaviour. In FS, Java returns `missing` with `version = lastCommitTimestamp`, which is deterministic, so they need not be
allowlisted.

---

## 10. Findings that also affect the Datastore-mode model (check against the repo)
1. The `__none__:1` snapshotVersion lookup tracks a shared pseudo-group, so two writing txns that both ran (ancestor) queries
   conflict (§2.4). This was probed in both modes, and the repo has no `__none__` handling.
2. Read-time contention errors do not mark the txn failed (§2.1).
3. Snapshots are per txn per project, not per group.
4. The transaction handle counter starts at 1.

## 11. Unverified items and how to probe them
- The `__collection__`/`__document__` namespace and database scoping, and cursor/limit/projection behaviour: run `HAS_PARENT` queries with
  `partitionId.namespaceId` and `databaseId` against an FS emulator.
- The DS-mode `child/__entity_group__:1` 500 with entity groups on: look up a 3-element `__entity_group__` key on a DS emulator.
- EG-off eventual consistency ordering (§7, last bullet): on a DS emulator with `--enable_entity_groups=false`, PATCH pct 100, put and
  delete a child, then do a strong lookup.
- A multi-batch FS txn non-ancestor query followed by `:runQuery` with the cursor: continuation should work, unlike the ancestor-snapshot quirk (txn notes §1.5).
