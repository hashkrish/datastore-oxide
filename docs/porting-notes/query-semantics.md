# Local Datastore (App Engine dev) query semantics — porting notes

Source: GoogleCloudPlatform/appengine-java-standard (main). Paths are abbreviated:

- `LDS` = `api_dev/.../datastore/dev/LocalDatastoreService.java`
- `api/` = `api/src/main/java/com/google/appengine/api/datastore/`
- `dev/` = `api_dev/src/main/java/com/google/appengine/api/datastore/dev/`
- `core/` = `api_dev/src/main/java/com/google/cloud/datastore/core/`

Everything here works on **v3 protos** (`DatastoreV3Pb.Query`, `EntityProto`, `PropertyValue`).
The Cloud Datastore emulator's v1 to v3 translation layer is **not** in this repo, so anything that
depends on it (v1 `IN`/`NOT_EQUAL`/`NOT_IN`, `distinct_on` requirements, empty arrays, timestamp
meaning on output) is marked **[outside repo]**.

**Black-box results checked against the source.** The source agrees with all of them: the type order;
min/max for arrays; inequality ranges spanning types; projections returning meaning 18
(`INDEX_VALUE`) values sorted by projected property name; batches of 300; cursor shape. The
only addition is that the type order also has **user**, between geo and key.

---

## 1. Value comparison and ordering

### 1.1 How values become comparable
- Filters and sorts use the **raw index value** and ignore `meaning`:
  `DataTypeTranslator.getComparablePropertyValue(Property)` wraps `RawValue(property.getValue())`
  (`api/DataTypeTranslator.java:765-768`, `RawValueType.asComparable` `:1116-1125`).
- `RawValue.getValue()` checks the fields in this order: bool, double, int64, point→`GeoPt`,
  reference→`Key`, string→`byte[]`, user→`User`, and returns `null` if none is set
  (`api/RawValue.java:107-123`).
  - **Timestamps (GD_WHEN, meaning 7) are plain int64.** They compare equal to ints with the same
    microsecond value, and they mix with ints in ranges.
  - **Strings, blobs, short blobs, text, links, emails and so on are all the `string_value` bytes.** They compare as
    unsigned bytes, lexicographically, with a shorter prefix sorting first
    (`ComparableByteArray.compareTo`, `api/DataTypeTranslator.java:2015-2025`). A string and a
    blob with the same bytes are **equal**.
  - An empty `PropertyValue` (null) gives Java `null`.
- `__key__` is synthesized from `EntityProto.key` (`api/DataTypeTranslator.java:382-391,402-404`).

### 1.2 Type order across types
`MultiTypeComparator` (`api/BaseEntityComparator.java:212-233`):
- `null` < everything (`:215-224`).
- Otherwise the comparator compares the rank, which is the v3 `PropertyValue` field tag
  (`api/DataTypeTranslator.java:183-195,790-793`):

| rank | type |
|---|---|
| (null) | null |
| 1 | int64 (includes timestamps and ratings) |
| 2 | boolean |
| 3 | string/bytes (ComparableByteArray) |
| 4 | double |
| 5 | point (GeoPt) |
| 8 | user |
| 12 | reference (Key) |

So the full order is: null < int < bool < string/blob < double < point < user < key.

### 1.3 Order within a type
- **int64**: `Long.compareTo` (signed).
- **bool**: false < true.
- **double**: `Double.compareTo`, so **-0.0 < +0.0**, they are **not equal** (an equality filter for
  0.0 does not match -0.0), **NaN > +Inf**, and NaN == NaN (any NaN bit pattern).
- **point**: converted to **float** first, `(float)x, (float)y` (`api/DataTypeTranslator.java:1371-1373`).
  The comparator uses `Float.compareTo` on latitude, then on longitude (`api/GeoPt.java:78-84`).
  Because of the float conversion, points that differ only below float precision compare equal.
- **user**: `email.compareTo` only, a Java String (UTF-16) comparison. Auth domain and gaia id are
  ignored (`api/users/User.java:192-194`).
