"""Generates the App Engine v3 API server scenarios (tests/conformance/scenarios/v3_*.scn).

Steps are CallHandler calls (`V3 <Method>` with a text-format body, `V3HEX` with raw bytes);
see tools/conformance/src/v3.rs. Every scenario uses its own app ids: Java's app-id cache is
process-global and survives /reset.
"""
import json, os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tests", "conformance", "scenarios")


def q(s):
    return json.dumps(s)


class T(str):
    """Text-format source inserted as is (not quoted as a string value)."""


def el(kind, v=None, tag="Element", t="type", i="id", n="name"):
    body = f"{t}: {q(kind)}"
    if isinstance(v, int):
        body += f" {i}: {v}"
    elif v is not None:
        body += f" {n}: {q(v)}"
    return f"{tag} {{ {body} }}"


def path_of(*path):
    """(kind, id-or-name) pairs; an odd-length path ends with an incomplete element."""
    if len(path) % 2:
        path = path + (None,)
    it = iter(path)
    return list(zip(it, it))


def ref(app, *path, ns=None, db=None, field="key"):
    els = " ".join(el(k, v) for k, v in path_of(*path))
    extra = ""
    if ns is not None:
        extra += f" name_space: {q(ns)}"
    if db is not None:
        extra += f" database_id: {q(db)}"
    return f"{field} {{ app: {q(app)}{extra} path {{ {els} }} }}"


def refval(app, *path, ns=None):
    els = " ".join(el(k, v, tag="PathElement") for k, v in path_of(*path))
    extra = f" name_space: {q(ns)}" if ns is not None else ""
    return T(f"ReferenceValue {{ app: {q(app)}{extra} {els} }}")


def val(v):
    if isinstance(v, bool):
        return f"booleanValue: {str(v).lower()}"
    if isinstance(v, int):
        return f"int64Value: {v}"
    if isinstance(v, float):
        return f"doubleValue: {v!r}"
    if v is None:
        return ""
    if isinstance(v, T):
        return v
    if isinstance(v, str):
        return f"stringValue: {q(v)}"
    raise TypeError(v)


def prop(name, v, multiple=False, meaning=None, field="property", extra=""):
    m = f"meaning: {meaning} " if meaning is not None else ""
    return f"{field} {{ {m}name: {q(name)} multiple: {str(multiple).lower()} value {{ {val(v)} }}{extra} }}"


def raw(name, v, **kw):
    return prop(name, v, field="raw_property", **kw)


def ent(app, path, props=(), ns=None, db=None, eg: object = "root", field="entity", extra=""):
    pairs = path_of(*path)
    if eg == "root":
        group = el(*pairs[0]) if pairs and pairs[0][1] is not None else ""
    elif eg is None:
        group = ""
    else:
        group = " ".join(el(k, v) for k, v in path_of(*eg))
    body = ref(app, *path, ns=ns, db=db)
    body += " " + " ".join(props) if props else ""
    return f"{field} {{ {body}{extra} entity_group {{ {group} }} }}"


def txn(h="t", app="", field="transaction"):
    return f"{field} {{ handle: {{{{{h}}}}} app: {q(app)} }}"


def V3(method, body="", caps=(), opts=""):
    return ("V3", method + (" " + opts if opts else ""), body, list(caps))


def HEX(method, hexbody, opts=""):
    return ("V3HEX", method + (" " + opts if opts else ""), hexbody, [])


def REST(path, body):
    return ("POST", path, body if isinstance(body, str) else json.dumps(body), [])


def put(*ents, extra=""):
    return V3("Put", " ".join(ents) + (" " + extra if extra else ""))


def get(*refs, extra=""):
    return V3("Get", " ".join(refs) + (" " + extra if extra else ""))


def begin(app, cap="t", extra=""):
    return V3("BeginTransaction", f"app: {q(app)} {extra}".strip(), [f"{cap} = handle"])


def commit(app, h="t", opts=""):
    return V3("Commit", f"handle: {{{{{h}}}}} app: {q(app)}", opts=opts)


def rollback(app, h="t"):
    return V3("Rollback", f"handle: {{{{{h}}}}} app: {q(app)}")


def query(app, kind=None, *parts, cap=None):
    body = f"app: {q(app)}"
    if kind is not None:
        body += f" kind: {q(kind)}"
    body += "".join(" " + p for p in parts)
    return V3("RunQuery", body, [f"{cap} = cursor.cursor"] if cap else [])


def flt(op, name, v, meaning=None):
    return f"Filter {{ op: {op} {prop(name, v, meaning=meaning)} }}"


def order(name, d=None):
    return f"Order {{ property: {q(name)}" + (f" direction: {d}" if d else "") + " }"


def nxt(app, h="c", extra="", cap=None):
    return V3("Next", f"cursor {{ cursor: {{{{{h}}}}} app: {q(app)} }} {extra}".strip(),
              [f"{cap} = cursor.cursor"] if cap else [])


