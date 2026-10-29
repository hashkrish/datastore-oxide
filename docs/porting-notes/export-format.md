# Java emulator Admin v1 ExportEntities / ImportEntities: spec for the port

Evidence: black-box probes against the running emulator (agent projects named below) and the recorded
project cases `tests/conformance/projects/{export,import}_*`. The port is
`crates/oxide-api/src/{export,v3,admin}.rs`. The entity format is the App Engine `EntityProto`
(`storage_onestore_v3`).

**Resolved after this spec was written** (see the project cases): export strips `__scatter__` and
the embedded-entity index data from the stored entities in place (queries on `ent.x` stop matching
the exported entity until it is rewritten; the stripped form is what gets persisted), and after a
restart the kind order is *kept* within each bucket of the model below, while kinds created later
still go first in their bucket.

Labels used below: **[P]** = confirmed by a probe, **[U]** = unverified, with a probe to run.
Unlabeled items were confirmed by the recorded project cases or follow from them.

---

## 0. Overview

- **Export**: fails with 400 `The output_url_prefix: %s already exists` if the prefix path exists. Otherwise, for each
  entity set (§1.3), it collects the matching entities and writes an export directory (§1.6); an I/O failure gives
  500 `Failed to export entities.` (INTERNAL). The response is a finished operation: `metadata` =
  `ExportEntitiesMetadata{common{startTime, endTime, EXPORT_ENTITIES, SUCCESSFUL}, entity_filter, output_url_prefix}`
  (both echoed from the request) and `response` = `ExportEntitiesResponse{output_url}` (§1.8).
- **Import**: reads and parses **every** file first, then writes the entities in batches of 5000 (§6.4). The response is
  a finished operation named `projects/<project>/operations/<32 hex digits>`, with `response` = `google.protobuf.Empty`
  and `metadata` = `ImportEntitiesMetadata{common{SUCCESSFUL, IMPORT_ENTITIES, startTime, endTime}, entity_filter,
  input_url}`.

Errors: BAD_REQUEST failures give INVALID_ARGUMENT (HTTP 400) and internal failures INTERNAL (HTTP 500). Unexpected
failures give gRPC UNKNOWN, which REST shows as `{"error":{"code":500,"status":"UNKNOWN"}}` with **no message** [P].

---

## 1. Directory and file naming, prefix handling, errors

### 1.1 Export name (timestamp directory)
The name is the current time in ISO-8601 **UTC** with exactly 3 millisecond digits, with `.` replaced by `_` and the
`Z` dropped: `2026-10-27T11:18:03_103`, where `_103` = **milliseconds, UTC**, truncated rather than rounded. This clock
is read separately from `common.startTime`/`endTime`, which have nanosecond resolution.

### 1.2 Layout
```
<prefix>/<NAME>/<NAME>.overall_export_metadata                       LevelDB log: "3", OverallExportMetadata
<prefix>/<NAME>/<NSDIR>/<KINDDIR>/<NSDIR>_<KINDDIR>.export_metadata  bare proto: BackupProtos.Backup (sec. 4)
<prefix>/<NAME>/<NSDIR>/<KINDDIR>/output-<i>                         LevelDB log of v3 EntityProto, i = 0,1,...
```
- NSDIR: ALL_NAMESPACES gives `all_namespaces`, DEFAULT_NAMESPACE gives `default_namespace`, and ONE_NAMESPACE
  gives `namespace_<ns>`, with the raw namespace (no escaping).
- KINDDIR: ALL_KINDS gives `all_kinds` and ONE_KIND gives `kind_<kind>`, with the raw kind (no escaping, so a `/` in a
  kind would create subdirectories).

### 1.3 EntityFilter to entity sets
- Namespace list: `namespace_ids` empty gives `[{ns_type: ALL_NAMESPACES}]`. Otherwise one entry per id, in
  request order: `""` gives `DEFAULT_NAMESPACE`, and anything else gives `ONE_NAMESPACE` with that namespace.
  There is no dedup.
