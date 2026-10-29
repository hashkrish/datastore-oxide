"""Generates the index-file project cases in tests/conformance/projects (see tools/conformance/src/project.rs)."""
import json, os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tests", "conformance", "projects")
AUTO_XML = "WEB-INF/appengine-generated/datastore-indexes-auto.xml"
YAML = "WEB-INF/index.yaml"
MANUAL_XML = "WEB-INF/datastore-indexes.xml"


def eq(p, v):
    return {"propertyFilter": {"property": {"name": p}, "op": "EQUAL", "value": v}}


def ineq(p, op, v):
    return {"propertyFilter": {"property": {"name": p}, "op": op, "value": v}}


def AND(*fs):
    return {"compositeFilter": {"op": "AND", "filters": list(fs)}}


def I(v):
    return {"integerValue": str(v)}


def order(*specs):
    out = []
    for s in specs:
        desc = s.startswith("-")
        o = {"property": {"name": s.lstrip("-")}}
        if desc:
            o["direction"] = "DESCENDING"
        out.append(o)
    return out


def Q(kind="K", filter=None, orders=(), ancestor=None, projection=(), distinct=(), limit=None, offset=None, ns=None, project="p", **extra):
    q = {}
    if kind:
        q["kind"] = [{"name": kind}]
    fs = []
    if ancestor:
        fs.append({"propertyFilter": {"property": {"name": "__key__"}, "op": "HAS_ANCESTOR", "value": {"keyValue": {"path": [{"kind": ancestor[0], "name": ancestor[1]}]}}}})
    if filter:
        fs.append(filter)
    if len(fs) == 1:
        q["filter"] = fs[0]
    elif fs:
        q["filter"] = AND(*fs)
    if orders:
        q["order"] = order(*orders)
    if projection:
        q["projection"] = [{"property": {"name": p}} for p in projection]
    if distinct:
        q["distinctOn"] = [{"name": p} for p in distinct]
    if limit is not None:
        q["limit"] = limit
    if offset is not None:
        q["offset"] = offset
    q.update(extra)
    body = {"query": q}
    if ns:
        body["partitionId"] = {"namespaceId": ns}
    return f"POST /v1/projects/{project}:runQuery\n" + json.dumps(body) + "\n"


def gql(s, project="p"):
    return f"POST /v1/projects/{project}:runQuery\n" + json.dumps({"gqlQuery": {"queryString": s, "allowLiterals": True}}) + "\n"


def case(name, comment, args, steps, files=()):
    lines = [f"# {comment}", "ARGS " + args]
    for path, text in files:
        lines += [f"SETUP {path}", text.rstrip("\n"), "END"]
    lines.append("")
    lines += steps
    open(os.path.join(OUT, name + ".case"), "w").write("\n".join(lines))


MEM = "--store_on_disk=false --consistency=1.0 {dir}"
DISK = "--consistency=1.0 {dir}"