def write(name, comment, steps):
    lines = [f"# {comment}"]
    for kind, path, body, caps in steps:
        lines.append(f"{kind} {path}")
        if body:
            lines.append(body)
        lines += [f"> {c}" for c in caps]
        lines.append("")
    with open(os.path.join(OUT, name + ".scn"), "w") as f:
        f.write("\n".join(lines))


# ---------------------------------------------------------------------------------------------
A = "s~env"
k_a = ref(A, "K", "a")
write("v3_envelope", "v3 CallHandler envelope: method dispatch, ignored fields, parse errors", [
    put(ent(A, ["K", "a"], [prop("x", 1)])),
    # service_name and request_id are ignored.
    V3("Get", k_a, opts="service=memcache request_id=r1"),
    V3("Get", k_a, opts="service=-"),
    V3("Get", k_a, opts="service=taskqueue"),
    # Unknown / unsupported methods.
    *[V3(m) for m in ["Bogus", "get", "Touch", "DeleteCursor", "CreateIndex", "UpdateIndex",
                      "DeleteIndex", "Add", "BulkAdd", "-"]],
    # Empty requests that are valid, and ones missing required fields.
    *[V3(m) for m in ["Get", "Put", "Delete", "AllocateIds", "Commit", "Rollback",
                      "BeginTransaction", "RunQuery", "Next", "GetIndices"]],
    HEX("AddActions", ""),
    # Parse errors: truncated length, truncated varint, unmatched groups, wire types 6/7, tag 0.
    HEX("Get", "0a05"), HEX("Get", "08"), HEX("Get", "0b"), HEX("Get", "0c"), HEX("Get", "0e"),
    HEX("Get", "0f"), HEX("Get", "0001"), HEX("Put", "0a"), HEX("RunQuery", "0a03"),
    # Lenient: known field with the wrong wire type, unknown fields.
    HEX("Get", "0d00000000"), HEX("Get", "0801"), HEX("Get", "f80701"),
    # Missing required fields deep inside.
    get(f"key {{ app: {q(A)} }}"),
    get(f"key {{ path {{ {el('K', 'a')} }} }}"),
    get(f"key {{ app: {q(A)} path {{ Element {{ name: \"a\" }} }} }}"),
    put(f"entity {{ {k_a} }}"),
    put(f"entity {{ {k_a} entity_group {{ }} property {{ name: \"x\" value {{ }} }} }}"),
    put(f"entity {{ {k_a} entity_group {{ }} property {{ name: \"x\" multiple: false }} }}"),
    put(f"entity {{ {k_a} entity_group {{ }} property {{ name: \"x\" multiple: false value {{ PointValue {{ x: 1 }} }} }} }}"),
    V3("RunQuery", f"app: {q(A)} Filter {{ }}"),
    V3("RunQuery", f"app: {q(A)} Order {{ }}"),
    V3("Next", "cursor { }"),
    V3("Commit", "handle: 1"),
    V3("GetIndices", "database_id: \"x\""),
])

# ---------------------------------------------------------------------------------------------
write("v3_appids", "v3 app ids: partition prefixes, scrubbing, response app ids, v1 interop", [
    put(ent("s~ap1", ["K", "a"], [prop("x", 1)])),
    get(ref("s~ap1", "K", "a")),
    get(ref("e~ap1", "K", "a")),
    get(ref("dev~ap1", "K", "a")),
    get(ref("ap1", "K", "a")),
    REST("/v1/projects/ap1:lookup", {"keys": [{"path": [{"kind": "K", "name": "a"}]}]}),
    REST("/v1/projects/ap2:commit", {"mode": "NON_TRANSACTIONAL", "mutations": [{"upsert": {
        "key": {"path": [{"kind": "K", "name": "v1"}]},
        "properties": {"r": {"keyValue": {"path": [{"kind": "R", "id": "5"}]}},
                       "ref6": {"keyValue": {"partitionId": {"projectId": "ap6"}, "path": [{"kind": "R", "name": "z"}]}}}}}]}),
    get(ref("e~ap2", "K", "v1")),
    get(ref("s~ap2", "K", "v1")),
    *[x for app in ["example.com:foo", "s~example.com:foo", "__metadata-12_ap3__", "s~__x__",
                    "S~Foo", "a~b~c", "~ap4", "a" * 101 + "~ap4", "-ap4", "s~-ap4", "s~ap_q",
                    "s~ap3#db", "s~", "x" * 101, "s~Example.com:Foo", ""]
      for x in [put(ent(app, ["K", "a"], [prop("app", app)])), get(ref(app, "K", "a"))]],
    V3("RunQuery", f"app: {q('s~scrub')} kind: \"K\""),
    V3("RunQuery", "app: \"S~Foo\" kind: \"K\""),
    V3("RunQuery", "app: \"\" kind: \"K\""),
    REST("/v1/projects/scrubbed--oo:runQuery", {"query": {"kind": [{"name": "K"}]}}),
])