- Kind list: `kinds` empty gives `[{kind_type: ALL_KINDS}]`. Otherwise one `ONE_KIND` entry per kind, in request
  order, with no dedup and no validation (`""` and `__kind__` are accepted and simply match nothing [P, golden]).
- The result is the cartesian product **namespace-major**: `for ns in nsList: for k in kindList`.
  **All four EntitySetSpec fields are present** on the wire (`namespace=""` and `kind=""` included):
  bytes `08 <kt> 12 <len> kind 18 <nt> 22 <len> ns`.
  Enums: KindType ONE_KIND=1, ALL_KINDS=2. NamespaceType DEFAULT=1, ONE=2, ALL=3.

### 1.4 Which entities each spec gets
- The project is `req.project_id` verbatim, and it covers **all databases** of that project.
- A ONE_KIND spec matches entities whose key's **last** path element has that kind; a namespace spec matches the key's
  namespace exactly (DEFAULT_NAMESPACE matches `""`); ALL matches anything.
  **database_id is not filtered**: entities from every database of the project are exported, and their keys
  keep `database_id` [P: agent1 export contained `D:x` with `database_id: "d2"`].
- Each exported entity is in its "user-visible" v3 form: no `__scatter__` or other special properties, no computed
  properties, and stashed properties restored. This also changes the stored entity (see "Resolved" above).

### 1.5 Entity order within an output file
Entities are grouped by kind, kinds in **hash-table order of the kind string** (below), and within a kind in
**insertion order**:
- Overwriting an existing key keeps its position. Delete followed by re-insert moves the key to the end [P agent2:
  `T t1, t2(ns n1), t3, t0` after deleting and re-adding t3].
- Namespaces and databases share one list per kind, so they interleave in insertion order [P].
- Within one commit, mutations are applied in request order [P agent3: `b2(zz), a1, c3(aa)`].

Kind order [P]: export order fits a model of kinds kept in a hash table and visited in ascending bucket
order, where a kind's bucket is `String.hashCode()` spread as `h ^ (h>>>16)` and masked by the capacity
minus 1; the capacity starts at 16 and doubles when the number of kinds exceeds 0.75 × capacity. **Within one bucket, the kind created most recently comes first** (agent4/7/8: created
`A,Q,a`, which all hash to bucket 1, and exported `a,Q,A`; created `T` then `D`, both bucket 4, and exported `D,T`).
Port it as observed. The kind table never shrinks: a kind whose entities were all deleted keeps its slot.
- [U] Order across a resize (more than 12 kinds) when kinds collide. Probe: create 13+ kinds where two pairs
  collide mod 16 but not mod 32 (for example `A`(65) and `Q`(81), with 11 fillers), export, then decode with
  `tools/keys.py`.
- [U] Whether kinds that exist only in a non-default database take part in the same ordering. Probe: create a
  kind only in database `d1`, then export the default database and `d1` and compare the order with §1.5.

### 1.6 Multiple specs: one export directory per spec, cumulative
An export directory is written **once per spec**, each time with a **fresh name** (§1.1) and containing **all specs
processed so far**. Consequences:
- `kinds:[A,B]` produces dir1 = {A} and then dir2 = {A,B}. If both calls fall in the same millisecond the names
  collide: the existing directory is reused, files are overwritten, and you see one dir with {A,B}. Otherwise you see several
  dirs and only the **last** one is complete [P: agent2 `kinds:[T,D]` gave one dir (same ms). `kinds:[T,Nope,D] ×
  ns:["",n1]` gave dirs `_292`={T}, `_293`={T,D,n1/T}, `_294`={T,D,n1/T}].
- **The goldens in `export_filters.golden` look only at the FIRST matching dir.** For `kinds:["L","K"]` the golden
  shows only the L pointer because `PROTO k2/*/…` picked the first directory.