# XML format (no manual file): what gets recorded, how indexes are computed, counts and ordering.
case("index_xml_auto", "XML auto file: index computation, history counts and ordering, reset.", MEM, [
    "LS WEB-INF",
    Q(),  # kind only: no index, but the file is still written
    f"FILE {AUTO_XML}",
    Q(orders=["a", "-b"]),
    f"FILE {AUTO_XML}",
    Q(orders=["a", "-b"]),  # same key: count rises in memory only
    f"FILE {AUTO_XML}",
    Q(orders=["a", "-b"], limit=5),  # new key (limit differs)
    f"FILE {AUTO_XML}",
    Q(filter=eq("x", I(1))),  # single equality: no index
    Q(filter=AND(eq("x", I(1)), eq("y", I(2)))),  # equalities only: no index
    Q(filter=eq("x", I(1)), orders=["a"]),
    Q(filter=eq("x", I(2)), orders=["a"]),  # same index, new key
    Q(filter=AND(eq("y", I(1)), eq("x", I(1))), orders=["-a"]),
    Q(filter=ineq("a", "GREATER_THAN", I(1))),  # single inequality: no index
    Q(filter=AND(eq("x", I(1)), ineq("a", "GREATER_THAN", I(1)))),
    Q(filter=ineq("a", "LESS_THAN", I(1)), orders=["a", "b"]),
    Q(orders=["a"]),  # one order: no index
    Q(orders=["-a"]),
    Q(orders=["a", "__key__"]),  # trailing __key__ asc removed
    Q(orders=["a", "-__key__"]),
    Q(orders=["-__key__"]),
    Q(filter=ineq("__key__", "GREATER_THAN", {"keyValue": {"path": [{"kind": "K", "name": "a"}]}})),
    Q(filter=ineq("__key__", "GREATER_THAN", {"keyValue": {"path": [{"kind": "K", "name": "a"}]}}), orders=["-__key__"]),
    Q(ancestor=("P", "a")),
    Q(ancestor=("P", "a"), filter=eq("x", I(1))),
    Q(ancestor=("P", "a"), orders=["a"]),
    Q(ancestor=("P", "a"), filter=AND(eq("x", I(1)), eq("y", I(1)))),
    Q(projection=["a"]),
    Q(projection=["b", "a"]),
    Q(projection=["a", "b"], distinct=["a"]),
    Q(projection=["__key__"]),
    Q(filter=eq("x", I(1)), projection=["a"]),
    Q(kind=None),
    Q(kind=None, filter=ineq("__key__", "GREATER_THAN", {"keyValue": {"path": [{"kind": "K", "name": "a"}]}})),
    Q(kind="__kind__"),
    Q(kind="__property__", orders=["-__key__"]),
    Q(orders=["a", "-b"], ns="n1"),
    Q(orders=["a", "-b"], project="p2"),
    Q(kind="K2", orders=["c", "d"]),
    Q(orders=["a", "-b"], projection=["__key__"]),
    gql("SELECT * FROM G WHERE x = 1 ORDER BY y DESC"),
    gql("SELECT * FROM G WHERE x = 2 ORDER BY y DESC"),
    f"FILE {AUTO_XML}",
    "POST /reset\n",
    Q(kind="K3", orders=["a", "b"]),
    f"FILE {AUTO_XML}",
])

# Validation errors and transactional queries.
case("index_xml_misc", "XML auto file: invalid queries are not recorded; transactions; paging.", MEM, [
    Q(filter=AND(ineq("a", "GREATER_THAN", I(1)), ineq("b", "GREATER_THAN", I(1)))),  # invalid
    "LS WEB-INF",
    "POST /v1/projects/p:beginTransaction\n{}\n> t = transaction\n",
    "POST /v1/projects/p:runQuery\n" + json.dumps({"readOptions": {"transaction": "{{t}}"}, "query": {"kind": [{"name": "K"}], "filter": {"propertyFilter": {"property": {"name": "__key__"}, "op": "HAS_ANCESTOR", "value": {"keyValue": {"path": [{"kind": "P", "name": "a"}]}}}}, "order": [{"property": {"name": "z"}}]}}) + "\n",
    f"FILE {AUTO_XML}",
    "POST /v1/projects/p:commit\n" + json.dumps({"mode": "NON_TRANSACTIONAL", "mutations": [{"upsert": {"key": {"path": [{"kind": "K", "name": str(i)}]}, "properties": {"a": I(i), "b": I(i % 3)}}} for i in range(5)]}) + "\n",
    Q(orders=["a", "b"], limit=2) + "> c = batch.endCursor\n",
    "POST /v1/projects/p:runQuery\n" + json.dumps({"query": {"kind": [{"name": "K"}], "order": order("a", "b"), "limit": 2, "startCursor": "{{c}}"}}) + "\n",
    "POST /v1/projects/p:runQuery\n" + json.dumps({"query": {"kind": [{"name": "K"}], "order": order("a", "b"), "limit": 2, "startCursor": "{{c}}"}}) + "\n",
    Q(orders=["a", "b"], offset=1),
    f"FILE {AUTO_XML}",
])