# ---------------------------------------------------------------------------------------------
G = "s~get"
write("v3_get", "v3 Get: validation, lax keys, strong/eventual reads, versions", [
    put(ent(G, ["K", "a"], [prop("x", 1)]), ent(G, ["K", "b", "C", 2], [prop("y", "s")])),
    get(ref(G, "K", "a"), ref(G, "K", "missing"), ref(G, "K", "b", "C", 2), ref(G, "K", "a")),
    get(ref(G, "K", "a"), extra="strong: false"),
    get(ref(G, "K", "missing"), extra="strong: false"),
    get(ref(G, "K", "missing"), extra="strong: true failover_ms: 5 allow_deferred: true read_time_epoch_micros: 7"),
    # Validation.
    get(ref("", "K", "a")),
    get(f"key {{ app: {q(G)} path {{ }} }}"),
    get(ref(G, "K")),
    get(ref(G, "K", 0)),
    get(ref(G, "K", "a", "C")),
    get(f"key {{ app: {q(G)} path {{ Element {{ type: \"K\" id: 1 name: \"a\" }} }} }}"),
    get(ref(G, "K", "")),
    get(ref(G, "K", "a", ns="")),
    get(ref(G, "K", 5), ref(G, "K", "a", ns="")),
    # Not validated: these just come back missing.
    get(ref(G, "", "a"), ref(G, "__foo__", "a"), ref(G, "K", "__a__"), ref(G, "K", "a", ns="a b"),
        ref(G, "K", "a", ns="__x__"), ref(G, "K", "a", db="bad db"), ref(G, "K", -5),
        ref(G, "K" * 1501, "a"), ref(G, "K", "n" * 1501)),
    get(ref(G, *(["K", "a"] * 101))),
    get(ref(G, "__kind__", "K")),
    get(ref(G, "__entity_group__", 1)),
    get(ref(G, "K", "a", "__entity_group__", 1)),
    get(ref(G, "K", "a"), extra="transaction { handle: 424242 app: \"s~get\" }"),
])

# ---------------------------------------------------------------------------------------------
P = "s~put"
write("v3_put", "v3 Put: auto ids, entity groups, validation, routing, costs", [
    put(ent(P, ["K", None], [prop("x", 1)], eg=None), ent(P, ["K", None], eg=None)),
    put(ent(P, ["K", "a", "C", None], eg=None)),
    put(ent(P, ["K", "a"], [prop("x", 1)], eg=["K", "zzz"])),
    get(ref(P, "K", "a")),
    put(ent(P, ["K", "b", "C", "c"], [prop("x", 1)], eg=None)),
    get(ref(P, "K", "b", "C", "c")),
    put(ent(P, ["K", "a"], [prop("x", 1)])),
    put(ent(P, ["K", "a"], [prop("x", 2)]), ent(P, ["K", "a"], [prop("x", 3)])),
    get(ref(P, "K", "a")),
    put(ent(P, ["K", "g1"]), ent(P, ["K", "g2"]), ent(P, ["K", "g1", "C", 1])),
    put(),
    put(ent(P, ["K", "a"]), extra="trusted: true force: true mark_changes: true auto_id_policy: SEQUENTIAL sequence_number: 9"),
    # Validation.
    put(ent(P, ["K", "v"], [prop("__x__", 1)])),
    put(ent(P, ["K", "v"], [raw("__y__", 1)])),
    put(ent(P, ["K", "n" * 1501])),
    put(ent(P, ["K", "v"], [prop("s", "x" * 1501)])),
    put(ent(P, ["K", "v"], [raw("s", "x" * 1501)])),
    put(ent(P, ["K", "v"], [prop("s", "x" * 1500)])),
    # Accepted (unlike v1).
    put(ent(P, ["__foo__", "a"]), ent(P, ["K", "__a__"]), ent(P, ["", "e"]), ent(P, ["K", ""], eg=None)),
    put(ent(P, ["K" * 1501, "a"]), ent(P, ["K", "a"], ns="__x__"), ent(P, ["K", "a"], ns="a b"),
        ent(P, ["K", "a"], ns=""), ent(P, ["K", "a"], db="bad db")),
    put(ent(P, ["K", "props"], [prop("", 1), prop("a.b", 2), prop("t", "text", meaning=15),
                                prop("b", "blob", meaning=14), prop("iv", 1, meaning=18)])),
    get(ref(P, "__foo__", "a"), ref(P, "K", "__a__"), ref(P, "", "e"), ref(P, "K", "props"),
        ref(P, "K", "a", ns="__x__"), ref(P, "K", "a", db="bad db"), ref(P, "K", "a", ns="")),
    # First entity's project wins.
    put(ent("s~pq", ["K", "d"], [prop("n", 1)]), ent("s~other2", ["K", "d"], [prop("n", 2)])),
    get(ref("s~pq", "K", "d"), ref("s~other2", "K", "d")),
    put(ent("s~pq", ["K", None], eg=None), ent("s~other2", ["K", None], eg=None)),
    # Incomplete non-final element: Java fails with UNKNOWN.
    put(ent(P, ["P", None, "K", "a"], eg=None)),
    put(ent("", ["K", "a"])),
    # Indexed ENTITY_PROTO bytes that do not parse.
    put(ent(P, ["K", "bad"], [prop("e", T('stringValue: "\\377\\377"'), meaning=19)])),
    put(ent(P, ["K", "badraw"], [raw("e", T('stringValue: "\\377\\377"'), meaning=19)])),
    get(ref(P, "K", "badraw")),
    # Transactions buffer the write: empty cost, no versions.
    begin(P),
    put(ent(P, ["K", "t1"], [prop("x", 1)]), ent(P, ["K", None], eg=None), extra=txn("t", P)),
    get(ref(P, "K", "t1")),
    commit(P),
    get(ref(P, "K", "t1")),
    begin(P, extra="mode: READ_ONLY", cap="ro"),
    put(ent(P, ["K", "t2"]), extra=txn("ro", P)),
    V3("Delete", ref(P, "K", "t2") + " " + txn("ro", P)),
    rollback(P, "ro"),
])