- A spec with zero entities gets **no directory, no pointer and no files**. The overall pointer order is the spec
  order (namespace-major), skipping empty specs.
- Duplicate specs (`kinds:["D","D"]`) share one pointer, and their entities are **appended twice**:
  `num_entities: 2` and two copies in `output-0` [P agent2/dup].
- Empty project or no matches: one dir containing only `NAME.overall_export_metadata`, which holds "3" and a
  **zero-length** OverallExportMetadata record (`052b2843 0000 01`). There is no `all_namespaces` dir [P agent5].
- To be byte-identical apart from timestamps, emulate the loop: N iterations, each with the cumulative set and
  a newly read millisecond clock.

### 1.7 Shards
Each spec's entities go into files `output-0`, `output-1`, and so on, with up to 5000 entities per file [P].
n = 0 cannot happen (see 1.6). [U] Shard sizes above 1,250,000 entities (250 files) were not probed.

### 1.8 output_url_prefix interpretation
- The prefix is a plain file path: repeated `/` collapse and a trailing `/` is stripped. It does **not** resolve `.` or `..`, and **there is no scheme handling**:
  `gs://bucket/x` becomes the relative path `gs:/bucket/x`, and `file:///w/x` becomes `file:/w/x`.
- A relative path is resolved against the emulator's cwd, which is `/` in the container. So `exp` produces
  `outputUrl` `/exp`, and `gs://bucket/x` produces `/gs:/bucket/x`.
- Empty or missing prefix: the export is written at the filesystem root (`/NAME`), and `outputUrl` is the working
  directory (`"/"` in the container). It can be repeated without error.
- An existing prefix path returns **400** `The output_url_prefix: <raw prefix> already exists`, where
  the message echoes the raw string, including a trailing `/` [P]. This applies whether the existing path is
  a directory or a file. A nonexistent path is created, parents included [P].
- Trailing slash rule: `/a/b/` and `/a/b//` are the same path as `/a/b`. They fail only if `/a/b` exists. On success,
  `outputUrl` has no trailing slash [P agent2/ts].
- A prefix under an existing **file** gives **500** `Failed to export entities.` (status INTERNAL).
- A prefix with `..` through a missing directory (`/w/x/../dd`): the directories `/w/dd/NAME/...` are created, but
  writing through the non-canonical path fails, so you get 500 and the directories are left behind [P].
- Failures are not atomic: files written for earlier specs stay on disk.
- `ExportEntitiesMetadata.output_url_prefix` echoes the raw request string. `entity_filter` echoes the request
  verbatim, duplicates included. `labels` is ignored (it is not copied).
- `ExportEntitiesResponse.output_url` = the absolute form of the prefix path. It is **not** the path of the
  overall_export_metadata file. Real Datastore returns that file path; this emulator does not.

---

## 2. LevelDB log format

Constants: BLOCK_SIZE = 32768 and HEADER_LENGTH = 7. Header (little-endian): `u32 masked_crc | u16 length | u8 type`.
Types: NONE=0, FULL=1, FIRST=2, MIDDLE=3, LAST=4. Other type bytes are invalid (see the reader).

CRC = `crc32c(type_byte || payload)` (standard Castagnoli). It is masked as in LevelDB:
`((c >>> 15) | (c << 17)) + 0xA282EAD8` (mod 2^32).

### Writer
For each record (an empty payload still produces one physical record):
1. `left` = free bytes in the current 32 KiB block buffer. `avail = left - 7`.
   `frag = min(avail, remaining)`. The type is FULL or FIRST on the first fragment (FIRST if
   `remaining > avail`) and LAST or MIDDLE afterwards. With exactly 7 bytes left this produces a
   **zero-length FIRST** (or a zero-length FULL for an empty payload), as in LevelDB.
