import json, os
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tests", "conformance", "scenarios")
def I(v): return {"integerValue": str(v)}
def S(v): return {"stringValue": v}
def D(v): return {"doubleValue": v}
def B(v): return {"booleanValue": v}
def T(v): return {"timestampValue": v}
def A(*v): return {"arrayValue": {"values": list(v)} if v else {}}
NULL = {"nullValue": None}
def K(*path, ns=None):
    els = []
    it = iter(path)
    for kind, idn in zip(it, it):
        e = {"kind": kind}
        if idn is not None:
            e["id" if isinstance(idn, int) else "name"] = str(idn) if isinstance(idn, int) else idn
        els.append(e)
    k = {"path": els}
    if ns: k["partitionId"] = {"namespaceId": ns}
    return k
def ent(key, **props): return {"upsert": {"key": key, "properties": props}}

people = [
  ent(K("P","a"), age=I(30), name=S("Alice"), score=D(1.5), tags=A(S("x"),S("y")), active=B(True), born=T("1990-01-01T00:00:00Z"), city=S("Paris"), unidx={"stringValue":"u","excludeFromIndexes":True}),
  ent(K("P","b"), age=I(25), name=S("bob"), score=D(2.5), tags=A(S("y"),S("z")), active=B(False), born=T("1995-06-01T12:00:00Z"), city=S("Rome")),
  ent(K("P","c"), age=I(35), name=S("Carol"), score=D(-1.0), tags=A(), active=B(True), city=NULL),
  ent(K("P",1), age=I(30), name=S("dave"), score=D("NaN"), tags=A(S("x")), active=B(True)),
  ent(K("P",2), age=S("30"), name=S("Eve"), score=D(0)),
  ent(K("P","d"), name=S("Frank"), tags=A(S("z"),S("x"))),
  ent(K("P","e"), age=I(40), name=S(""), score=D(0), active=NULL),
  ent(K("P","a","C",1), v=I(1)), ent(K("P","a","C",2), v=I(2)), ent(K("P","b","C",3), v=I(3)),
  ent(K("P","a","C",1,"G","x"), v=I(4)),
  ent(K("P","a", ns="ns"), age=I(99)),
  ent(K("Q","z"), age=I(30)),
]
mixed_vals = [NULL, B(False), B(True), I(-5), I(5), T("2020-01-01T00:00:00Z"), D(1.5), D("NaN"), D(-0.5), S("abc"), S("B"), {"blobValue":"YWJj"}, {"keyValue":K("P","a")}, {"keyValue":K("P",1)}, {"geoPointValue":{"latitude":1,"longitude":2}}, {"entityValue":{"properties":{"x":I(1)}}}, A(), A(I(1),S("a"))]
mixed = [ent(K("M", i+1), v=v) for i, v in enumerate(mixed_vals)]
SEED = {"mode":"NON_TRANSACTIONAL","mutations": people + mixed}

def q(**kw): return kw
def kind(k): return [{"name": k}]
def pf(prop, op, value): return {"propertyFilter": {"property": {"name": prop}, "op": op, "value": value}}
def cf(op, *filters): return {"compositeFilter": {"op": op, "filters": list(filters)}}
def order(prop, d="ASCENDING"): return {"property": {"name": prop}, "direction": d}
def proj(*props): return [{"property": {"name": p}} for p in props]

def write(name, comment, steps, seed=SEED):
    lines = [f"# {comment}", "POST /v1/projects/p:commit", json.dumps(seed), ""]
    for step in steps:
        if isinstance(step, tuple):  # (body, captures) or (path, body, captures)
            if len(step) == 3: path, body, caps = step
            else: (body, caps), path = step, "/v1/projects/p:runQuery"
        else: path, body, caps = "/v1/projects/p:runQuery", step, []
        lines += [f"POST {path}", body if isinstance(body, str) else json.dumps(body)]
        lines += [f"> {c}" for c in caps]
        lines.append("")
    open(os.path.join(OUT, name + ".scn"), "w").write("\n".join(lines))