# ---------------------------------------------------------------------------------------------
S = "s~store"
user = T('UserValue { email: "a@b.c" auth_domain: "gmail.com" nickname: "n" gaiaid: 0 obfuscated_gaiaid: "o" }')
point = T("PointValue { x: 1.5 y: -2.5 }")
write("v3_put_storage", "v3 Put stores the EntityProto nearly verbatim; v1 sees Java's conversion", [
    put(ent(S, ["K", "a"], [
        prop("i", 1), prop("b", True), prop("d", 2.5), prop("s", "str"),
        prop("m", "x", multiple=True), prop("m", "y", multiple=True),
        prop("dup", 1), prop("dup", 2),
        prop("single", 7, multiple=True),
        prop("ts", 1234567, meaning=7), prop("bs", "bytes", meaning=16), prop("blob", "b", meaning=14),
        prop("geo", point, meaning=9), prop("pt", point), prop("u", user),
        prop("r", refval(S, "R", 1, "C", "x")), prop("r2", refval("e~store2", "R", "n", ns="ns")),
        prop("atom", "c", meaning=1), prop("link", "http://x", meaning=2), prop("email", "a@b", meaning=8),
        prop("rating", 50, meaning=13), prop("phone", "1", meaning=11), prop("bk", "k", meaning=17),
        prop("two", T('int64Value: 5 stringValue: "s"')),
        prop("nul", None), prop("el", None, meaning=24),
        prop("both", 1), raw("both", 2), raw("text", "t", meaning=15), raw("rawb", "rb", meaning=14),
        raw("rawi", 3), raw("rm", 1, multiple=True), raw("rm", 2, multiple=True),
        prop("muri", 1, extra=' meaning_uri: "uri"'),
    ], extra=' owner { email: "o@x" auth_domain: "d" gaiaid: 9 } kind: GD_CONTACT kind_uri: "kuri"')),
    get(ref(S, "K", "a")),
    REST("/v1/projects/store:lookup", {"keys": [{"path": [{"kind": "K", "name": "a"}]}]}),
    # Order is kept as sent.
    put(ent(S, ["K", "order"], [prop("z", 1), prop("a", 2), raw("y", 3), prop("b", 4)])),
    get(ref(S, "K", "order")),
    # computed properties are dropped on read, stashed ones moved.
    put(ent(S, ["K", "special"], [prop("c", 1, extra=" computed: true"), prop("st", 2, extra=" stashed: 3"),
                                  prop("p", 3), prop("st0", 4, extra=" stashed: 0")])),
    get(ref(S, "K", "special")),
    # v1-written entities as v3 sees them.
    REST("/v1/projects/store:commit", {"mode": "NON_TRANSACTIONAL", "mutations": [{"upsert": {
        "key": {"path": [{"kind": "V1", "name": "all"}]},
        "properties": {
            "ts": {"timestampValue": "2020-01-02T03:04:05.123456Z"},
            "blob": {"blobValue": "AAEC"},
            "rawblob": {"blobValue": "AwQ=", "excludeFromIndexes": True},
            "geo": {"geoPointValue": {"latitude": 1.0, "longitude": 2.0}},
            "arr": {"arrayValue": {"values": [{"integerValue": "1"}, {"stringValue": "s", "excludeFromIndexes": True}]}},
            "empty": {"arrayValue": {}},
            "ent": {"entityValue": {"key": {"path": [{"kind": "E", "name": "x"}]}, "properties": {"q": {"integerValue": "1"}}}},
            "ent0": {"entityValue": {"properties": {"q": {"booleanValue": True}}}},
            "text": {"stringValue": "long", "excludeFromIndexes": True, "meaning": 15},
            "k": {"keyValue": {"path": [{"kind": "R", "id": "3"}]}},
            "n": {"nullValue": None}, "d": {"doubleValue": -0.0},
        }}}]}),
    get(ref(S, "V1", "all")),
    get(ref("e~store", "V1", "all")),
    # Queries over v3-written entities.
    V3("RunQuery", f"app: {q(S)} kind: \"K\" " + flt(5, "m", "y")),
    V3("RunQuery", f"app: {q(S)} kind: \"K\" " + flt(5, "dup", 2)),
    V3("RunQuery", f"app: {q(S)} kind: \"K\" " + flt(5, "c", 1)),
    REST("/v1/projects/store:runQuery", {"query": {"kind": [{"name": "K"}], "filter": {"propertyFilter": {"property": {"name": "dup"}, "op": "EQUAL", "value": {"integerValue": "1"}}}}}),
])