2. Write the header and the fragment.
3. If `0 < left_after < 7`, write `left_after` **zero bytes** (trailer). If the block is full, flush it.
The partial last block is flushed at the end. **The file is not padded to a block boundary.** `num_bytes` (§5) counts
`7 + frag` per fragment plus trailer zeros, so it equals the size of the shard file [P: 137 bytes and 0x89].

### Reader (what import accepts)
With `blockLeft = 32768 - pos%32768`:
- If `blockLeft < 7`, the reader treats the rest as a trailer: it reads the rest of the block and **every byte must be 0**
  (`Found a non-zero byte: <b> before the end of the block <i> bytes after encountering a RecordType of NONE`).
  A short read at EOF is fine. A short read before EOF gives `There are <n> but <m> were read.`
- End of file exactly at a header boundary ends the stream cleanly. Fewer than 7 header bytes gives
  `Record header length not valid`.
- A length larger than `blockLeft` measured *before* the header gives `Record length is too large:<len>`. A length
  that fits in `blockLeft` but not in `blockLeft - 7` is not rejected by this check.
- A short payload read gives `Invalid record`.
- CRC: header crc == 0, type == 0 and length == 0 is accepted without checking. Otherwise
  `unmask(crc) == crc32c(type_byte || payload)` must hold, or you get `Checksum doesn't validate.`
  A record with an unknown type byte and a CRC computed over its real type byte also gives
  `Checksum doesn't validate.`
- A logical record is assembled as FULL, or FIRST, MIDDLE*, LAST. Errors: FULL or FIRST while inside a
  fragment, or MIDDLE or LAST outside one, give `Invalid RecordType: <ENUM NAME>` (for example
  `Invalid RecordType: FULL`). UNKNOWN gives `Invalid RecordType: -1`.
- A NONE record (type 0) means: check that the rest of the block is all zeros, skip to the next block, and reset
  the fragment state to NONE.
- **EOF in the middle of a fragmented record ends the stream silently.** The partial record is dropped
  without an error.
- The final file does not need block padding. All errors above are 400 INVALID_ARGUMENT with the message as given.

---

## 3. v3 EntityProto bytes written to `output-N`

### 3.1 Encoding
Each record is a serialized `storage_onestore_v3.EntityProto`:
- Top-level fields are emitted in **field-number order**: `kind(4)`, `kind_uri(5)`, `key(13)`, `property(14)*`,
  `raw_property(15)*`, `entity_group(16)`, `owner(17)`. Only key, property, raw_property and entity_group occur.
- Property = `meaning(1)?` `meaning_uri(2)?` `name(3)` `multiple(4)` `value(5)` (`stashed(6)` and `computed(7)` never
  appear after postprocess). `multiple` is always present.
- Reference = `app(13)` `path(14)` `name_space(20)?` `database_id(23)?`. name_space is absent for `""` and
  database_id is absent for the default database.
- Path = repeated group `Element(1)` {`type(2)` `id(3)?` `name(4)?`}, encoded as group start `0b` and end `0c`.
- PropertyValue = `int64Value(1)` `booleanValue(2)` `stringValue(3)` `doubleValue(4)` `PointValue(5){x6 y7}`
  `UserValue(8)` `ReferenceValue(12){app13 PathElement14*{type15 id16 name17} name_space20 database_id23}`.
- **Embedded entity bytes** (the ENTITY_PROTO stringValue) are the bytes stored at commit time, copied as opaque
  bytes. They are not re-encoded on export. The observed order is the same field-number order.

### 3.2 Property order [P]
`property` and `raw_property` each keep the **order of the v1 `properties` map as received**: JSON object
order over REST, and wire order of the map entries over gRPC. Nothing is sorted (agent1: `z_null, b_bool, i_int, …`
in request order). An array's values are split by indexedness: indexed values go to `property` and
`exclude_from_indexes` values go to `raw_property`, each list keeping the relative order of its values.
The `raw_property` list follows all of `property`.