MANUAL_INDEXES = """<?xml version="1.0" encoding="utf-8"?>
<datastore-indexes autoGenerate="{auto}">
  <datastore-index kind="K" ancestor="false">
    <property name="a" direction="asc"/>
    <property name="b" direction="desc"/>
  </datastore-index>
  <datastore-index kind="K" ancestor="false" source="manual">
    <property name="x" direction="asc"/>
    <property name="c" direction="asc"/>
  </datastore-index>
  <datastore-index kind="K" ancestor="false">
    <property name="y" direction="asc"/>
    <property name="c" direction="asc"/>
  </datastore-index>
  <datastore-index kind="K" ancestor="false" source="auto">
    <property name="old" direction="asc"/>
    <property name="older" direction="desc"/>
  </datastore-index>
  <datastore-index kind="N">
    <property name="a"/>
    <property name="b"/>
  </datastore-index>
</datastore-indexes>
"""

case("index_xml_manual", "XML with a manual datastore-indexes.xml (autoGenerate=true).", MEM, [
    Q(orders=["a", "-b"]),  # covered by a manual index
    f"FILE {AUTO_XML}",
    Q(orders=["c", "d"]),
    Q(kind="N", orders=["a", "b"]),  # manual index without directions
    f"FILE {AUTO_XML}",
], files=[(MANUAL_XML, MANUAL_INDEXES.format(auto="true"))])

case("index_xml_disabled", "XML autoGenerate=false: missing indexes fail on a query's first run only.", MEM, [
    Q(orders=["a", "-b"]),
    Q(orders=["c", "d"]),
    Q(orders=["c", "d"]),  # same key: not verified again
    Q(orders=["c", "d"], limit=1),
    Q(filter=AND(eq("x", I(1)), eq("y", I(1))), orders=["c"]),  # merge join of manual indexes
    Q(filter=AND(eq("x", I(1)), eq("z", I(1))), orders=["c"]),  # partially covered
    Q(filter=AND(eq("z", I(1)), eq("w", I(1))), orders=["c"]),
    Q(ancestor=("P", "a"), orders=["c"]),
    Q(orders=["old", "-older"]),
    Q(kind="N", orders=["a", "b"]),
    "LS WEB-INF",
], files=[(MANUAL_XML, MANUAL_INDEXES.format(auto="false"))])

case("index_xml_malformed", "A malformed datastore-indexes.xml.", MEM, [
    Q(),
    Q(orders=["a", "b"]),
], files=[(MANUAL_XML, "<datastore-indexes>\n  <datastore-index kind=\"K\">\n")])

case("index_xml_no_autogen_attr", "datastore-indexes.xml without autoGenerate.", MEM, [
    Q(orders=["a", "b"]),
], files=[(MANUAL_XML, "<datastore-indexes>\n</datastore-indexes>\n")])

# YAML format (store on disk, no XML files).
YAML_FILE = """indexes:
- kind: K
  properties:
  - name: a
  - name: b
    direction: desc

- kind: "K"
  ancestor: yes
  properties:
  - name: x
  - name: c

# AUTOGENERATED

# old comment
- kind: K
  properties:
  - name: old
  - name: older
    direction: desc
"""

case("index_yaml_default", "YAML: a fresh WEB-INF/index.yaml from `create`.", DISK, [
    "LS WEB-INF",
    Q(),
    f"FILE {YAML}",
    Q(orders=["a", "-b"]),
    Q(filter=eq("x", I(1)), orders=["a"]),
    Q(ancestor=("P", "a"), orders=["a"]),
    Q(kind="__property__", orders=["-__key__"]),
    Q(projection=["a", "b"]),
    f"FILE {YAML}",
    "LS WEB-INF",
    "LS WEB-INF/appengine-generated",
], files=[(YAML, "indexes:\n# AUTOGENERATED\n")])