# ---------------------------------------------------------------------------------------------
D = "s~del"
write("v3_delete", "v3 Delete: versions per key, costs, validation, routing", [
    put(ent(D, ["K", "a"], [prop("x", 1)]), ent(D, ["K", "b"])),
    V3("Delete", ref(D, "K", "a") + " " + ref(D, "K", "missing")),
    V3("Delete", ref(D, "K", "missing")),
    V3("Delete"),
    V3("Delete", ref(D, "K")),
    V3("Delete", ref(D, "__foo__", "a")),
    V3("Delete", ref(D, "K", "b") + " " + ref(D, "K", "b")),
    put(ent("s~dq", ["K", "x"]), ent("s~dr", ["K", "x"])),
    V3("Delete", ref("s~dq", "K", "x") + " " + ref("s~dr", "K", "x")),
    get(ref("s~dq", "K", "x"), ref("s~dr", "K", "x")),
    V3("Delete", ref(D, "K", "a") + " trusted: true force: true mark_changes: true sequence_number: 3"),
    begin(D),
    V3("Delete", ref(D, "K", "b") + " " + txn("t", D)),
    commit(D),
])

# ---------------------------------------------------------------------------------------------
Q = "s~qry"
setup_q = put(*[ent(Q, ["K", f"e{i:02d}"], [prop("x", i % 3), prop("y", i), prop("t", ["a", "b"][i % 2]),
                                            prop("arr", 1, multiple=True), prop("arr", i, multiple=True)])
                for i in range(25)],
              ent(Q, ["K", "e00", "C", "c1"], [prop("x", 9)]), ent(Q, ["Z", "z"], [prop("x", 1)]))
write("v3_query", "v3 RunQuery: batching, offsets, projections, compile, result shapes", [
    setup_q,
    query(Q, "K"),
    query(Q, "K", "count: 3", order("y")),
    query(Q, "K", "limit: 4", order("y", 2)),
    query(Q, "K", "limit: 0"),
    query(Q, "K", "count: 0"),
    query(Q, "K", "offset: 5 count: 2", order("y")),
    query(Q, "K", "offset: 30"),
    query(Q, "K", "offset: -1"),
    query(Q, "K", "keys_only: true limit: 3", order("y")),
    query(Q, "K", 'property_name: "t" limit: 3', order("t")),
    query(Q, "K", 'property_name: "t" group_by_property_name: "t"', order("t")),
    query(Q, "K", 'property_name: "arr" limit: 4', flt(5, "y", 3)),
    query(Q, "K", "distinct: true limit: 2", order("y")),
    query(Q, "K", flt(5, "x", 1), order("y")),
    query(Q, "K", flt(3, "y", 20), flt(2, "y", 22)),
    query(Q, "K", flt(6, "y", 4)),
    query(Q, "K", flt(7, "y", 4), "limit: 2"),
    query(Q, None, "limit: 3", order("__key__")),
    query(Q, "K", ref(Q, "K", "e00", field="ancestor")),
    query(Q, "K", ref(Q, "K", field="ancestor")),
    query(Q, "K", "limit: 2 compile: true", order("y")),
    query(Q, "K", "limit: 2 offset: 1 compile: true keys_only: true", flt(5, "x", 1)),
    query(Q, "K", "limit: 2 compile: true", order("__key__", 2)),
    query(Q, "K", 'name_space: ""'),
    query(Q, "K", 'database_id: "other" limit: 1 failover_ms: 3 strong: true hint: ORDER_FIRST '
                  'require_perfect_plan: true min_safe_time_seconds: 1 read_time_us: 1 shallow: true persist_offset: false'),
    query(Q, "__kind__"),
    query(Q, "__property__", "keys_only: true"),
    query(Q, "__namespace__"),
    query(Q, "K", "limit: 3", order("__scatter__")),
    query(Q, "K", "limit: 3", flt(3, "__scatter__", "")),
    query("e~qry", "Z"),
    query(Q, "K", ref("s~qry", "Z", "z", field="ancestor") + " compiled_cursor { }", "limit: 1"),
    # Cursors carry the project id.
    query(Q, "K", "limit: 2 compile: true", order("y"),
          'compiled_cursor { postfix_position { key { app: "qry" path { Element { type: "K" name: "e05" } } } before: false } }'),
    query(Q, "K", "limit: 2", order("y"),
          'compiled_cursor { postfix_position { key { app: "s~qry" path { Element { type: "K" name: "e05" } } } before: false } }'),
    query(Q, "K", "limit: 2", order("y"),
          'end_compiled_cursor { postfix_position { key { app: "qry" path { Element { type: "K" name: "e03" } } } before: true } }'),
    query(Q, "K", "limit: 2",
          'compiled_cursor { Position { start_key: "garbage" } }'),
])