- **key** (`api/Key.java:426-524`): walk both paths from the root (`getPathIterator`, `:395-401`).
  For each pair of elements:
  1. Compare `AppIdNamespace`: app id, then namespace. Both are Java `String.compareTo`
     (`api/AppIdNamespace.java:98-104`). Every element in a path carries the key's app and namespace,
     so this is effectively compared at the first element.
  2. Compare the kind with `String.compareTo`.
  3. **An id sorts before any name.** Ids compare with `Long.compare` (signed). Names compare with
     `String.compareTo` (`:497-523`). If both elements are incomplete, the result comes from the
     identity hash (`:489-495`). An incomplete element against a named one behaves like name=null,
     which sorts after the name (`:517-522`).
  4. If all shared elements are equal, the **shorter path sorts first**: an ancestor sorts before its descendants (`:449-457`).
- **Caveat:** Java `String.compareTo` compares **UTF-16 code units**, not code points. It differs
  from UTF-8 byte order only for supplementary characters compared with U+E000–U+FFFF. Kind
  names, key names, namespaces and user emails use this comparison. String *property values* use UTF-8
  bytes.

### 1.4 Multi-valued properties in sorts
`BaseEntityComparator.compare` (`api/BaseEntityComparator.java:139-182`):
- For each adjusted order, take the entity's values for that property. **Ascending uses the min**
  and **descending uses the max** (`:160-176`).
- The min or max is taken only over values that pass that property's **inequality range**
  (`considerValueForOrder` → `matchesRange`, `api/FilterMatcher.java:104-128`). Equality filters do
  **not** restrict which value is used for ordering.
- If either side has no values (`null`), the comparison returns `0` immediately (`:150-154`). This
  is meant for truncated cursor entities.
- If no value passes the range, `multiTypeExtreme` throws `IllegalArgumentException("Entity
  contains no relevant values.")` (`:205-208`). This can only happen with an odd cursor entity.
- Values are read only from `EntityProto.property`, never from `raw_property`. If a property has a
  **non-multiple** entry, the comparator uses **only the first non-multiple entry**. Otherwise it uses
  all `multiple=true` entries (`api/DataTypeTranslator.java:400-416`).

---

## 2. `runQuery` execution (`LDS:1176-1373`)

1. **Validate and normalize** with `LocalCompositeIndexManager.ValidatedQuery(query)`, which is
   `NormalizedQuery` plus `ValidatedQuery` (§3). The builder is **mutated in place** (`LDS:1179-1181`).
2. **Modernize cursors** (`CursorModernizer.modernizeQueryCursors`). On `InvalidConversionException`
   it returns BAD_REQUEST `"Invalid cursor"` (`LDS:1186-1190`).
3. **Transaction app check**: INTERNAL_ERROR `"Can't query app " + app + "in a transaction on app " + txnApp`. The missing
   space is in the original message (`LDS:1196-1205`).
4. **Ancestor**: with a transaction, read the entity group's snapshot. With a transaction, or when
   `failover_ms` is unset, apply pending jobs first so the read is strongly consistent
   (`LDS:1207-1224`). Non-ancestor queries see **only applied jobs**: eventual consistency through
   HighRepJobPolicy.
5. A `search_query` returns BAD_REQUEST `"full-text search unsupported"` (`LDS:1226-1228`).
6. **Pseudo-kinds** run first (§4) (`LDS:1231`). Their results then go through **all of the steps
   below** as well.
7. **Candidate set** (`LDS:1234-1265`):
   - With a kind: every entity in that kind's extent. One extent per kind covers **all namespaces**.
   - Kindless: every entity of every kind. If the query has **no orders**, it adds an explicit
     `__key__ ASC` (`LDS:1242-1252`).
   - A missing extent gives an empty result.
8. **Predicates**, all ANDed together (`LDS:1278-1331`):
   - Ancestor: the entity path starts with the ancestor's path elements. Elements are compared by
     proto equality of type plus id/name. App and namespace are not compared here; validation has
     already checked them (`:1280-1288`).
   - `shallow`: path length equals the ancestor length + 1, or 1 if there is no ancestor (`:1290-1295`).
   - Namespace: if the query **has** `name_space`, the entity key must have `name_space` equal to it. If
     it does not, the entity key must **not have** `name_space` (`:1297-1314`). This checks field
     *presence*. An explicit `""` on one side and an absent field on the other do **not** match, and
     no normalization happens. **[edge; the v1 layer probably normalizes]**
   - Filters: `EntityProtoComparator.matches` (§2.1).