### 3.3 v1 value to v3 mapping [P agent1, export_basic.golden]
| v1 value | list (indexed / excluded) | meaning | PropertyValue |
|---|---|---|---|
| null | property / raw | none | empty value message (`2a 00`) |
| boolean | " | none | `booleanValue` (field 2) |
| integer | " | none | `int64Value`, two's-complement varint (10 bytes if negative) |
| double | " | none | `doubleValue` fixed64. **-0.0 is written as +0.0** (`0000…00`). NaN = `000000000000f87f`, ±Inf as normal |
| timestamp | " | `GD_WHEN`=7 | `int64Value` = **microseconds** since epoch. Nanoseconds are truncated (`.123456789` gives `…123456`). Negative values are allowed (0001-01-01 = -62135596800000000) |
| key | " | none | `ReferenceValue` group: app=project, PathElements, name_space if not empty, database_id if not default |
| string | " | none (meaning from the request is kept, for example ATOM_CATEGORY=1) | `stringValue` UTF-8 bytes. Empty string gives `1a 00` |
| blob | **indexed: `BYTESTRING`=16**, **excluded: `BLOB`=14** | | `stringValue` raw bytes |
| geoPoint | " | `GEORSS_POINT`=9 | `PointValue{x=latitude, y=longitude}`. `{}` gives 0, 0 |
| entity | " | `ENTITY_PROTO`=19 | `stringValue` = serialized EntityProto (see 3.4) |
| array (non-empty) | each value goes to its own list | per value | one Property per value, `multiple: true` |
| array (empty) | property | `EMPTY_LIST`=24 | `multiple: false`, empty value |
| integer with meaning 13 | " | GD_RATING=13 | the meaning is passed through when the type allows it (meaning 22 on an integer is rejected at commit) |

Long excluded strings (more than 1500 bytes) are plain `raw_property` stringValue entries. There is no TEXT meaning
unless one is given [P: agent1 `D:x long`].
[U] user values, meaning 15/TEXT on strings, `meaning` on arrays. Probe: commit them under agent13 and decode with
`protoc --decode=storage_onestore_v3.EntityProto`.

### 3.4 Embedded entity encoding [P]
- No key: `key{ app:"" path:{} }` and `entity_group: {}`, both **present and empty**:
  `6a 04 6a 00 72 00 … 82 01 00`.
- With a key: `key{app, path[, name_space]}`, then properties, then `entity_group` = the root element of the key
  path.
- Inner properties follow the same rules (indexed go to property(14), excluded go to raw_property(15)) and nest
  recursively. An `excludeFromIndexes` entity value goes to raw_property with ENTITY_PROTO.

### 3.5 Top level
- `key{app=project_id, path, name_space?, database_id?}`. `entity_group` = Path containing **only the root element** of
  the key (`82 01 <len> 0b 12 … 0c`).
- There are no `__entity_version__`, `__entity_create_time__`, `__entity_update_time__` or `__scatter__`
  properties.

---

## 4. Per-set `.export_metadata` = `apphosting.ext.datastore_admin.BackupProtos.Backup`
A bare proto with no framing.
```
Backup        { 1: BackupInfo backup_info; 2: repeated KindBackupInfo kind_info }
BackupInfo    { 1: string backup_name = NAME; 2: int64 start_timestamp; 3: int64 end_timestamp }   (epoch µs)
KindBackupInfo{ 1: string kind = the spec's kind ("" for all kinds);  2: repeated string file = "output-0", "output-1", ... }
```
- start_timestamp is read before the shards are written and end_timestamp after, both in epoch microseconds.
- `kind` is **always present**: `""` for ALL_KINDS (`12 0c 0a 00 12 08 output-0`).
- There is exactly one kind_info. The file names are relative to the directory of this file.
- Import ignores unknown fields in it. Read errors are passed through as 400 (sec. 6).

---