case("index_yaml_manual", "YAML with --regenerate_indexes=false: manual indexes above the marker, old generated ones carried over.", "--regenerate_indexes=false " + DISK, [
    Q(orders=["a", "-b"]),
    f"FILE {YAML}",
    Q(orders=["c", "d"]),
    Q(orders=["old", "-older"]),
    f"FILE {YAML}",
], files=[(YAML, YAML_FILE)])

case("index_yaml_regenerate", "YAML by default (--regenerate_indexes=true) drops old generated indexes.", DISK, [
    Q(orders=["c", "d"]),
    f"FILE {YAML}",
], files=[(YAML, YAML_FILE)])

case("index_yaml_disabled", "YAML without the AUTOGENERATED marker: missing indexes fail.", DISK, [
    Q(orders=["a", "-b"]),
    Q(orders=["c", "d"]),
    Q(orders=["c", "d"]),
    Q(ancestor=("P", "a"), orders=["c"]),
    Q(ancestor=("P", "a"), filter=eq("x", I(1)), orders=["c"]),
    Q(filter=eq("x", I(1)), orders=["c"]),
    f"FILE {YAML}",
], files=[(YAML, YAML_FILE.split("# AUTOGENERATED")[0])])

case("index_require", "--require_indexes checks every run; the verified set is loaded once.", "--require_indexes " + DISK, [
    Q(orders=["a", "-b"]),
    Q(orders=["c", "d"]),
    Q(orders=["c", "d"]),
    Q(orders=["e", "f"]),
    f"FILE {YAML}",
    Q(orders=["c", "d"]),
    Q(),
    Q(orders=["old", "-older"]),
    "RESTART\n",
    Q(orders=["c", "d"]),
    Q(orders=["e", "f"]),
    f"FILE {YAML}",
], files=[(YAML, YAML_FILE)])

case("index_file_flag", "--index_file creates and maintains the named YAML file.", "--index_file={dir}/custom.yaml " + MEM, [
    f"FILE custom.yaml",
    Q(orders=["a", "-b"]),
    f"FILE custom.yaml",
    "LS .",
])

case("index_no_store", "--store_index_configuration_on_disk=false never writes the file.", "--store_index_configuration_on_disk=false " + DISK, [
    Q(orders=["a", "-b"]),
    f"FILE {YAML}",
], files=[(YAML, "indexes:\n# AUTOGENERATED\n")])

case("index_no_store_disabled", "--store_index_configuration_on_disk=false still verifies a file without the marker.", "--store_index_configuration_on_disk=false " + DISK, [
    Q(orders=["c", "d"]),
], files=[(YAML, YAML_FILE.split("# AUTOGENERATED")[0])])
print("ok")

# ---------------------------------------------------------------------------------------------
# Persistence (store on disk): what survives a restart, and when data is written.
# ---------------------------------------------------------------------------------------------
def first_commit_body(scenario):
    text = open(os.path.join(OUT, "..", "scenarios", scenario + ".scn")).read()
    body = text.split("POST /v1/projects/p:commit\n", 1)[1].split("\n\n", 1)[0]
    return body


def commit(mutations, project="p", mode="NON_TRANSACTIONAL", txn=None):
    body = {"mode": mode, "mutations": mutations}
    if txn:
        body["transaction"] = txn
    return f"POST /v1/projects/{project}:commit\n" + json.dumps(body) + "\n"


def key(*path, ns=None, db=None):
    k = {"path": [{"kind": path[i], **({"name": path[i + 1]} if isinstance(path[i + 1], str) else {"id": str(path[i + 1])} if path[i + 1] is not None else {})} for i in range(0, len(path), 2)]}
    part = {}
    if ns:
        part["namespaceId"] = ns
    if db:
        part["databaseId"] = db
    if part:
        k["partitionId"] = part
    return k