9. **Projection expansion** if `property_name` is non-empty (§2.3) (`LDS:1334-1336`).
10. **Sort** with `Collections.sort`, which is stable, using the comparator and its adjusted orders (§2.2) (`LDS:1338`).
11. **Group-by / distinct** (§2.4) (`LDS:1341`). It runs after sorting.
12. Build the `LiveQuery` (snapshot list). Cursors and the limit are applied here (§2.5) (`LDS:1344`).
13. The composite index manager `processQuery` can throw NEED_INDEX (§6) (`LDS:1347`). This happens
    *after* the query has run but before results are returned.
14. Fetch the first batch with `nextResult(offset, count, compile)`. If `compile` is set, also add
    `compiled_query` (`LDS:1350-1357`). If `more_results` is true, register a cursor
    `{app, cursor=queryId++}` (`LDS:1358-1362`). Add `index` entries for the composite index the
    query would use (`LDS:1364-1366`).

### 2.1 Filter matching (`api/FilterMatcher.java`, `api/EntityProtoComparators.java:59-67`)
- Filters are grouped **per property name** into one `FilterMatcher`
  (`api/BaseEntityComparator.java:111-122`).
- Every property in the **query orders** (the normalized ones, stopping at the first `__key__` order)
  gets `MATCH_ALL` if it has no filter. This makes the order act as an **existence filter**
  (`:124-133`).
- An entity matches only if **every** filtered or ordered property has at least one indexed value
  (`values != null`, `EntityProtoComparators:60-64`), and the matcher accepts those values:
  - **Each EQUAL value must be present among the entity's values** (binary search) (`FilterMatcher:136-141`).
    `a=1 AND a=4` matches an entity whose `a` contains both 1 and 4.
  - All inequalities on the property are merged into one range, keeping the tightest bounds
    (`:175-206`). **At least one value** must fall in the range (`:166-172`). The range is checked
    independently of the equality values, so `a<0 AND a=4` matches `[-1,4,2]`.
  - Comparisons use the multi-type comparator, so a range spans types: `a > 5` also matches every
    string, double, key and so on, and `a < "x"` matches ints and bools.
  - `EXISTS` adds no constraint beyond `values != null`. `CONTAINED_IN_REGION` needs some GeoPt value
    inside the region. Any other operator throws `IllegalArgumentException("Unable to perform filter
    using operator " + op)` (`:207-210`).
- **IN**: `NormalizedQuery` rewrites a single-property `IN` to `EQUAL` (`api/NormalizedQuery.java:65-67`).
  An IN with more than one property fails validation (§3). The v3 API has no `!=`. **[outside repo:
  the v1 IN/NOT_EQUAL/NOT_IN translation]**
- **Null**: a null filter value compares as `null`, which equals an empty `PropertyValue`.

### 2.2 Adjusted orders (`api/BaseEntityComparator.java:70-109`)
The comparator builds the adjusted order list from the normalized query:
1. The query's orders.
2. If there are **no** orders: ASC on the property of the **first inequality filter** (`:87-100`).
3. For **every EXISTS filter**, ASC on its property, **sorted by property name** (`:72-82,102`).
   Normalization adds EXISTS filters for projected and group-by properties that have no
   equality or inequality filter (`NormalizedQuery:116-126`). That is why a projection with no order
   sorts by the projected properties in alphabetical order. These come **after** the explicit orders, even
   after an explicit `__key__` order.
4. Add `__key__ ASC` unless the last order is already exactly `__key__ ASC` (`:104-107`).
   The **tie-break is always key ascending**, even when the query orders by `__key__ DESC`
   (`[__key__ DESC, …exists, __key__ ASC]`).

`NormalizedQuery` (`api/NormalizedQuery.java:50-152`):
- Drop duplicate EQUAL filters, where the name and value are the same (`:70-74`).
- Drop orders on properties that have an equality filter and no inequality filter, and drop
  repeated orders (`:86-95`).
- Drop EXISTS filters that duplicate an existing filter property (`:100-113`).
- Add EXISTS filters for projection and group-by properties (`:116-126`).
- If there is any `__key__ = k` filter, **clear all orders** (`:130-137`).
- Drop every order after the first `__key__` order (`:141-151`).

### 2.3 Projections (`LDS:1434-1521`, `LDS:2716-2738`)
- `postfixProps` holds the property names of all adjusted orders, including `__key__`. Projected
  properties are always in this set through their EXISTS orders.