## 5. Overall metadata
LevelDB log with two records:
1. The UTF-8 string `"3"` (a 1-byte FULL record: `b86d444e 0100 01 33`).
2. `OverallExportMetadata { repeated SingleExportMetadataPointer export_pointers = 1 }`, where each pointer is
   `{ 1: EntitySetSpec (all 4 fields present); 2: export_metadata_file = path relative to <prefix>/<NAME>
   (for example "all_namespaces/kind_K/all_namespaces_kind_K.export_metadata"); 3: num_entities = number of
   entities (duplicates counted, always set); 4: num_bytes = total size of the spec's shard files on disk }`.
   Field order is 1, 2, 3, 4.

---

## 6. Import

### 6.1 inputUrl
`input_url` must be the path of the **`.overall_export_metadata` file itself**:
- A directory gives 400 `Failed to parse overall export metadata file`.
  Empty or missing inputUrl, a nonexistent file and any read error give the same message.
- No scheme handling: `gs://…` is a relative path from cwd `/`, so it is effectively always missing.
- The first record must be the string `"3"`, or you get 400 `Overall export metadata file verion not supported`
  (sic: "verion").
- **An empty file**, or a file with only the version record, gives 500 `{"code":500,"status":"UNKNOWN"}` with no message [P].
- Reader errors (sec. 2) propagate as 400 with their own messages (for example `Checksum doesn't validate.` and
  `Invalid record`).