def upsert(k, **props):
    return {"upsert": {"key": k, "properties": {n: I(v) if isinstance(v, int) else {"stringValue": v} for n, v in props.items()}}}


def lookup(*keys, project="p", db=None, read_options=None):
    body = {"keys": list(keys)}
    if db:
        body["databaseId"] = db
    if read_options:
        body["readOptions"] = read_options
    return f"POST /v1/projects/{project}:lookup\n" + json.dumps(body) + "\n"


def allocate(n, project="p"):
    return f"POST /v1/projects/{project}:allocateIds\n" + json.dumps({"keys": [key("K", None)] * n}) + "\n"


ALL_KEYS = [key("T", "all"), key("K", "a"), key("K", "a", "C", "b"), key("K", "n", ns="ns1")]

case("persist_restart", "Store on disk: entities, versions, commit timestamps and id counters survive a clean restart.", DISK, [
    "POST /v1/projects/p:commit\n" + first_commit_body("value_types") + "\n",
    commit([upsert(key("K", "a"), v=1), upsert(key("K", "a", "C", "b"), v=2), upsert(key("K", "n", ns="ns1"), v=3)]),
    commit([upsert(key("K", "a"), v=4)]),
    commit([{"insert": {"key": key("K", None), "properties": {"v": I(5)}}}]),
    commit([upsert(key("K", "q"), v=6)], project="p2"),
    commit([upsert(key("K", "d"), v=7)]).replace('"mode"', '"databaseId": "db1", "mode"'),
    commit([{"delete": key("K", "gone")}]),
    allocate(2),
    "POST /v1/projects/p:beginTransaction\n{}\n> t = transaction\n",
    lookup(*ALL_KEYS),
    Q(),
    lookup(key("K", "d")),
    "POST /persist\n",
    "RESTART\n",
    lookup(*ALL_KEYS),
    lookup(key("K", "q"), project="p2"),
    lookup(key("K", "d"), db="db1"),
    Q(),
    Q(ns="ns1"),
    Q(kind="__namespace__"),
    Q(kind="__kind__"),
    Q(kind="__property__"),
    commit([upsert(key("K", "a"), v=8)]),
    commit([{"insert": {"key": key("K", None), "properties": {"v": I(9)}}}]),
    allocate(1),
    lookup(key("K", "a"), read_options={"transaction": "{{t}}"}),
    "POST /v1/projects/p:rollback\n{\"transaction\": \"{{t}}\"}\n",
    "POST /reset\n",
    "POST /persist\n",
])

case("persist_unapplied", "Unapplied jobs are rolled forward at shutdown (consistency 0: nothing applies on its own).", "--consistency=0.0 {dir}", [
    commit([upsert(key("K", "a"), v=1), upsert(key("K", "b"), v=2)]),
    Q(),
    "RESTART\n",
    Q(),
    lookup(key("K", "a")),
])

case("persist_clean_only", "Only applied writes mark the store dirty: allocated ids alone are not saved.", DISK, [
    allocate(3),
    "RESTART\n",
    allocate(1),
    commit([{"insert": {"key": key("K", None), "properties": {}}}]),
    "RESTART\n",
    allocate(1),
    commit([{"insert": {"key": key("K", None), "properties": {}}}]),
])

case("persist_storage_file", "--storage_file names the backing store.", "--storage_file={dir}/custom.bin --consistency=1.0 {dir}", [
    "LS .",
    commit([upsert(key("K", "a"), v=1)]),
    "RESTART\n",
    lookup(key("K", "a")),
    "LS .",
    "LS WEB-INF",
])

case("persist_memory", "In memory (--store_on_disk=false) nothing survives a restart.", MEM, [
    commit([upsert(key("K", "a"), v=1)]),
    "POST /persist\n",
    "RESTART\n",
    lookup(key("K", "a")),
    allocate(1),
    "LS WEB-INF",
])
print("persist ok")