write("v3_query_errors", "v3 RunQuery errors", [
    setup_q,
    query(Q, "K", flt(1, "x", 1), flt(3, "y", 2)),
    query(Q, "K", "Filter { op: 5 " + prop("x", 1) + " " + prop("x", 2) + " }"),
    query(Q, "K", "Filter { op: 6 }"),
    query(Q, "K", flt(9, "x", 1)),
    query(Q, "K", flt(10, "x", 1)),
    query(Q, "K", 'search_query: "x"'),
    query(Q, None, flt(5, "x", 1)),
    query(Q, None, order("x")),
    query(Q, None, order("__key__", 2)),
    query(Q, "__entity_group__"),
    query(Q, "K", flt(5, "__key__", 1)),
    query(Q, "K", 'property_name: "__key__"'),
    query(Q, "K", 'keys_only: true property_name: "x"'),
    query(Q, "K", ref("s~zzz", "K", "a", field="ancestor")),
    query(Q, "K", ref(Q, "K", "a", ns="n1", field="ancestor")),
    query(Q, "Z", 'compiled_cursor { postfix_position { key { app: "qry" path { Element { type: "K" name: "e05" } } } before: false } }'),
    query(Q, "K", order("y"), 'compiled_cursor { postfix_position { index_value { property_name: "x" value { int64Value: 1 } } key { app: "qry" path { Element { type: "K" name: "e05" } } } before: false } }'),
    query(Q, "K", "count: -1"),
    query(Q, "K", "limit: -1"),
    begin(Q),
    query(Q, "K", txn("t", Q)),
    query("s~zz", "K", ref("s~zz", "K", "a", field="ancestor"), txn("t", Q)),
    query(Q, "K", ref(Q, "K", "e01", field="ancestor"), txn("t", Q)),
    rollback(Q),
])

# ---------------------------------------------------------------------------------------------
N = "s~nxt"
write("v3_next", "v3 Next: batches, offsets, snapshots, handles", [
    put(*[ent(N, ["K", f"e{i:03d}"], [prop("y", i)]) for i in range(30)]),
    query(N, "K", "count: 10", order("y"), cap="c"),
    nxt(N, extra="count: 5"),
    nxt(N),
    nxt(N, extra="compile: true count: 2"),
    nxt(N),
    nxt(N),
    nxt(N, h="c", extra="count: 1"),
    # Snapshot: later writes are not seen.
    query(N, "K", "count: 2", order("y"), cap="s"),
    put(ent(N, ["K", "e000"], [prop("y", 100)])),
    V3("Delete", ref(N, "K", "e001")),
    nxt(N, h="s", extra="count: 3"),
    # Offsets.
    query(N, "K", "offset: 5 count: 2", order("y"), cap="o"),
    nxt(N, h="o", extra="offset: 3"),
    nxt(N, h="o", extra="offset: 0 count: 2"),
    query(N, "K", "offset: 25 count: 0", order("y"), cap="o2"),
    nxt(N, h="o2", extra="offset: 5"),
    nxt(N, h="o2", extra="offset: 25 count: 1"),
    nxt(N, h="o2", extra="offset: 0 count: 1"),
    # Handles belong to the project, not the app id.
    query("e~nxt", "K", "count: 1", order("y"), cap="p"),
    nxt("s~nxt", h="p", extra="count: 1"),
    V3("Next", "cursor { cursor: {{p}} app: \"nxt\" database_id: \"d\" } count: 1"),
    nxt("s~nobody", h="p"),
    V3("Next", "cursor { cursor: {{p}} }"),
    nxt(N, h="p", extra="count: -1"),
    V3("Next", "cursor { cursor: 987654 app: \"s~nxt\" }"),
    # Large offsets carry over in batches of 300.
    put(*[ent(N, ["L", i + 1]) for i in range(350)]),
    query(N, "L", "offset: 320 keys_only: true count: 5", cap="l"),
    nxt(N, h="l", extra="offset: 50"),
    nxt(N, h="l", extra="offset: 20 count: 2"),
    query(N, "L", "keys_only: true count: 400", cap="l2"),
])