- For each entity (`createIndexEntities`):
  - If **no** postfix property has more than one entry, the entity is kept whole. It is trimmed at
    output time (`:1475-1478`).
  - Otherwise the entity is split into the **cartesian product** of the postfix properties' values. Only
    values that pass that property's whole `FilterMatcher` count, each as a single value
    (`matches(prop)`, `api/EntityProtoComparators.java:69-76`). Duplicate values within a
    property are removed (hash set of `PropertyValue`). Each split entity holds only the key, an
    empty `entity_group`, and the postfix properties with `meaning=INDEX_VALUE (18)` and
    `multiple=false` (`LDS:1480-1515`).
  - As a result, a multi-valued sort or inequality property produces **one result per distinct
    matching value combination**.
  - A non-projected order property also causes splits, so the same key can appear more than once.
- Output (`postProcessEntityForQuery`, `LDS:2718-2738`): the key, an empty `entity_group`, and only the
  projected properties, each with `meaning=INDEX_VALUE`, `multiple=false`, and the raw value. That raw value
  has no meaning, so timestamps come back as bare int64. If one output still holds two values for a
  projected property, the call fails with INTERNAL_ERROR `"LocalDatastoreServer produced invalid results."`.
- Projected properties must exist and be **indexed** (EXISTS). Entities that lack one are excluded.
- `result.index_only = property_name_count > 0`. `version` entries are omitted for projection and
  keys-only queries (`LDS:2608-2615,2684-2686`).

### 2.4 Group-by / distinct (`LDS:1393-1425`)
- This step runs on the sorted list, after any projection split.
- It walks the list and remembers the `(name, PropertyValue)` set of the **last emitted** entity, for
  the group-by properties only. An entity is emitted if **any** of its group-by `(name,value)` pairs
  is missing from that set. When it is emitted, the set is replaced with its own pairs.
- Duplicates are therefore removed **only between consecutive entries**. Validation requires group-by
  properties to come first in the ordering (§3), so equal tuples end up next to each other.
- The **first entity of each group in sort order** is kept. Because of the key tie-break, that is the
  one with the smallest key when the other orders tie.
- Equality uses proto `PropertyValue` equality, which is **not** comparator equality. For example
  -0.0 and 0.0 differ, and so do an int and a timestamp with the same value.
- If an entity was not split, it keeps all of its values. With multi-valued group-by properties, the
  comparison then uses the whole set of values (edge case).

### 2.5 LiveQuery: cursors, offset, limit, batches (`LDS:2525-2844`)
- **Snapshot**: the result list is frozen when `runQuery` runs. The constructor copies it (`:2605`).
- **Start and end cursors** (`DecompiledCursor`, `:2531-2565`):
  - A missing cursor puts the position at 0 for the start, or at the end of the list for the end cursor.
  - A cursor that is present but has **no key and no index_value** counts as an unset start cursor,
    *even when it was given as the end cursor* (`isStart=true`, `:2543-2547`). Its position is 0, so it
    **empties the result** when used as an end cursor. This is a quirk of the original code.
  - Otherwise the cursor is turned into a pseudo-entity (`decompilePosition`, `:2750-2794`):
    - The key is taken from `postfix_position.key`. If the query has a kind and it differs from the
      key's last-element kind, the request fails with BAD_REQUEST
      `"The query kind is %s but cursor.postfix_position.key kind is %s."`.
    - Each `index_value` becomes a property with `multiple=false`. It also gets
      `meaning=INDEX_VALUE` when the query has a projection or group-by.
    - Every index_value name must belong to the cursor property set, and every name in that set
      must be present. The cursor property set is the group-by properties if there are any. Otherwise it is
      the adjusted order properties **minus `__key__`**. Both failures give BAD_REQUEST
      `"cursor does not match query"`.
  - Position: `binarySearch(entities, cursorEntity, comparator)`, using the same comparator and the
    same min/max-in-range rules (`:2559-2564`).
    - When the cursor entity is found: position = `loc` if `inclusive`, otherwise `loc+1`. `inclusive` is
      `postfix_position.before`.
    - When it is not found: position = the insertion point.
    - A cursor entity without a key (the group-by case) compares as `0` on `__key__` (`getComparablePropertyValues`
      returns null when `!hasKey`, `api/EntityProtoComparators.java:87-89`). The binary search then lands
      somewhere inside the run of equal values. **[ordering inside a run of ties is not well defined]**
  - The cursor entity's properties are `multiple=false`, so the comparator uses **only the first**
    `index_value` with each name (§1.4). Its own compiled cursors store **every** value of a
    multi-valued order property (see below). Resuming from a full-entity (non-projection) cursor
    therefore compares with the *first stored value*, not the min or max. **[possible inconsistency in
    the original; worth a test]**
  - Applying the cursors: `end = min(endPos, size)`, then `start = min(endPos, startPos)`. The code
    drops `[end, size)` and then `[0, start)` (`:2626-2632`).