# ---------------------------------------------------------------------------------------------
# Admin v1 export / import (managed export format: LevelDB logs of v3 EntityProtos).
# ---------------------------------------------------------------------------------------------
def admin(verb, body, project="p"):
    return f"POST /v1/projects/{project}:{verb}\n" + json.dumps(body) + "\n"


def export(prefix, project="p", **extra):
    return admin("export", {"outputUrlPrefix": prefix, **extra}, project)


SEED = [
    "POST /v1/projects/p:commit\n" + first_commit_body("value_types") + "\n",
    commit([upsert(key("K", "a"), v=1, s="x"), upsert(key("K", "a", "C", "b"), v=2),
            upsert(key("L", 5), v=3), upsert(key("K", "n", ns="ns1"), v=4), upsert(key("L", "m", ns="ns2"), v=5)]),
    commit([upsert(key("K", "a"), v=6)]),
]


def dump(prefix):
    """Every file of the export under `prefix`, decoded."""
    return [
        f"LS {prefix}",
        f"LS {prefix}/*",
        f"RECORDS {prefix}/*/*.overall_export_metadata",
        f"PROTO {prefix}/*/*.overall_export_metadata#1",
    ]


def dump_set(prefix, set_dir, set_name):
    return [
        f"LS {prefix}/*/{set_dir}",
        f"PROTO {prefix}/*/{set_dir}/{set_name}.export_metadata",
        f"ENTITIES {prefix}/*/{set_dir}/output-*",
    ]


case("export_basic", "Export of everything: response, layout and file contents.", MEM, SEED + [
    export("{dir}/exp"),
    *dump("exp"),
    *dump_set("exp", "all_namespaces/all_kinds", "all_namespaces_all_kinds"),
    export("{dir}/exp/"),
    "LS exp",
    export("{dir}/empty", project="nothing"),
    *dump("empty"),
    "LS empty/*/all_namespaces/all_kinds",
])

FILTERS = [
    ("k1", {"kinds": ["K"]}),
    ("k2", {"kinds": ["L", "K"]}),
    ("n0", {"namespaceIds": [""]}),
    ("n1", {"namespaceIds": ["ns1"]}),
    ("n2", {"namespaceIds": ["", "ns2"]}),
    ("kn", {"kinds": ["K"], "namespaceIds": ["ns1"]}),
    ("kn2", {"kinds": ["K", "L"], "namespaceIds": ["", "ns1"]}),
    ("missing", {"kinds": ["Nope"]}),
]
steps = list(SEED)
for name, f in FILTERS:
    steps += [export("{dir}/" + name, entityFilter=f), f"LS {name}/<last>", f"PROTO {name}/<last>/*.overall_export_metadata#1"]
steps += [f"ENTITIES kn2/<last>/default_namespace/kind_K/output-0", f"ENTITIES kn2/<last>/namespace_ns1/kind_K/output-0"]
case("export_filters", "Export entity filters: one file set per kind/namespace selection (several specs write several directories; the last is complete).", MEM, steps)

case("export_errors", "Export request errors.", MEM, SEED[:1] + [
    admin("export", {}),
    export(""),
    export("exp"),
    export("gs://bucket/x"),
    export("file://{dir}/viafile"),
    "LS .",
    export("{dir}/no/such/dir"),
    "LS no",
    export("{dir}/afile/x"),
    export("{dir}/lab", labels={"a": "b"}),
    export("{dir}/badkind", entityFilter={"kinds": [""]}),
    export("{dir}/reserved", entityFilter={"kinds": ["__kind__"]}),
    export("{dir}/badns", entityFilter={"namespaceIds": ["__x__"]}),
    admin("export", {"outputUrlPrefix": "{dir}/x", "bogus": 1}),
    export("{dir}/p2", project="BAD"),
    "GET /v1/projects/p/operations\n",
    "GET /v1/projects/p/operations/x\n",
    "POST /v1/projects/p/operations/x:cancel\n{}\n",
    "DELETE /v1/projects/p/operations/x\n",
    "GET /v1/projects/p/indexes\n",
], files=[("afile", "not a directory\n")])

