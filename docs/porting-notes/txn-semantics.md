> **Corrections from the Firestore-mode study** (`firestore-mode.md` §10), which override this note:
> a contention error at read time does **not** close the transaction (only a failed commit does);
> reads from the transaction's snapshot see the data as of the first outside write to any tracked group;
> the transaction handle counter starts at 1; and every query run in a transaction also tracks
> the pseudo-group `__none__:1` (observed through `snapshotVersion`), so writing transactions that
> both ran a query conflict.

# Local Datastore: transactions, versions, eventual consistency (porting notes)

Source: GoogleCloudPlatform/appengine-java-standard `main`. Abbreviations (all under
`api_dev/src/main/java/com/google/appengine/api/datastore/dev/` unless stated):
`LDS`=LocalDatastoreService.java, `LDJ`=LocalDatastoreJob.java, `HRJP`=HighRepJobPolicy.java,
`DHRJP`=DefaultHighRepJobPolicy.java, `EGPK`=EntityGroupPseudoKind.java, `CA`=LocalDatastoreCostAnalysis.java,
`Utils`=Utils.java, `proto`=`protobuf/api/datastore_v3_bytes.proto`. Query matching/ordering is in
`query-semantics.md` and is not repeated here.

**The emulator's observable behavior matches these classes in the points below and differs in others**
(`firestore-mode.md` §10). It matches them in: the same error strings
("too much contention…", "Cannot retry a read-only transaction.", "Cannot modify entities in a read-only
transaction.", the XG message, "Unable to take transaction snapshot.", "Unapplied job percentage must be >= 0 /
<= 100.00 (received %f)") and the same seeded-RNG job policy, confirmed by the `consistency*` goldens. The CLI flag
`--consistency` is described as "fraction of job application attempts that will succeed … Defaults to 0.9", so
pct = (1−c)·100. `--testing` means consistency 1.0, and `--firestore_in_datastore_mode` ignores `--consistency`
(`firestore-mode.md`).
Anything that only the v1 layer could decide is marked **[outside repo]**. The string "Concurrency exception" does
**not** occur in this repo or in the emulator's responses. It probably comes from a client library **[outside repo]**.

Error helper: `newError(code, msg)` → `ApiProxy.ApplicationException(code.getNumber(), msg)` (Utils:39-41).
`checkRequest(ok,msg)` → BAD_REQUEST (Utils:33-37). Codes (proto:528-540): BAD_REQUEST=1,
CONCURRENT_TRANSACTION=2, INTERNAL_ERROR=3, TIMEOUT=5.

---

## 1. Transactions

### 1.1 Data structures
- **LiveTxn** (LDS:2847-2985) fields:
  - `creationTime` = `clock.getCurrentTime()` when it is constructed (LDS:2882, `HasCreationTime` 2512-2522).
  - `entityGroups: HashMap<EntityGroup, EntityGroupTracker>` (2869). `EntityGroup` does not override `hashCode`, so
    this map iterates in **identity-hash order, i.e. non-deterministic** (it matters for commit, see 1.6).
  - `actions` (task-queue adds, max 5: LDS:359, 2941-2948, error `"Too many messages, maximum allowed: 5"`).
  - `allowMultipleEg`, `failed`, `concurrencyMode`, `originalTransactionMode`.
- **ConcurrencyMode**: `UNKNOWN`/`READ_WRITE` → `OPTIMISTIC`, `READ_ONLY` → `READ_ONLY` (LDS:3338-3347).
  PESSIMISTIC and SHARED_READ are rejected by `checkArgument` (LDS:2886-2888) but cannot be produced anyway.
- **EntityGroupTracker** (LDS:2987-3049):
  - `entityGroupVersion` = `eg.getVersion()` **at the moment the txn first touches the group** (2999).
  - `written: HashMap<Reference,EntityProto>` and `deleted: HashSet<Reference>`.
  - `isDirty()` = `written.size()+deleted.size() > 0`.
- **Profile.EntityGroup** (LDS:1891-2053):
  - `version: AtomicLong` starts at **0**. It is in-memory only, because `groups` is `transient` (2076) and is reset
    on restart.
  - `snapshots: WeakHashMap<LiveTxn, Profile>` (1894).
  - `unappliedJobs: LinkedList<LocalDatastoreJob>` (1896).

### 1.2 beginTransaction (LDS:1563-1597)
1. `getOrCreateProfile(app)` (1564; an empty app fails `checkArgument` "appId not set", 1800).
2. If `previous_transaction` is set:
   - `mode == READ_ONLY` → BAD_REQUEST `"specifying a previous transaction handle on a read-only transaction is not allowed"` (1567-1569, 277-278).
   - Otherwise it looks up the previous txn **quietly**. If the handle is unknown or expired, it is silently ignored (1573-1576).
   - If the previous txn was READ_ONLY → BAD_REQUEST `"Cannot retry a read-only transaction."` (1577-1579).
   - If `allowMultipleEg` differs → BAD_REQUEST `"Transaction options should be the same as specified previous transaction."` (1581-1583).
   - Otherwise it removes and closes the previous txn (1585).
   - **Nothing is inherited** from the previous txn: no priority, no locks, no snapshot. A failed txn (1.8) is a valid previous txn.
3. Handle = `transactionHandleProvider.getAndIncrement()` (1593). This is an `AtomicInteger` **starting at 0**
   (LDS:419), shared by every app in one service instance. It resets on restart, is never reused, and is stored
   in the `fixed64 handle`. The txn map is **per profile/app** (2087), so a handle used with another app → "not found".
4. Registers `new LiveTxn(clock, req.allow_multiple_eg, req.mode)` (1595). Nothing is snapshotted or tracked at begin.

### 1.3 Looking up a handle and the reuse errors
- `getTxn(handle)` → BAD_REQUEST `"transaction has expired or is invalid"` if the handle is missing (2163-2165, 1527-1533, 275).
- `removeTxn` = `getTxn` + `close()`, which drops this txn from every tracked group's `snapshots`, then removes it (2176-2181, 2964-2974).
- Every operation on a txn whose `failed` flag is set → BAD_REQUEST `"transaction closed"` (`checkFailed`,
  2980-2984, 273). The flag is checked in `trackEntityGroup` (2899), `isDirty` (2955) and `addActions` (2942),
  so in practice on every get/put/delete/ancestor query/commit. **rollback does not check it.**
- After a **successful commit** or a **rollback**, the handle is gone. Any reuse (get/put/delete/ancestor query/commit/rollback)
  → `"transaction has expired or is invalid"`.
- `rollback` (1702-1705) = `profiles.get(app).removeTxn(handle)`. If the app has no profile → Java NPE (no proto error). Unknown handle → the "expired or invalid" error.
- `commit` with no profile → `checkNotNull` NPE (1601-1602).

### 1.4 Entity-group tracking: `trackEntityGroup` (LDS:2895-2935)
Every get, put, delete and ancestor query in a txn calls `eg.addTransaction(txn)` (1948-1954). That call runs
`trackEntityGroup` and puts `snapshots[txn] = null` if the txn is absent. The checks run in this order:
1. null eg → NPE.
2. `checkFailed()` → "transaction closed".
3. Group already tracked → return the existing tracker. The version is not checked again.
4. New group:
   - XG: if `size >= MAX_EG_PER_TXN (25)` (LDS:200) → BAD_REQUEST `"operating on too many entity groups in a single transaction."`. So at most **25** groups.
   - Non-XG: if `size >= 1` → BAD_REQUEST, message `ENTITY_GROUP_MESSAGE + "found both " + eg + " and " + newEg` (2909-2911).
     That is literally `"cross-group transaction need to be explicitly specified, see TransactionOptions.Builder.withXGfound both <p1> and <p2>"`
     (**no space** before "found"). `<p>` is `Path.toString()` (1990-1992), i.e. protobuf text format (uncertain exact rendering).
     This check does **not** fail the txn. Read-only txns get the same checks.
5. **Consistency check on every already-tracked group**: if any tracker's stored version ≠ that group's current version,
   then `failed = true` and CONCURRENT_TRANSACTION `"too much contention on these datastore entities. please try again."` (2921-2929, 270-271, 3007-3011).
6. Create the tracker and record the group's current version (2931-2932).
- `MULTI_EG_TXN_NOT_ALLOWED` (267) is never used.

### 1.5 Reads inside a txn and snapshots
- **Snapshots are taken lazily, copy-on-conflict, per group.** `addTransaction` records `snapshots[txn]=null`. Until
  someone modifies the group, `getSnapshot(txn)` returns the **live** profile (1960-1970).
- `EntityGroup.incrementVersion()` (1913-1925) runs **before** each job for that group is created and added.
  - It does `old = version++`.
  - For each txn in `snapshots` whose tracker version for this group == `old`, it sets `snapshots[txn] = S`.
  - `S` is a single `takeSnapshot()` shared by all such txns: a deep copy of the **whole Profile** (all kinds,
    applied state only) made with Java serialization (1973-1987). On failure it throws RuntimeException `"Unable to take transaction snapshot."`.
- So reads of group G inside txn T see:
  - the **live, rolled-forward** state of G until the first write to G that happens after T started tracking G;
  - after that, the frozen state of G as it stood just before that write.
  - Writes by others **before** T first touched G are visible. There is no begin-time snapshot.
  - Cross-group consistency comes only from the check in 1.4 step 5: if G1 changed before T touches G2, T fails at once.
  - The txn's **own buffered writes are never visible** to its reads.
- Edge case: T touches G with a **blind** put/delete. Blind writes do no roll-forward. If G still had unapplied
  jobs and another writer then bumps G, the snapshot does not contain those unapplied jobs. A later read in T then
  sees state older than "committed before T touched G". T cannot commit anyway, because the version moved.
- **get with txn** (LDS:720-766):
  - Keys are processed in request order. `validatePathComplete` runs first.
  - The txn is looked up lazily at the first key (732-734), then `eg.addTransaction` (737).
  - `eventualConsistency = request.has failover_ms && liveTxn == null` (739). Txn gets are always strong.
  - Pseudo-kinds are tried first (740). For example, `__entity_group__` returns
    `__version__ = trackerVersion + baseVersion` with `baseVersion = currentTimeMillis*1000`, taken when the
    pseudo-kind is constructed (EGPK:43,67-77). Without a txn, `eg.getVersion()` is used instead.
  - Otherwise `eg.get` → `rollForwardUnappliedJobs()` (forced, on the live group) → read the snapshot (or live) extent (1933-1946).
  - Version in the response:
    - found → the stored entity version from the snapshot/live extent (750);
    - missing → `profile.getReadTimestamp()` of the **live** profile (746).
  - After **each key**, `profile.groom()` runs on the live profile (761).
- **runQuery with txn** (LDS:1195-1224):
  - First, if `query.app != txn.app` → **INTERNAL_ERROR** `"Can't query app " + app + "in a transaction on app " + txnApp` (no space before "in", 1197-1204).
  - Only if `query.hasAncestor()`: `getTxn` (unknown → "expired or invalid") → `eg.addTransaction` (the XG and contention checks above) →
    `profile = eg.getSnapshot(txn)` → `eg.rollForwardUnappliedJobs()` on the live group (1210-1223).
  - **No ancestor is required.** `TRANSACTION_REQUIRES_ANCESTOR` exists in `api/.../ValidatedQuery.java:305` but is never thrown.
    A non-ancestor query that carries a txn runs as a plain eventually-consistent global query: the handle is not
    looked up, nothing is tracked, and an invalid or expired handle is **not** an error. **[outside repo: the v1 layer may reject or handle it differently]**
  - Quirk: when a snapshot exists, `profile` *is* the snapshot copy. So the trailing `profile.groom()` (1270) grooms the copy (a no-op).
  - Quirk/bug: with a snapshot, a multi-batch result stores its LiveQuery in the **snapshot's** query map (1360).
    `next()` looks it up in the live profile (1537-1538), giving BAD_REQUEST `"query has expired or is invalid. Please restart it with the last cursor to read more results."`.
    It only happens when a txn ancestor query has `more_results` and the group changed since the txn touched it. Not verified black-box.
  - Result versions come from the snapshot/live extent (1257-1263). Projection and keys-only queries return no versions (2608-2615).

### 1.6 Writes inside a txn
- **put** (LDS:816-921):
  1. Every entity is validated and processed first, and **ids are allocated**, before the txn is looked up (824-864).
     Incomplete keys therefore consume ids even if the txn is invalid. Allocated keys are returned in the response.
  2. Under the profile lock, per entity:
     - `getTxn` (first entity only);
     - `checkRequest(!readOnly, "Cannot modify entities in a read-only transaction.")` (879);
     - `eg.addTransaction(txn).addWrittenEntity(clone)` (882).
  3. Nothing is applied. No versions are returned for txn puts (912-918). `cost` is an empty Cost message (821, 919).
- **delete** (LDS:1061-1125): per key, `validatePathComplete` → `getTxn` → the read-only check (same message, 1084) →
  `addTransaction(txn).addDeletedEntity(key)`. No versions are returned. Cost is set but empty (1066).
- **Same key several times**:
  - `addWrittenEntity`: last put wins, and it removes a pending delete.
  - `addDeletedEntity`: adds to the delete set and removes a pending put (3018-3036).
  - So the **last operation per key wins**. A key is never in both sets.
  - Outside txns, `WriteJob` also dedupes puts with last-wins (2408-2414) and deletes with a set (2407).

### 1.7 commit (LDS:1600-1699) — exact order
1. `profiles.get(app)` (NPE if missing) → take `globalLock.readLock` → `synchronized(profile)`.
2. `removeTxn(handle)`: unknown → "transaction has expired or is invalid". The txn is **not** re-added on this error. Then `close()`.
3. `liveTxn.isDirty()`: if failed → "transaction closed" (see step 7).
   **Not dirty** (no tracked puts/deletes, including every read-only txn) → success with
   `cost{entity_writes:0, index_writes:0}` and **no conflict check at all** (1618-1619). A read-write txn that only read data never fails at commit.
4. `commitImpl`: `checkEntityGroupVersion()` for **every** tracker, written or only read, in HashMap order. The first
   mismatch → CONCURRENT_TRANSACTION (code 2), `"too much contention on these datastore entities. please try again."` (1654-1658).
   Version = the group's counter (1.1). It is bumped by **every** job on the group, including non-txn writes,
   no-op writes, other commits, and other commits that only *read* the group (next step).
5. `commitTimestamp = profile.incrementAndGetCommitTimestamp()` happens **once per commit** (1663). Then for **every tracked group**,
   including groups this txn only read (1664-1692):
   - `eg.incrementVersion()`, which may snapshot for other txns;
   - `new WriteJob(policy, eg, profile, commitTimestamp, written, deleted)`. For read-only-tracked groups this job is empty, but
     it still bumps the version (so other txns that read the group get contention) and still draws RNG in `addJob`;
   - add the job's cost;
   - `eg.addJob(job)`, which tries policy-based application (section 2);
   - add a `CommitResponse.Version{root_entity_key = <the full entity key>, version = job.getMutationTimestamp(key)}`.
     All written keys come first, in HashMap order, then the deleted keys.
6. Response `cost` = sum of job costs.
7. Any ApplicationException in steps 3-5 → `profile.addTxn(sameHandle, new LiveTxn(clock, allowMultipleEg, originalMode, failed=true))`, then rethrow (1621-1627).
   - The failed txn has a **fresh creationTime** and no trackers.
   - Afterwards, commit, get, put, delete and ancestor query → "transaction closed". A repeated commit removes and
     re-adds it each time, so it keeps returning "transaction closed".
   - rollback succeeds.
   - A non-ancestor query with that handle still runs (1.5).
   - begin with `previous_transaction` = that handle is accepted and removes it.
8. After a successful commit, the txn's task-queue actions are dispatched. Failures are only logged (1638-1644).
- The global read lock is held through steps 2-7. Only `persist()` takes the write lock (3204).

### 1.8 Lifetime and expiry
- `datastore.max_txn_lifetime` defaults to **300000 ms** (LDS:184-186, 479-481).
- A sweeper runs `removeStaleTransactions(now)` with **initial delay = period = 5×lifetime = 1500 s** (637-642).
  It removes txns with `creationTime < now − lifetime` (3056-3072, 3282-3291), without `close()`. Afterwards → "transaction has expired or is invalid".
  The effective lifetime is therefore 300-1800 s after begin (or after a failed commit re-added the txn).
- Activity does not extend the lifetime. Txns are never persisted. `stop()` → `clearProfiles()` (664).
- Port note: on expiry, also drop the txn from the groups' snapshot maps. Java relies on a WeakHashMap and GC for this.

### 1.9 Cost (CA:67-96, LDS:2438-2454)
- Computed per job by `calculateJobCost()` against the state **just before the job**: the previous unapplied job's
  view of the key, else the live extents (`getSnapshotEntity`). For a commit this is the state at commit time, not the txn's snapshot.
- Delete of an existing entity → `getWriteOps(null, old)`. Delete of a missing entity → 0.
- Put → `getWriteOps(old, new)`:
  - 0/0 if `equalProperties(old,new)` (both `property` and `raw_property` lists are equal and in the same order, LDS:3325-3329);
  - otherwise `entity_writes = 1` and `index_writes = changedIndexRows + (old==null ? 1 : 0)`;
  - `changedIndexRows` counts the built-in ASC+DESC single-property indexes for every indexed property name, plus the
    composite indexes for the kind, × path length for ancestor composites (CA:106-196).
- Non-txn put/delete costs are summed over groups (903, 1110). Commit costs are summed over tracked groups (1674).

---

## 2. Jobs, versions and eventual consistency

### 2.1 Timestamps and versions
- `Profile.lastCommitTimestamp` is **per app**. It starts at `MINIMUM_VERSION = 1` (2064, 1872), is a serialized field,
  and so is **persisted** (read back as 0 from very old files).
- `getReadTimestamp()` returns it (2093). `incrementAndGetCommitTimestamp()` is `++` (2101-2103).
- Each **non-txn put/delete request** creates one `WriteJob` **per entity group**, using the 5-arg constructor,
  which increments the timestamp per group (2382-2395). So a multi-group non-txn write gets a **different timestamp per group**,
  in the order the groups first appear in the request (LinkedHashMap, 866, 1074).
- A **txn commit** uses **one timestamp for all its groups** (1663).
- The first write in a fresh profile gets version **2**.
- Entity version = the timestamp of the job that last changed it (2471). It is stored in `Extent.versions` and persisted
  as the reserved property `__entity_version__` (2251, 2291-2328). Legacy entities get 1.
- **EntityGroup.version** is a separate counter (1.1), used only for conflicts, snapshots and `__entity_group__`.
- `WriteJob.getMutationTimestamp(key)` (2478-2495), used for put/delete/commit response versions:
  - If `isNoOpWrite(key)` (2501-2509): previous unapplied job's `getMutationTimestamp(key)`; else the extent's stored
    version of key; else `profile.getReadTimestamp()`.
  - Otherwise the job's timestamp.
  - `isNoOpWrite`: delete → the pre-job entity is absent; put → `equalProperties(pre-job, new)`; key not in job → true.
  - Consequences:
    - identical re-put → the **old** version;
    - delete of a missing key → the current read timestamp (= this job's or commit's timestamp, since nothing else runs under the lock);
    - a real change → the new timestamp.
  - It is called **after** `addJob`, so the job may already be applied. After application, a changed put looks like a
    no-op but returns the stored version, which is the same value.
  - Quirk: deleting a key whose deletion is still in an **unapplied** previous job returns that previous job's timestamp.
- **Non-txn put/delete responses** return versions in request order (912-918, 1119-1123) whether or not the job was applied.
- **Lookup versions while jobs are unapplied**:
  - Strong get rolls forward first, so the version is current.
  - Eventual get (`failover_ms` set, no txn) returns the **applied** entity's version. A missing entity has **no version field** (745).
  - Strong missing → `lastCommitTimestamp`, which also counts the timestamps of unapplied jobs in all groups.

### 2.2 LocalDatastoreJob (LDJ) and WriteJob
- Fields: `newJob = true`, `applied = false`, `timestamp` (LDJ:34-52).
- `tryApply()` (LDJ:59-78):
  - if `newJob`: ask `shouldApplyNewJob(eg)`; else ask `shouldRollForwardExistingJob(eg)`;
  - on true → `apply()`, return true;
  - `finally newJob = false` (one policy call per invocation).
- `apply()` (LDJ:81-88): if already applied → IllegalStateException `"Job on entity group %s was already applied."`. Otherwise mark applied and run `applyInternal`.
- `WriteJob` (LDS:2375-2510):
  - `previousJob = eg.getLastJob()` at construction (2406), forming a chain through the unapplied jobs.
  - `applyInternal` (2457-2475): `previousJob = null`, then remove the deletes, then put each **non-no-op** put with
    version = the job timestamp. No-op puts leave the stored entity and its version untouched. It sets `dirty = true` (for persistence).
  - `getEntity`/`getSnapshotEntity` provide the "state after/before this job" view used for cost and no-op detection (2418-2436).

### 2.3 Applying jobs in an entity group
- `addJob(job)` (2002-2008): append to `unappliedJobs`, add the group to `groupsWithUnappliedJobs` (a LinkedHashSet), then call `maybeRollForwardUnappliedJobs()`.
- `maybeRollForwardUnappliedJobs()` (2027-2045): iterate the group's queue **in order**. On `tryApply()` true → remove it.
  On false → **stop**, because jobs apply in order. If the queue becomes empty, remove the group from the set.
  - A new job on a group that already has unapplied jobs is **not** forced. The old head is retried first, through
    `shouldRollForwardExistingJob`. If the head fails, the new job gets **no** policy call and keeps `newJob = true`.
  - With n queued jobs, one `addJob` makes 1..n RNG draws.
- `rollForwardUnappliedJobs()` (2011-2020): apply **all** queued jobs with `apply()`, consulting no policy and drawing no RNG. Then clear the queue and the set entry.
- **Forced roll-forward happens only on**:
  1. a strong get (no txn and no `failover_ms`, or any txn get), for that key's group (1935-1938). Pseudo-kind keys skip `eg.get`, so no roll-forward;
  2. an ancestor query with a txn or without `failover_ms`, for the ancestor's group (1219-1223);
  3. `stop()` when storage is enabled: `rollForwardAllUnappliedJobs()` over every profile and group, then `persist()` (657-662, 673-685).
- **Not forced** by: puts, deletes, commits (even of a txn that touches the group), `beginTransaction`, `persist()` (3203-3235, which saves applied state only,
  so unapplied writes are not in the periodic file), eventual gets, eventual ancestor queries, and non-ancestor queries.
- **Groomer** `profile.groom()` (2132-2140): for each path in a *copy* of `groupsWithUnappliedJobs` (insertion order), call
  `maybeRollForwardUnappliedJobs()`.
  - Called **after each key** of every get (761) and once per `runQuery` after the result set is captured (1270).
    It does not affect the current result.
  - It is not called by `next()`, put, delete, commit or any timer.
- **What reads see**:
  - Non-ancestor queries (with or without a txn) and eventual ancestor queries read the live extents, i.e. **applied jobs only**.
  - Eventual gets read applied state too.
  - Strong gets, strong or txn ancestor queries and txn reads see everything.

### 2.4 DefaultHighRepJobPolicy (DHRJP) — exact RNG
- Properties: `datastore.default_high_rep_job_policy_random_seed` (`Long.parseLong`) and
  `datastore.default_high_rep_job_policy_unapplied_job_pct` (`Float.parseFloat`) (DHRJP:37-47, 115-132).
  - Default pct is **0**. A NumberFormatException propagates.
  - Default seed is `System.currentTimeMillis()` when the policy is constructed.
- `random = new java.util.Random(seed)` (90). Then:
  - pct `< 0` → IllegalArgumentException `"Unapplied job percentage must be >= 0 (received %f)"`;
  - pct `> 100.00d` → IllegalArgumentException `"Unapplied job percentage must be <= 100.00 (received %f)"` (91-100).
- `RANDOM_CEILING = 10000`, `cutoff = (int)(pct_float * 100)`, computed in float arithmetic and truncated (64-70, 101).
  - Example: `0.29f*100 = 28.999998 → 28`.
  - NaN passes both checks and gives cutoff 0.
- `shouldApplyNewJob` and `shouldRollForwardExistingJob` behave identically and ignore the group: `random.nextInt(10000) >= cutoff` (134-156).
  This is **one draw per `tryApply` call, per job attempt**, not per group or per request.
  - pct 0 → always applies, but still consumes draws.
  - pct 100 → never applies until forced.
- `java.util.Random`, needed for bit-exact replay:
  - `seed0 = (seed ^ 0x5DEECE66D) & (2^48−1)`.
  - `next(31)`: `s = (s*0x5DEECE66D + 0xB) & (2^48−1)`; return `(int)(s >>> 17)`.
  - `nextInt(10000)` (10000 is not a power of 2): `u = next(31); r = u % 10000; while (u − r + 9999 < 0 /*int overflow*/) { u = next(31); r = u % 10000 }`.
- Selecting the policy (LDS:557-590):
  - The property `datastore.high_replication_job_policy_class` loads a class reflectively.
  - With `datastore.use_vnext=true`, `SpannerJobPolicy` always applies (594-604). A custom class then → BAD_REQUEST `VNEXT_POLICY_DISALLOWED` (293-295, 570-573).
- Even with a fixed seed, the draw order is only deterministic up to the commit's HashMap iteration over tracked groups (1.1).

### 2.5 Scheduler and lifecycle
- One static `ScheduledThreadPoolExecutor` serves all instances: 10 daemon threads named `"LocalDatastoreService-%d"`,
  with remove-on-cancel and a JVM shutdown hook `cleanupActiveServices` (396-413).
- `cleanupActiveServices()` (3299-3309) calls `stop()` on every active service, then `scheduler.shutdownNow()`.
- `start()` (620-649):
  - runs `load()`;
  - schedules the query sweeper (every 5×`max_query_lifetime`, default 30000 → 150 s) and the txn sweeper (1500 s);
  - unless `no_storage`, schedules `persist()` every `store_delay` ms (default 30000).
- `stop()` (651-671): roll forward and persist (if storage) → clear profiles and query history → cancel tasks.

---

## 3. Implementation checklist and open points
- The emulator's behavior differs from the SDK code in places. Confirm the details marked uncertain by black-box tests. In particular:
  - the txn-ancestor-query cursor bug (1.5);
  - non-ancestor queries inside txns;
  - the XG message rendering;
  - how CONCURRENT_TRANSACTION maps to the v1/gRPC code (probably ABORTED) **[outside repo]**.
- Keep two counters per app: `lastCommitTimestamp` (persisted, gives entity versions) and per-group `version` (memory only, for conflicts).
- A txn that writes nothing never conflicts. Read-only groups in a writing txn are checked at commit **and** bumped.
- A failed commit leaves a "failed" txn under the same handle: rollback succeeds, everything else returns "transaction closed".