- **before / before_ascending** (`core/proto2/CursorModernizer.java`):
  - An `absolute_position` is rejected, and so are `position` together with `postfix_position`, and
    `start_key`. Each of these gives `"Invalid cursor"` (`:96-103`).
  - The legacy `position` field is moved into `postfix_position`, mapping
    `start_inclusive` to `before` (`:98-122`).
  - Where `before_ascending` is present: `before = before_ascending XOR (firstOrder == DESC)`.
    Otherwise the code computes `before_ascending` from `before` (`:128-135,166-177`).
  - `firstOrder` is the query's first normalized order. With no orders it counts as ascending.
  - The dev server only reads `before` (inclusive).
  - **[unsure]** The proto default of `IndexPostfix.before` is believed to be `true`. Check
    `datastore_v3.proto`.
- **Limit**: the code keeps `min(limit + offset, INT_MAX)` entries after the cursors (`:2635-2641`).
- **Offset**: `remainingOffset` starts at `query.offset`.
  - If a request gives an offset different from `remainingOffset`, the server returns BAD_REQUEST `"offset
    mismatch"` (`:2665-2667`).
  - Each batch skips at most `MAX_QUERY_RESULTS = 300` entries (`:2644-2652`, `LDS:173`) and reports them in
    `skipped_results`.
  - Results are returned **only after the whole offset is used up** (`:2679`). A large offset therefore produces
    batches that contain only skips.
- **Batch size**: `count` comes from the request, then `query.count`, then `DEFAULT_BATCH_SIZE = 20`
  (`LDS:170`). It is capped at 300 (`:2657-2663,2681`).
- `more_results = !remaining.isEmpty()` (`:2692`). It is false exactly when the result is exhausted or
  the limit is reached. The server never returns an extra empty batch.
- `keys_only` and `index_only` are set on every batch (`:2693-2694`).
- **Compiled cursors** (`compilePosition`, `:2796-2832`), filled only when `compile=true`:
  - Without group-by: the cursor holds `key` = entity key, plus an `index_value` for **every**
    property entry whose name is an adjusted order property. Repeated values each get their own
    entry, in entity order.
  - With group-by: the cursor holds no key, plus the group-by properties' values.
  - `before=false`, and `before_ascending` is computed from that.
  - A batch can fill these cursors:
    - `compiled_cursor`: the position after the last returned or skipped entity. Before any result
      it is the start cursor's entity, or an **empty IndexPostfix**.
    - `skipped_results_compiled_cursor`: set when entries were skipped.
    - One `result_compiled_cursor` per result.
  - `compiled_query`: the primary scan's `index_name` holds the serialized query (`:2834-2843`).
- **next()** (`LDS:1536-1553`):
  - Looks up `{app, cursor}`. If the query is unknown, it returns BAD_REQUEST `"query has expired or is invalid. Please
    restart it with the last cursor to read more results."` (`LDS:289-291,2142-2144`).
  - When `more_results=false`, the query is removed. `deleteCursor` also removes it, and fails with the
    same error if the query does not exist (`LDS:1556-1560,2150-2154`).
- **Lifetime**: queries older than `datastore.max_query_lifetime` (default 30000 ms) are deleted by a
  sweeper that runs every 5× that interval (`LDS:179-181,630-635,3056-3072`).
- **Output for full entities**: a copy of the stored entity. Special properties are handled
  (`__scatter__` is stripped because it is stored but not visible). Indexes are post-processed:
  computed flattened properties are removed and stashed ones restored (`LDS:784-813`,
  `core/proto2/EntityStorageConversions.java:79-145`). The entity version is reported per result.