ALL = [key("T", "all"), key("K", "a"), key("K", "a", "C", "b"), key("L", 5), key("K", "n", ns="ns1"), key("L", "m", ns="ns2")]
case("import_basic", "Import of an export: versions, existing entities, other projects, filters.", MEM, SEED + [
    export("{dir}/exp"),
    "GLOB exp/*/*.overall_export_metadata\n> meta = path\n",
    "GLOB exp/*\n> exdir = path\n",
    commit([upsert(key("K", "a"), v=99), {"delete": key("L", 5)}, upsert(key("K", "extra"), v=7)]),
    admin("import", {"inputUrl": "{{meta}}"}),
    lookup(*ALL, key("K", "extra")),
    Q(),
    commit([upsert(key("K", "z"), v=1)]),
    admin("import", {"inputUrl": "{{meta}}"}, project="p2"),
    lookup(key("K", "a"), key("K", "n", ns="ns1"), project="p2"),
    admin("import", {"inputUrl": "{{meta}}", "entityFilter": {"kinds": ["L"]}}, project="p3"),
    Q(kind=None, project="p3"),
    Q(kind=None, project="p3", ns="ns2"),
    admin("import", {"inputUrl": "{{meta}}", "entityFilter": {"namespaceIds": ["ns1"]}}, project="p4"),
    Q(kind=None, project="p4", ns="ns1"),
    Q(kind=None, project="p4"),
    admin("import", {"inputUrl": "{{exdir}}"}, project="p5"),
    admin("import", {"inputUrl": "{dir}/exp"}, project="p5"),
    admin("import", {"inputUrl": "{dir}/nope.overall_export_metadata"}, project="p5"),
    admin("import", {"inputUrl": ""}, project="p5"),
    admin("import", {}, project="p5"),
    admin("import", {"inputUrl": "gs://b/x.overall_export_metadata"}, project="p5"),
    admin("import", {"inputUrl": "{dir}/afile"}, project="p5"),
    "POST /v1/projects/p6:allocateIds\n" + json.dumps({"keys": [key("K", None)]}) + "\n",
], files=[("afile", "not an export\n")])
print("export ok")

EMB = commit([{"upsert": {"key": key("E", 7), "properties": {"ent": {"entityValue": {"properties": {"x": I(1)}}}}}}])
PLAIN = commit([{"upsert": {"key": key("E", 8), "properties": {"n": I(1)}}}])
EMB_Q = Q(kind="E", filter=eq("ent.x", I(1)))
case("export_side_effects", "Export strips __scatter__ and embedded-entity index data from stored entities (survives a restart).", DISK, [
    EMB, PLAIN, EMB_Q, EMB, PLAIN,
    export("{dir}/exp"),
    EMB_Q,
    "RESTART\n",
    EMB_Q,
    EMB, PLAIN, EMB_Q,
], files=[(YAML, "indexes:\n# AUTOGENERATED\n")])

KINDS = ["A", "Q", "a", "B", "C", "D", "b", "r"]
case("export_kind_order", "Export kind order: Java hash-table order, reversed within buckets by a restart (deserialization).", DISK,
     [commit([upsert(key(k, "x"), v=1)]) for k in KINDS] + [
         export("{dir}/one"),
         "ENTITIES one/*/all_namespaces/all_kinds/output-0",
         "RESTART\n",
         export("{dir}/two"),
         "ENTITIES two/*/all_namespaces/all_kinds/output-0",
         commit([upsert(key("q", "x"), v=1)]),
         export("{dir}/three"),
         "ENTITIES three/*/all_namespaces/all_kinds/output-0",
     ], files=[(YAML, "indexes:\n# AUTOGENERATED\n")])