# ---------------------------------------------------------------------------------------------
T = "s~txn"
write("v3_txn", "v3 transactions: options, cross-group rules, commit versions, contention", [
    put(ent(T, ["K", "a"], [prop("x", 1)]), ent(T, ["K", "b"], [prop("x", 1)])),
    begin(T),
    get(ref(T, "K", "a"), extra=txn("t", T)),
    get(ref(T, "K", "b"), extra=txn("t", T)),
    get(ref(T, "K", "a", "C", 1), extra=txn("t", T)),
    put(ent(T, ["K", "b"]), extra=txn("t", T)),
    put(ent(T, ["K", "a", "C", 2]), extra=txn("t", T)),
    commit(T),
    begin(T, extra="allow_multiple_eg: true database_id: \"db\"", cap="x"),
    get(ref(T, "K", "a"), ref(T, "K", "b"), ref(T, "K", "c"), extra=txn("x", T)),
    put(ent(T, ["K", "a"], [prop("x", 2)]), ent(T, ["K", "c"]), extra=txn("x", T)),
    V3("Delete", ref(T, "K", "b") + " " + ref(T, "K", "gone") + " " + txn("x", T)),
    commit(T, h="x"),
    commit(T, h="x"),
    rollback(T, h="x"),
    get(ref(T, "K", "a"), ref(T, "K", "b"), ref(T, "K", "c")),
    # Too many groups (allow_multiple_eg).
    begin(T, extra="allow_multiple_eg: true", cap="m"),
    get(*[ref(T, "G", i + 1) for i in range(26)], extra=txn("m", T)),
    rollback(T, h="m"),
    # Contention: CONCURRENT_TRANSACTION, then the handle can still be rolled back.
    begin(T, cap="c1"),
    get(ref(T, "K", "a"), extra=txn("c1", T)),
    put(ent(T, ["K", "a"], [prop("x", 3)])),
    put(ent(T, ["K", "a"], [prop("x", 4)]), extra=txn("c1", T)),
    commit(T, h="c1"),
    rollback(T, h="c1"),
    rollback(T, h="c1"),
    # Queries in transactions.
    begin(T, cap="q"),
    query(T, "K", ref(T, "K", "a", field="ancestor"), txn("q", T)),
    get(ref(T, "K", "a"), extra=txn("q", T)),
    get(ref(T, "K", "b"), extra=txn("q", T)),
    rollback(T, h="q"),
    begin(T, cap="q2", extra="allow_multiple_eg: true"),
    query(T, "K", ref(T, "K", "a", field="ancestor"), txn("q2", T)),
    get(ref(T, "K", "b"), extra=txn("q2", T)),
    query(T, "K", txn("q2", T)),
    rollback(T, h="q2"),
    # Read-only transactions and retries.
    begin(T, cap="r", extra="mode: READ_ONLY"),
    get(ref(T, "K", "a"), extra=txn("r", T)),
    commit(T, h="r"),
    begin(T, cap="r2", extra="mode: READ_ONLY"),
    begin(T, cap="r3", extra="previous_transaction { handle: {{r2}} app: \"s~txn\" }"),
    begin(T, cap="r4", extra="mode: READ_ONLY previous_transaction { handle: {{r2}} app: \"s~txn\" }"),
    begin(T, cap="w"),
    begin(T, cap="w2", extra="allow_multiple_eg: true previous_transaction { handle: {{w}} app: \"s~txn\" }"),
    begin(T, cap="w3", extra="mode: READ_WRITE previous_transaction { handle: {{w}} app: \"s~txn\" }"),
    begin(T, cap="w4", extra="previous_transaction { handle: 777777 app: \"s~txn\" }"),
    begin(T, cap="w5", extra="mode: READ_WRITE"),
    # Handles in other projects.
    V3("Commit", "handle: {{w5}} app: \"s~txn-other\""),
    put(ent("s~txn-other", ["K", "a"])),
    V3("Commit", "handle: {{w5}} app: \"s~txn-other\""),
    V3("Rollback", "handle: {{w5}} app: \"e~txn\""),
    V3("Rollback", "handle: 5555555 app: \"s~txn\""),
    V3("Rollback", "handle: 5555555 app: \"s~never-seen\""),
    V3("Commit", "handle: 5555555 app: \"s~never-seen2\""),
    V3("BeginTransaction", "app: \"\""),
])

# ---------------------------------------------------------------------------------------------
L = "s~alloc"
write("v3_alloc", "v3 AllocateIds: the sequential counter shared with auto ids", [
    V3("AllocateIds", ref(L, "K", field="model_key") + " size: 5"),
    put(ent(L, ["K", None], eg=None)),
    V3("AllocateIds", "size: 3"),
    V3("AllocateIds", "max: 100"),
    V3("AllocateIds", "max: 50"),
    V3("AllocateIds", ref(L, "K", "a", "C", 3, field="model_key") + " size: 1"),
    V3("AllocateIds", "size: 0"),
    V3("AllocateIds", "size: -1"),
    V3("AllocateIds", "size: 1"),
    V3("AllocateIds", "size: 1000000000"),
    V3("AllocateIds", "size: 1000000001"),
    V3("AllocateIds", "max: 0"),
    V3("AllocateIds", "size: 2 max: 5000"),
    V3("AllocateIds", ref(L, "K", field="model_key") + " " + ref(L, "K", 9, field="reserve") + " trusted: true size: 1"),
    V3("AllocateIds"),
    REST("/v1/projects/alloc:allocateIds", {"keys": [{"path": [{"kind": "K"}]}]}),
    put(ent(L, ["K", None], eg=None)),
])