### 2.6 Side effect
`processQuery` and `queryIndexList` build an `IndexComponentsOnlyQuery` on the **same builder**
(`LDS:1347,1364`, `dev/LocalCompositeIndexManager.java:771-777`). It removes a trailing `__key__ ASC`
order and some `__key__` filters (`api/IndexComponentsOnlyQuery.java:62-116`) after the
LiveQuery has been built. The only visible effect is on `firstSortDirection` for later compiled
cursors, and ascending is the default anyway. A port can ignore this.

---

## 3. Validation (`api/ValidatedQuery.java`, runs after `NormalizedQuery`)

All failures are `ApplicationException(BAD_REQUEST, msg)` (`:318-321`). The checks run in this order:

| # | Condition | Message | Line |
|---|---|---|---|
| 1 | projection and keys_only | `projection and keys_only cannot both be set` | 53-56 |
| 2 | duplicate projection | `cannot project a property multiple times` | 60-65 |
| 3 | duplicate group-by | `cannot group by a property multiple times` | 68-72 |
| 4 | group-by name matches `^__.*__$` | `group by is not supported for the property: <name>` | 76-80 |
| 5 | a non-group-by order before all group-by properties are ordered | `must specify all group by orderings before any non group by orderings` | 84-94 |
| 6 | kindless with a non-`__key__` filter | `kind is required for non-__key__ filters` | 97-103 |
| 7 | kindless with an order other than `__key__` ASC | `kind is required for all orders except __key__ ascending` | 104-111 |
| 8 | ancestor app ≠ query app | `The query app is <app> but ancestor app is <aapp>` | 117-120 |
| 9 | ancestor namespace ≠ query namespace | `The query namespace is <ns> but ancestor namespace is <ans>` | 122-128 |
| 10 | filter property count ≠ 1 | `Filter has <n> properties, expected 1` | 137-141 |
| 11 | `__key__` filter value is not a reference | `__key__ filter value must be a Key` | 152-156 |
| 12 | `__key__` filter app ≠ query app | `__key__ filter app is <fapp> but query app is <app>` | 158-166 |
| 13 | `__key__` filter namespace ≠ query namespace | `__key__ filter namespace is <fns> but query namespace is <ns>` | 167-175 |
| 14 | inequalities on 2 properties | `Only one inequality filter per query is supported.  Encountered both <first> and <second>` (two spaces after the period) | 178-188 |
| 15 | EQUAL on a projected property | `cannot use projection on a property with an equality filter` | 190-193 |
| 16 | EQUAL on a group-by property | `cannot use group by on a property with an equality filter` | 194-198 |
| 17 | geo filter has no region, or has a point value | `Geo-spatial filter on <p> should specify GeoRegion rather than Property Value` | 201-207 |
| 18 | geo region is both or neither circle/rectangle | `Geo-spatial filter on <p> should specify Circle or Rectangle, but not both` | 209-216 |
| 19 | IN operator (unreachable for one property, see §2.1) | `Unsupported filter operator: IN` | 217-221 |
| 20 | geo + inequality | `Inequality filter with geo-spatial query is not supported.` | 225-229 |
| 21 | geo + ancestor | `Geo-spatial filter on ancestor query is not supported.` | 231-235 |
| 22 | geo + start/end cursor | `Start and end cursors are not supported on geo-spatial queries.` | 237-241 |
| 23 | inequality property not in group-by (when group-by is set) | `Inequality filter on <p> must also be a group by property when group by properties are set.` | 244-253 |
| 24 | first order ≠ inequality property | `The first sort property must be the same as the property to which the inequality filter is applied.  In your query the first sort property is <o> but the inequality filter is on <p>` (two spaces after "applied.") | 255-268 |

Notes:
- Checks 8, 9, 12 and 13 compare `getNameSpace()` strings, so an absent namespace equals `""` here.
  The runtime namespace predicate in §2 compares presence instead.
- Check 24 runs **after** normalization. An inequality combined with an equality on the same
  property still requires the order. If a `__key__ = k` filter has cleared the orders, check 24 cannot fail.
- `__key__` inequality filters count as inequalities for check 14, for example `__key__ > k` together with `a > 1`.
- Nothing here requires group-by properties to be projected, or forbids keys_only with group-by.
  **[outside repo: the v1 `distinct_on` rules]**
- Other errors from the dev server: `"Invalid cursor"`, `"full-text search unsupported"`, `"offset
  mismatch"`, `"cursor does not match query"`, the cursor-kind mismatch message, and the
  query-not-found message (all in §2).

---