print("export2 ok")

# ---------------------------------------------------------------------------------------------
# --enable_entity_groups=false (both modes): reads and deletes use the full key path as the group,
# puts the root entity.
# ---------------------------------------------------------------------------------------------
def begin(name):
    return f"POST /v1/projects/p:beginTransaction\n{{}}\n> {name} = transaction\n"


def tcommit(muts, txn):
    return "POST /v1/projects/p:commit\n" + json.dumps({"mode": "TRANSACTIONAL", "transaction": "{{" + txn + "}}", "mutations": muts}) + "\n"


def tlookup(keys, txn):
    return "POST /v1/projects/p:lookup\n" + json.dumps({"keys": keys, "readOptions": {"transaction": "{{" + txn + "}}"}}) + "\n"


A, B = key("R", "r", "C", "a"), key("R", "r", "C", "b")
EG = [
    commit([upsert(A, v=1), upsert(B, v=2)]),
    # sibling write
    begin("t1"), tlookup([A], "t1"), commit([upsert(B, v=3)]), tcommit([upsert(A, v=4)], "t1"),
    # outside put of the read child: lost update
    begin("t2"), tlookup([A], "t2"), commit([upsert(A, v=5)]), tcommit([upsert(A, v=99)], "t2"),
    lookup(A),
    # outside delete of the read child
    begin("t3"), tlookup([B], "t3"), commit([{"delete": B}]), tcommit([upsert(B, v=6)], "t3"),
    # ancestor query on the root, outside put of a descendant
    begin("t4"),
    "POST /v1/projects/p:runQuery\n" + json.dumps({"query": {"kind": [{"name": "C"}], "filter": {"propertyFilter": {"property": {"name": "__key__"}, "op": "HAS_ANCESTOR", "value": {"keyValue": key("R", "r")}}}}, "readOptions": {"transaction": "{{t4}}"}}) + "\n",
    commit([upsert(A, v=7)]), tcommit([upsert(key("R", "r", "C", "z"), v=1)], "t4"),
    # lookup root, outside put of child, then lookup child
    begin("t5"), tlookup([key("R", "r")], "t5"), commit([upsert(A, v=8)]), tlookup([A], "t5"),
    "POST /v1/projects/p:rollback\n{\"transaction\": \"{{t5}}\"}\n",
    # many children of one root: puts vs lookups
    begin("t6"), tcommit([upsert(key("R", "m", "C", str(i)), v=i) for i in range(26)], "t6"),
    begin("t7"), tlookup([key("R", "m", "C", str(i)) for i in range(26)], "t7"),
    "POST /v1/projects/p:rollback\n{\"transaction\": \"{{t7}}\"}\n",
    # __entity_group__ version of a root (Datastore mode only)
    lookup(key("R", "r", "__entity_group__", 1)),
    commit([upsert(key("R", "r"), v=1)]),
    lookup(key("R", "r", "__entity_group__", 1)),
    # eventual consistency: put and delete jobs live on different groups
    "PATCH /v1/config\n" + json.dumps({"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "1", "unappliedJobPercentage": 100}}}) + "\n",
    commit([upsert(key("R", "e", "C", "x"), v=1)]),
    lookup(key("R", "e", "C", "x")),
    commit([{"delete": key("R", "e", "C", "x")}]),
    lookup(key("R", "e", "C", "x")),
    Q(kind="C", ancestor=("R", "e")),
    "PATCH /v1/config\n" + json.dumps({"jobPolicy": {"forceStrongConsistency": True}}) + "\n",
]
case("entity_groups_off", "--enable_entity_groups=false in Datastore mode.", "--testing --enable_entity_groups=false", EG)
case("entity_groups_off_firestore", "--enable_entity_groups=false in Firestore-in-Datastore mode.", "--testing --firestore_in_datastore_mode --enable_entity_groups=false", EG)
print("eg ok")