RQ = lambda **query: {"query": query}

write("query_basic", "Kind, kindless, ancestor and namespace queries; keys-only; limit and offset.", [
  RQ(kind=kind("P")), RQ(kind=kind("C")), RQ(kind=kind("Nope")), RQ(), RQ(kind=kind("P"), limit=3), RQ(kind=kind("P"), offset=2),
  RQ(kind=kind("P"), offset=2, limit=2), RQ(kind=kind("P"), offset=100), RQ(kind=kind("P"), limit=0), RQ(kind=kind("P"), offset=-1), RQ(kind=kind("P"), limit=-1),
  RQ(kind=kind("P"), projection=proj("__key__")), RQ(projection=proj("__key__")),
  RQ(kind=kind("C"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","a")})), RQ(filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","a")})),
  RQ(kind=kind("P"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","a")})), RQ(filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","zz")})),
  {"partitionId": {"namespaceId": "ns"}, "query": {"kind": kind("P")}}, {"partitionId": {"namespaceId": "nope"}, "query": {}},
  {"partitionId": {"projectId": "other"}, "query": {"kind": kind("P")}}, {"query": {"kind": kind("P")}, "readOptions": {"readConsistency": "EVENTUAL"}},
  RQ(kind=[{"name":"P"},{"name":"C"}]), RQ(kind=kind("")), RQ(kind=kind("__x__")),
  {}, {"query": {}, "gqlQuery": {"queryString": "SELECT * FROM P"}},
])

ops = ["EQUAL","LESS_THAN","LESS_THAN_OR_EQUAL","GREATER_THAN","GREATER_THAN_OR_EQUAL","NOT_EQUAL"]
fsteps = []
for prop, val in [("age", I(30)), ("name", S("bob")), ("score", D(1.5)), ("active", B(True)), ("born", T("1993-01-01T00:00:00Z")), ("tags", S("y")), ("city", NULL), ("unidx", S("u")), ("missing", I(1))]:
    for op in ops:
        fsteps.append(RQ(kind=kind("P"), filter=pf(prop, op, val), projection=proj("__key__")))
fsteps += [
  RQ(kind=kind("P"), filter=pf("age","EQUAL",S("30")), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("score","EQUAL",D("NaN")), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("score","EQUAL",I(0)), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("__key__","EQUAL",{"keyValue":K("P","b")})),
  RQ(kind=kind("P"), filter=pf("__key__","GREATER_THAN",{"keyValue":K("P","b")}), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("__key__","LESS_THAN",{"keyValue":K("P","b")}), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("__key__","GREATER_THAN",{"keyValue":K("Q","a")}), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("__key__","EQUAL",S("b"))),
  RQ(kind=kind("P"), filter=pf("age","IN",A(I(25),I(35),S("30"))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","NOT_IN",A(I(25),I(35))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("tags","IN",A(S("x"),S("z"))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("tags","NOT_IN",A(S("x"))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","IN",A()), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","IN",I(1)), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("AND", pf("age","EQUAL",I(30)), pf("active","EQUAL",B(True))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("AND", pf("age","GREATER_THAN",I(20)), pf("age","LESS_THAN",I(35))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("OR", pf("age","EQUAL",I(25)), pf("name","EQUAL",S("Frank"))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("OR", pf("age","EQUAL",I(30)), cf("AND", pf("tags","EQUAL",S("z")), pf("name","EQUAL",S("Frank")))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("AND"), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=cf("AND", pf("tags","EQUAL",S("x")), pf("tags","EQUAL",S("y"))), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("tags","EQUAL",A()), projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","EQUAL",{}), projection=proj("__key__")),
  RQ(kind=kind("P"), filter={"propertyFilter": {"property": {"name": "age"}, "value": I(1)}}),
  RQ(kind=kind("P"), filter={}),
  RQ(kind=kind("M"), filter=pf("v","GREATER_THAN",NULL), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","LESS_THAN",S("b")), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","GREATER_THAN_OR_EQUAL",{"keyValue":K("P","a")}), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","EQUAL",{"entityValue":{"properties":{"x":I(1)}}}), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v.x","EQUAL",I(1)), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","EQUAL",{"blobValue":"YWJj"}), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","EQUAL",T("2020-01-01T00:00:00Z")), projection=proj("__key__")),
  RQ(kind=kind("M"), filter=pf("v","EQUAL",I(1577836800000000)), projection=proj("__key__")),
]
write("query_filters", "Filter operators per value type, multi-valued and unindexed properties, IN/NOT_IN, AND/OR.", fsteps)