## 4. Pseudo-kinds (`dev/PseudoKinds.java`, registered at `LDS:516-523`)

- They are looked up by `query.kind`. The pseudo-kind receives a **copy without the kind**
  (`PseudoKinds:64-75`).
- Its results then pass through the normal pipeline with the **original** query: the namespace
  predicate, filters including `__key__` (applied again), sorting with the key tie-break, projection,
  group-by, cursors, offset and limit.
- They read only applied data (`profile.getExtents()`) and only **indexed** `property` entries.
- `get()` on `__kind__`, `__property__` and `__namespace__` keys returns null (`KeyFilteredPseudoKind:137-144`).

### Shared rules for `__kind__`, `__property__`, `__namespace__` (`dev/KeyFilteredPseudoKind.java:55-128`)
- Every filter must be a single-property `__key__` filter using `<`, `<=`, `>`, `>=` or `=`. Otherwise
  BAD_REQUEST `"Only comparison filters on __key__ supported"`. A value that is not a key gives `"__key__ must be
  compared to a key"`.
- Filters are folded into the tightest `[start, end]` key range. `=` sets both ends, inclusive.
- Only a leading `__key__ ASC` order is allowed. Anything else gives `"Only ascending order on __key__ supported"`.

### `__kind__` (`dev/KindPseudoKind.java`)
- An ancestor gives `"ancestor queries on __kind__ not allowed"`.
- A key filter must be a root named `__kind__` key. Otherwise one of:
  - `"Key filter on __kind__ is invalid (key has parent) - received <key>"`
  - `"Key filter on __kind__ is invalid (must be a named key for __kind__) - received <key>"`
- Returns one entity for each kind whose extent has an entity in the query namespace. Kind names in the range are compared with `String.compareTo`.
- Entity shape: key `app` / `name_space` (only if non-empty) / path `[__kind__: name=<kind>]`,
  `entity_group = [that element]`, **no properties**.

### `__property__` (`dev/PropertyPseudoKind.java`)
- A transaction gives `"transactional queries on __property__ not allowed"`.
- Key filters accept either a `__kind__:<K>` root, which means "kind K, property ''", or
  `__kind__:<K>/__property__:<P>`. Failures give:
  - `"Key filter on __property__ is invalid (parent must be a named key for __kind__) - received <key>"`
  - `"Key filter on __property__ is invalid (must have no parent or __kind__ parent) - received <key>"`
  - `"Key filter on __property__ is invalid (parent must be named key for __kind__) - received <key>"`
  - `"Key filter on __property__ is invalid (must be named key for __property__) - received <key>"`
- **An ancestor is allowed** (`:114-137`). This applies only when there are no key filters:
  - A `__kind__:K` ancestor gives all properties of K.
  - A `__kind__:K/__property__:P` ancestor gives exactly that property.
  - The ancestor predicate in the main pipeline then applies as well.
- The property range check is applied only when the kind equals the start or end kind
  (`:168-218`).
- Hidden special properties such as `__scatter__` are skipped.
- Properties are collected from `entity.getPropertyList()`: indexed values only, including the
  computed `a.b` / `a.__key__` properties flattened from indexed embedded entities. Properties that
  only ever appear unindexed are **not listed**.
- Entity shape: key path `[__kind__: name=K, __property__: name=P]`, `name_space` only if non-empty,
  `entity_group = [__kind__ element]`. Unless the query is keys-only, the entity has one property
  `property_representation` with `multiple=true` for each distinct representation, sorted
  alphabetically (`TreeMultimap`). Each value is a `string_value` from
  `PropertyType.name()`: `NULL`, `INT64`, `BOOLEAN`, `STRING`, `DOUBLE`, `POINT`, `USER`, `REFERENCE`
  (`dev/PropertyType.java:40-103`).
- Timestamps are reported as `INT64`, and blobs as `STRING`.
- The entities are built in extent and TreeMultimap order and then sorted by key.

### `__namespace__` (`dev/NamespacePseudoKind.java`)
- An ancestor gives `"ancestor queries on __namespace__ not allowed"`.
- A key filter must be a root `__namespace__` key. A named key means that namespace, and id **1** means the
  empty namespace. Failures give:
  - `"Key filter on __namespace__ is invalid (key has parent) - received <key>"`
  - `"... (must be a key for __namespace__) - received <key>"`
  - `"... (key must be a name or the number 1) - received <key>"`