# ---------------------------------------------------------------------------------------------
write("v3_indices", "v3 GetIndices and the composite indexes queries report", [
    V3("GetIndices", "app_id: \"s~idx\""),
    put(ent("s~idx", ["K", "a"], [prop("x", 1), prop("y", 2)])),
    query("s~idx", "K", flt(5, "x", 1), order("y")),
    query("s~idx", "K", order("x"), order("y", 2)),
    query("s~idx", "K", ref("s~idx", "K", "a", field="ancestor"), order("y")),
    V3("GetIndices", "app_id: \"s~idx\""),
    V3("GetIndices", "app_id: \"s~nobody\" database_id: \"x\""),
    V3("GetIndices", "app_id: \"\""),
    REST("/v1/projects/idx:runQuery", {"query": {"kind": [{"name": "J"}], "order": [{"property": {"name": "a"}}, {"property": {"name": "b"}}]}}),
    V3("GetIndices", "app_id: \"e~idx\""),
])


# ---------------------------------------------------------------------------------------------
def vint(n):
    out = b""
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out += bytes([b | 0x80])
        else:
            return out + bytes([b])


def fld(no, data):
    return vint(no << 3 | 2) + vint(len(data)) + data


def txn_hex(app, h="t"):
    """A v3 Transaction with a captured handle: `09 <fixed64>` + app."""
    return "09{{%s:fixed64}}" % h + fld(2, app.encode()).hex()


def add_req(txn=None, dtxn=None, app=None, name=b"t1"):
    """TaskQueueAddRequest hex; `txn`/`dtxn` are hex strings (may hold placeholders)."""
    head = (fld(1, b"default") + fld(2, name) + vint(3 << 3) + vint(123) + fld(4, b"/work") + fld(9, b"body")).hex()
    if txn is not None:
        head += "52(%s)" % txn
    if dtxn is not None:
        head += "aa01(%s)" % dtxn
    if app is not None:
        head += fld(11, app).hex()
    return head


def bulk(*reqs):
    return "".join("0a(%s)" % r for r in reqs)


AA = "s~act"
write("v3_actions", "v3 AddActions (taskqueue bulk add requests in a transaction)", [
    begin(AA),
    HEX("AddActions", bulk(add_req(txn=txn_hex(AA)))),
    HEX("AddActions", bulk(*[add_req(txn=txn_hex(AA), name=b"t%d" % i) for i in range(4)])),
    HEX("AddActions", bulk(add_req(txn=txn_hex(AA)))),
    HEX("AddActions", bulk(add_req())),
    HEX("AddActions", bulk(add_req(dtxn=txn_hex(AA)))),
    HEX("AddActions", bulk(add_req(dtxn="ffff"))),
    HEX("AddActions", bulk(add_req(txn="09" + "9f86010000000000" + fld(2, AA.encode()).hex()))),
    HEX("AddActions", bulk(add_req(txn=txn_hex("s~act-nope")))),
    HEX("AddActions", "0a03" + "0a0171"),
    put(ent(AA, ["K", "a"], [prop("x", 1)]), extra=txn("t", AA)),
    commit(AA),
    get(ref(AA, "K", "a")),
    begin(AA, cap="t2"),
    HEX("AddActions", bulk(add_req(txn=txn_hex(AA, "t2"), app=b"s~act"))),
    commit(AA, h="t2", opts="hostport=-"),
    begin(AA, cap="t3"),
    HEX("AddActions", bulk(add_req(txn=txn_hex(AA, "t3")))),
    put(ent(AA, ["K", "b"], [prop("x", 1)]), extra=txn("t3", AA)),
    commit(AA, h="t3", opts="hostport=127.0.0.1:1"),
    get(ref(AA, "K", "b")),
    rollback(AA, "t3"),
    begin(AA, cap="t4"),
    HEX("AddActions", bulk(add_req(txn=txn_hex(AA, "t4")))),
    rollback(AA, "t4"),
])

# ---------------------------------------------------------------------------------------------
write("v3_consistency", "v3 reads under the eventual-consistency job policy", [
    ("PATCH", "/v1/config", json.dumps({"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "7", "unappliedJobPercentage": 100}}}), []),
    put(ent("s~cons", ["K", "a"], [prop("x", 1)])),
    query("s~cons", "K"),
    get(ref("s~cons", "K", "a"), extra="strong: false"),
    get(ref("s~cons", "K", "missing"), extra="strong: false"),
    query("s~cons", "K"),
    get(ref("s~cons", "K", "a")),
    query("s~cons", "K"),
    ("PATCH", "/v1/config", json.dumps({"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "0", "unappliedJobPercentage": 0}}}), []),
])