- The pointer's `export_metadata_file` is resolved against **the parent directory of the overall file**. [U] Whether a
  path containing `\` is also tried with `/` (probe: a pointer written with `\` separators). A missing Backup file gives 400 with the raw I/O
  error message: `/work/x/nofile (No such file or directory)` [P]. A malformed Backup gives the protobuf
  parse error message (for example `Protocol message tag had invalid wire type.`) [P].
- Each `kind_info.file` is resolved against the Backup file's directory. A missing file or a record that fails to
  parse gives 400 `Failed parse entity file:<path>` with **no space after the colon** [P].
  LevelDB reader errors (sec. 2) pass through with their own messages.
- Missing required fields in records are accepted.

### 6.2 entityFilter on import
Specs are built as in 1.3.
- If the list has exactly one spec and it is ALL_KINDS/ALL_NAMESPACES (the empty filter), all pointers are returned.
- Otherwise, for each pointer in file order, find the **first** request spec `r` that can serve it. If one is found,
  mark r as matched and keep the pointer:
  - namespace: if pointer ns is ALL, r must be ALL. Otherwise r is the same (type and namespace string) or r is ALL.
  - kind: if pointer kind is ALL, r must be ALL. Otherwise r is the same (type and kind) or r is ALL.
- If some request spec was not matched, you get 400 `The requested kinds/namespaces are not available`.
  Consequences:
  - A **filtered import of an unfiltered export always fails**, because an ALL pointer only matches an ALL request.
  - Duplicate request entries always fail.
  - Every request spec must be matched by some pointer.
- The filter selects **whole pointers/files**. Individual entities are not filtered.

### 6.3 Key and project rewriting
- Top-level `key.app` is set to `req.project_id` **only if it is not empty**. namespace and **database_id are kept**,
  so an entity exported from database `d2` is imported into database `d2` of the target project.
- For each entry in **`property` (indexed) only**:
  - If meaning == ENTITY_PROTO: parse the embedded bytes, recurse, and re-serialize. An embedded key with an empty app
    stays empty. A parse failure gives 500 UNKNOWN.
  - If it has a ReferenceValue: set `app = project_id`, even if the reference pointed to another project.
- **`raw_property` (excluded) values are not rewritten**: unindexed key values and unindexed embedded entities
  keep the source project [P: `ru` and `eu` still `agent11`, `ri` and `ei` became `agent12`].

### 6.4 Write path
- Entities are written like App Engine v3 non-transactional puts, in batches of 5000.
- For each batch, **all** entities are validated first (v3 validation rules), then the writes happen. A validation error fails the whole batch, but earlier batches stay applied.
- Incomplete keys (last element with id 0 and no name, which an export never contains) get an id from the
  configured auto-id policy.
- Stored entities get `__scatter__` and computed/stashed index data as on any put. If `entity_group` is empty it is
  recomputed from the root of the key, otherwise it must have exactly one element (else 500 UNKNOWN).
- Entities are grouped by entity group, in batch order. Each group's version is bumped and a write job is scheduled
  under the emulator's job policy. This is the **same commit/version machinery as commits**: `--consistency` < 1 can delay index visibility exactly as for
  commits.
- **Existing entities are overwritten** (full replace, extra old properties disappear). Other entities are left
  alone [P: agent10, `old` property gone, `Z:z` kept at version 3].
- Version: the entity's version is the mutation timestamp of its job, from the same counter as commits. A fresh
  project gives 2 on first lookup [P agent12, golden]. Overwrites get the next numbers [P agent10: versions 4 and 5
  after commits that produced 2 and 3].
- **The id allocator is not reserved** for imported numeric ids. After importing `W:123456`, `allocateIds` still returned an unrelated scattered id [P]. Collisions with later
  auto ids are possible (as in Java).
- Partial import: if a parse error happens, nothing is written, because everything is read before the first put. A
  partial import is possible only through a failure in batch k ≥ 2 (validation or an unparseable embedded entity),
  or through a crash.
- Labels are ignored. There is no progress data (`progress_entities`/`bytes` are absent).

---

## 7. LRO surface
- Both calls are **synchronous**: the response is returned only after all work is done, with `done: true`.
- Export: **no `name`**. Fields: `metadata` (ExportEntitiesMetadata), `done`, `response` (ExportEntitiesResponse).
- Import: `name = "projects/<project>/operations/<UUID without dashes, 32 lowercase hex>"`. It is random and never
  stored. Fields: `metadata` (ImportEntitiesMetadata), `done`, `response` = `google.protobuf.Empty`.
- JSON field order on REST: `name`, `metadata`, `done`, `response` (proto field order: name=1, metadata=2,
  done=3, response=5). `common`: `startTime`, `endTime`, `operationType`, `state`. `labels` is never set.
- There is **no google.longrunning.Operations service**. REST `GET /v1/projects/p/operations`, `GET …/operations/<id>`,
  `POST …:cancel` and `DELETE …` all return **404 `Not Found`** (plain text) [P]. On gRPC the Operations service is
  not registered, so you get UNIMPLEMENTED ("unknown service") [U probe with grpcurl].
- DatastoreAdmin RPCs: ExportEntities and ImportEntities are implemented. **CreateIndex, DeleteIndex, GetIndex and
  ListIndexes** give UNIMPLEMENTED. REST returns 501 with
  `Method google.datastore.admin.v1.DatastoreAdmin/<Name> is unimplemented` [P ListIndexes, CreateIndex].
- An unknown JSON field (for example `"databaseId"`) gives 400 `Payload isn't valid for request.` [P].

## 8. Database id and Firestore mode
- The emulator's Admin requests have **no `database_id`** field, so a REST body containing it gets 400 (sec. 7).
  There is no way to choose a database.
- Export works per project id and includes every database's entities, with `database_id` in their keys. Import
  writes each entity into the database named in its key.
- [U] whether a Firestore-mode emulator (`--use_firestore_in_datastore_mode`) rejects export or import upstream.
  Probe on a scratch emulator.

## 9. Probes to run (unverified items)
1. Kind order across a kind-table resize (1.5).
2. User and TEXT meanings (3.3).
3. gRPC Operations service status (7).
4. Batch-level partial import: build a 5001-entity export whose 5001st entity has a corrupt indexed
   ENTITY_PROTO value and import it. Expect 500 UNKNOWN with the first 5000 entities written.