- Returns the distinct namespaces of all stored entities across all kinds that fall in the range, compared
  with `String.compareTo`, where `""` sorts lowest.
- Entity shape: path `[__namespace__: name=<ns>]`, or `id=1` for the empty namespace. The key carries
  the **query's** namespace if that is non-empty. `entity_group` = the path element. No properties.
- Sorting by key puts id 1 (the empty namespace) first, because ids sort before names.

### `__entity_group__` (`dev/EntityGroupPseudoKind.java`)
- It is only registered when the datastore is not spanner-backed.
- Any query fails with BAD_REQUEST `"queries not supported on __entity_group__"`.
- `get(root/__entity_group__:1)` returns an entity with `__version__` (int64) set to the group version + startup
  time in µs. Any other key shape returns null.

---

## 5. Unindexed, missing and special properties

- Filters, sorts, projections, group-by and `__property__` read only `EntityProto.property`. Values in
  `raw_property`, meaning unindexed values, Text and Blob, are **invisible**
  (`api/DataTypeTranslator.java:400-416`).
- If an entity has no indexed value for a filtered **or sorted** property, it is **excluded**. This
  includes order-only properties (§2.1) and projected or group-by properties (EXISTS).
- An explicit null value is indexed and is present: it sorts first.
- When an entity mixes indexed and unindexed values for one property, only the indexed ones count.
- Embedded entities stored with meaning `ENTITY_PROTO` in `property` are moved to raw (stashed). Their
  leaf values are flattened into computed indexed properties `parent.child`, `multiple` is set if any
  level was repeated, and `parent.__key__` is added if the embedded entity has a key
  (`core/proto2/EntityStorageConversions.java:61-69,194-280`). Those dotted names can be filtered,
  sorted and projected.
- **Empty lists**: the emulator calls `preprocessIndexesWithoutEmptyListSupport`, so an indexed
  `EMPTY_LIST` property is **not** stashed (`EntityStorageConversions:53-69`, `LDS:778-781`). It
  stays in `property` with an empty value, which compares as **null**. **[unsure; depends on how the v1
  layer encodes `[]`]**
- `__scatter__` is stored on roughly half of entities when it is enabled (`LDS:708-714,3083-3127`). It can
  be queried, but it is stripped from results.
- `__key__` always exists.

---

## 6. Composite index manager

Code: `dev/LocalCompositeIndexManager.java:771-959`, `api/CompositeIndexManager.java`.

- **Results are never changed by indexes.** The query runs over all entities whatever indexes exist.
- `processQuery` records the query shape in the query history. It only does something the **first time a shape is
  seen** (`:785-805`):
  - If `datastore-indexes` has `autoGenerate="false"`, it calls `verifyIndexExistsForQuery`. It
    computes the needed index. If that index is not in the file and no combination of existing indexes
    covers it (`minimumCompositeIndexForQuery`), it throws `ApplicationException(NEED_INDEX,
    "This query requires a composite index that is not defined. You must update <file> or enable
    autoGenerate to have it automatically added." + optional "\n\nThe minimum required index is:\n<xml>")`
    (`:898-922,309-321`).
  - Otherwise it writes `datastore-indexes-auto.xml`. Nothing is thrown.
- Because the check runs only the first time, running the same query shape again does **not** check
  again.
- The error is raised after the query was computed, but no results are returned. The Cloud
  emulator's `--require_indexes` flag is **[outside repo]**. This mechanism is presumably what implements it.
- The needed index (`compositeIndexForQuery`, `api/CompositeIndexManager.java:91-144`) is computed on
  the normalized query after removing a trailing `__key__ ASC`. `__key__` filters are also removed,
  unless there is a non-key inequality or a `__key__ DESC` order. Both removals are skipped
  entirely when EXISTS filters are present (`api/IndexComponentsOnlyQuery.java:62-116`).
  No index is needed in these cases:
  - no filters and no orders;
  - a kind, only equality filters (at least one), no key property, no orders (merge join);
  - a kind, no ancestor, at most 1 index property, not geo, and either no key property or the single property is ASC.

  Otherwise the index is: the equality properties sorted ASC by name, then the inequality property
  (when there are no orders), then the orders, with EXISTS and group-by components; ancestor = whether the query has an ancestor.
- `queryIndexList` fills `QueryResult.index` with that index. It has no effect on the results.