write("query_orders", "Sort orders: directions, multiple orders, multi-valued properties, cross-type ordering.", [
  RQ(kind=kind("P"), order=[order("age")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("age","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("age"), order("name","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("tags")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("tags","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("name")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("score")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("__key__","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("__key__")], projection=proj("__key__")),
  RQ(order=[order("__key__","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("unidx")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("missing")], projection=proj("__key__")),
  RQ(kind=kind("M"), order=[order("v")], projection=proj("__key__")),
  RQ(kind=kind("M"), order=[order("v","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(26)), order=[order("age","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(26)), order=[order("name")], projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("tags","EQUAL",S("x")), order=[order("tags")], projection=proj("__key__")),
  RQ(kind=kind("P"), filter=pf("tags","GREATER_THAN",S("x")), order=[order("tags")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[order("age"), order("age","DESCENDING")], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[{"property": {"name": "age"}}], projection=proj("__key__")),
  RQ(kind=kind("P"), order=[{"direction": "ASCENDING"}], projection=proj("__key__")),
  RQ(kind=kind("C"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","a")}), order=[order("v","DESCENDING")]),
])

write("query_projection", "Projections, distinct_on, projections over arrays, and their restrictions.", [
  RQ(kind=kind("P"), projection=proj("age")), RQ(kind=kind("P"), projection=proj("age","name")), RQ(kind=kind("P"), projection=proj("tags")),
  RQ(kind=kind("P"), projection=proj("tags","age")), RQ(kind=kind("P"), projection=proj("age"), distinctOn=[{"name":"age"}]),
  RQ(kind=kind("P"), projection=proj("tags"), distinctOn=[{"name":"tags"}]), RQ(kind=kind("P"), projection=proj("age","name"), distinctOn=[{"name":"age"}]),
  RQ(kind=kind("P"), distinctOn=[{"name":"age"}]), RQ(kind=kind("P"), projection=proj("age"), distinctOn=[{"name":"name"}]),
  RQ(kind=kind("P"), projection=proj("unidx")), RQ(kind=kind("P"), projection=proj("missing")), RQ(kind=kind("P"), projection=proj("age","age")),
  RQ(kind=kind("P"), projection=proj("age"), filter=pf("age","EQUAL",I(30))), RQ(kind=kind("P"), projection=proj("age"), filter=pf("age","GREATER_THAN",I(26))),
  RQ(kind=kind("P"), projection=proj("name"), order=[order("age")]), RQ(kind=kind("P"), projection=proj("age","__key__")),
  RQ(kind=kind("M"), projection=proj("v")), RQ(kind=kind("M"), projection=proj("v.x")), RQ(projection=proj("age")),
  RQ(kind=kind("P"), projection=[{"property": {}}]), RQ(kind=kind("P"), projection=proj("age"), order=[order("age","DESCENDING")], limit=2, offset=1),
])

write("query_cursors", "Cursors: paging with endCursor, start/end cursors, offsets, descending order, invalid cursors.", [
  (RQ(kind=kind("P"), limit=2), ["c1 = batch.endCursor", "r1 = batch.entityResults.0.cursor"]),
  RQ(kind=kind("P"), limit=2, startCursor="{{c1}}"), RQ(kind=kind("P"), startCursor="{{r1}}"), RQ(kind=kind("P"), endCursor="{{c1}}"),
  RQ(kind=kind("P"), startCursor="{{r1}}", endCursor="{{c1}}"), RQ(kind=kind("P"), startCursor="{{c1}}", offset=1, limit=1),
  (RQ(kind=kind("P"), order=[order("age","DESCENDING")], limit=3), ["c2 = batch.endCursor"]),
  RQ(kind=kind("P"), order=[order("age","DESCENDING")], startCursor="{{c2}}"), RQ(kind=kind("P"), order=[order("age")], startCursor="{{c2}}"),
  (RQ(kind=kind("P"), offset=3), ["c3 = batch.endCursor"]), (RQ(kind=kind("P"), offset=3, limit=0), ["c4 = batch.endCursor"]),
  RQ(kind=kind("P"), startCursor="{{c4}}"),
  (RQ(kind=kind("P"), projection=proj("tags"), limit=2), ["c5 = batch.endCursor"]), RQ(kind=kind("P"), projection=proj("tags"), startCursor="{{c5}}"),
  RQ(kind=kind("C"), startCursor="{{c1}}"), RQ(kind=kind("P"), startCursor="AAAA"), RQ(kind=kind("P"), startCursor="!!"), RQ(kind=kind("P"), startCursor=""),
  (RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(26)), limit=1), ["c6 = batch.endCursor"]),
  RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(26)), startCursor="{{c6}}"),
  RQ(kind=kind("Nope"), limit=1),
])

write("query_errors", "Query validation errors.", [
  RQ(kind=kind("P"), filter=cf("AND", pf("age","GREATER_THAN",I(1)), pf("score","GREATER_THAN",D(0)))),
  RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(1)), order=[order("name")]),
  RQ(kind=kind("P"), filter=pf("age","GREATER_THAN",I(1)), order=[order("name"), order("age")]),
  RQ(kind=kind("P"), filter=cf("AND", pf("age","NOT_EQUAL",I(1)), pf("score","GREATER_THAN",D(0)))),
  RQ(kind=kind("P"), filter=cf("AND", pf("age","NOT_EQUAL",I(1)), pf("age","NOT_IN",A(I(2))))),
  RQ(kind=kind("P"), filter=cf("AND", pf("age","IN",A(I(1))), pf("score","IN",A(D(1))))),
  RQ(kind=kind("P"), filter=pf("age","IN",A(*[I(i) for i in range(31)]))),
  RQ(kind=kind("P"), filter=pf("age","NOT_IN",A(*[I(i) for i in range(11)]))),
  RQ(kind=kind("P"), filter=pf("__key__","HAS_ANCESTOR",S("a"))),
  RQ(kind=kind("P"), filter=pf("age","HAS_ANCESTOR",{"keyValue":K("P","a")})),
  RQ(kind=kind("P"), filter=cf("AND", pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","a")}), pf("__key__","HAS_ANCESTOR",{"keyValue":K("P","b")}))),
  RQ(kind=kind("P"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("P")})),
  RQ(kind=kind("P"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":{"partitionId":{"namespaceId":"ns"},"path":[{"kind":"P","name":"a"}]}})),
  RQ(filter=pf("age","EQUAL",I(1))), RQ(order=[order("age")]),
  RQ(kind=kind("P"), filter=pf("__key__","IN",A({"keyValue":K("P","a")}))),
  RQ(kind=kind("P"), filter=pf("__key__","LESS_THAN",{"keyValue":K("P")})),
  RQ(kind=kind("P"), filter=pf("","EQUAL",I(1))), RQ(kind=kind("P"), filter=pf("__x__","EQUAL",I(1))),
  RQ(kind=kind("P"), filter=pf("age","EQUAL",A(I(1)))),
  RQ(kind=kind("P"), filter=pf("age","EQUAL",{"stringValue":"{{repeat 1501 s}}"})),
  RQ(kind=kind("P"), projection=proj("age"), filter=pf("age","IN",A(I(25),I(30)))),
  RQ(kind=kind("P"), filter=cf("OR", pf("age","GREATER_THAN",I(1)), pf("score","GREATER_THAN",D(0)))),
  RQ(kind=kind("P"), filter=cf("OR")), RQ(kind=kind("P"), filter=cf("OPERATOR_UNSPECIFIED", pf("age","EQUAL",I(1)))),
  RQ(kind=kind("P"), limit=2147483648), RQ(kind=kind("P"), startCursor="{{repeat 5 AAAA}}"),
])

def G(s, **kw): return {"gqlQuery": dict(queryString=s, **kw)}
write("query_gql", "GQL: syntax, literals, bindings, functions and errors.", [
  G("SELECT * FROM P"), G("select * from P"), G("SELECT * FROM `P`"), G("SELECT __key__ FROM P"), G("SELECT age, name FROM P"),
  G("SELECT DISTINCT age FROM P"), G("SELECT DISTINCT ON (age) age, name FROM P"), G("SELECT * FROM P LIMIT 2"), G("SELECT * FROM P LIMIT 2 OFFSET 1"),
  G("SELECT * FROM P OFFSET 5"), G("SELECT * FROM P ORDER BY age DESC"), G("SELECT * FROM P ORDER BY age ASC, name DESC"),
  G("SELECT * FROM P WHERE age = 30"), G("SELECT * FROM P WHERE age = 30", allowLiterals=True), G("SELECT * FROM P WHERE name = 'bob'", allowLiterals=True),
  G("SELECT * FROM P WHERE name = \"bob\"", allowLiterals=True), G("SELECT * FROM P WHERE active = TRUE", allowLiterals=True), G("SELECT * FROM P WHERE city = NULL", allowLiterals=True),
  G("SELECT * FROM P WHERE score > 1.0", allowLiterals=True), G("SELECT * FROM P WHERE age >= 30 AND age < 40", allowLiterals=True),
  G("SELECT * FROM P WHERE age = 30 OR age = 25", allowLiterals=True), G("SELECT * FROM P WHERE age IN ARRAY(25, 35)", allowLiterals=True),
  G("SELECT * FROM P WHERE age NOT IN ARRAY(25, 35)", allowLiterals=True), G("SELECT * FROM P WHERE age != 30", allowLiterals=True),
  G("SELECT * FROM P WHERE tags CONTAINS 'x'", allowLiterals=True), G("SELECT * FROM P WHERE tags HAS 'x'", allowLiterals=True),
  G("SELECT * FROM P WHERE __key__ = KEY(P, 'a')", allowLiterals=True), G("SELECT * FROM C WHERE __key__ HAS ANCESTOR KEY(P, 'a')", allowLiterals=True),
  G("SELECT * FROM P WHERE KEY(P, 'a') HAS DESCENDANT __key__", allowLiterals=True), G("SELECT * FROM P WHERE __key__ = KEY('P', 1)", allowLiterals=True),
  G("SELECT * FROM P WHERE born > DATETIME('1991-01-01T00:00:00Z')", allowLiterals=True), G("SELECT * FROM P WHERE born > DATETIME('1991-01-01 00:00:00')", allowLiterals=True),
  G("SELECT * FROM M WHERE v = BLOB('abc')", allowLiterals=True), G("SELECT * FROM P WHERE age = @a", namedBindings={"a": {"value": I(30)}}),
  G("SELECT * FROM P WHERE age = @1 AND name = @2", positionalBindings=[{"value": I(30)}, {"value": S("Alice")}]),
  G("SELECT * FROM P WHERE age = @missing"), G("SELECT * FROM P WHERE age = @1"), G("SELECT * FROM P WHERE age = @a", namedBindings={"a": {"value": I(30)}, "b": {"value": I(1)}}),
  G("SELECT * FROM P LIMIT @lim", namedBindings={"lim": {"value": I(1)}}),
  (G("SELECT * FROM P LIMIT 2", allowLiterals=True), ["g1 = batch.endCursor"]), G("SELECT * FROM P LIMIT 2 OFFSET @c", namedBindings={"c": {"cursor": "{{g1}}"}}),
  G("SELECT * FROM P WHERE age > 30 AND name = 'x'", allowLiterals=True), G("SELECT * FROM `__kind__`"), G("SELECT * WHERE __key__ HAS ANCESTOR KEY(P, 'a')", allowLiterals=True),
  G(""), G("SELECT"), G("SELECT * FROM"), G("SELEC * FROM P"), G("SELECT * FROM P WHERE"), G("SELECT * FROM P WHERE age = 'unterminated", allowLiterals=True),
  G("SELECT * FROM P ORDER BY"), G("SELECT * FROM P LIMIT -1", allowLiterals=True), G("SELECT * FROM P WHERE age == 30", allowLiterals=True),
  G("SELECT * FROM P WHERE age = 30 extra", allowLiterals=True), G("SELECT * FROM P WHERE `my prop` = 1", allowLiterals=True),
  G("SELECT * FROM P WHERE age = -30", allowLiterals=True), G("SELECT * FROM P WHERE score = 1e3", allowLiterals=True), G("SELECT * FROM P WHERE name = 'it''s'", allowLiterals=True),
  {"partitionId": {"namespaceId": "ns"}, "gqlQuery": {"queryString": "SELECT * FROM P"}},
])

write("query_pseudo", "Metadata pseudo-kinds: __kind__, __property__, __namespace__.", [
  RQ(kind=kind("__kind__")), RQ(kind=kind("__kind__"), projection=proj("__key__")), RQ(kind=kind("__property__")),
  RQ(kind=kind("__property__"), filter=pf("__key__","HAS_ANCESTOR",{"keyValue":K("__kind__","P")})),
  RQ(kind=kind("__property__"), projection=proj("__key__")), RQ(kind=kind("__namespace__")),
  RQ(kind=kind("__kind__"), filter=pf("__key__","GREATER_THAN",{"keyValue":K("__kind__","M")})),
  {"partitionId": {"namespaceId": "ns"}, "query": {"kind": kind("__kind__")}}, RQ(kind=kind("__entity_group__")), RQ(kind=kind("__Stat_Total__")),
  RQ(kind=kind("__kind__"), order=[order("__key__","DESCENDING")]), RQ(kind=kind("__property__"), projection=proj("property_representation")),
  ("/v1/projects/p:lookup", {"keys": [K("__entity_group__", 1, "P", "a")]}, []), ("/v1/projects/p:lookup", {"keys": [K("P","a","__entity_group__",1)]}, []),
])

big = {"mode":"NON_TRANSACTIONAL","mutations":[ent(K("Big", i), n=I(i % 7), s=S("x" * 100)) for i in range(1, 351)]}
write("query_batches", "Result batching: large result sets, NOT_FINISHED batches and continuation.", [
  (RQ(kind=kind("Big"), projection=proj("__key__")), ["b1 = batch.endCursor"]), RQ(kind=kind("Big"), projection=proj("__key__"), startCursor="{{b1}}"),
  RQ(kind=kind("Big"), projection=proj("__key__"), limit=400), RQ(kind=kind("Big"), projection=proj("__key__"), offset=310),
  RQ(kind=kind("Big"), projection=proj("__key__"), offset=1000), RQ(kind=kind("Big")),
  RQ(kind=kind("Big"), projection=proj("n"), distinctOn=[{"name":"n"}]), RQ(kind=kind("Big"), filter=pf("n","EQUAL",I(3)), projection=proj("__key__")),
], seed=big)
print("ok")
