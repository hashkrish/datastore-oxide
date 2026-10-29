import json, os
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "tests", "conformance", "scenarios")
def K(*path, ns=None):
    els = []; it = iter(path)
    for kind, idn in zip(it, it):
        e = {"kind": kind}
        if idn is not None: e["id" if isinstance(idn, int) else "name"] = str(idn)
        els.append(e)
    k = {"path": els}
    if ns: k["partitionId"] = {"namespaceId": ns}
    return k
def I(v): return {"integerValue": str(v)}
def up(key, **p): return {"upsert": {"key": key, "properties": p}}
def ins(key, **p): return {"insert": {"key": key, "properties": p}}
def dele(key): return {"delete": key}
P = "/v1/projects/p:"
def begin(**opts): return (P+"beginTransaction", opts)
def commit_t(t, *muts): return (P+"commit", {"mode": "TRANSACTIONAL", "transaction": "{{%s}}" % t, "mutations": list(muts)})
def commit_nt(*muts): return (P+"commit", {"mode": "NON_TRANSACTIONAL", "mutations": list(muts)})
def lookup(*keys, t=None, **ro):
    body = {"keys": list(keys)}
    if t: body["readOptions"] = {"transaction": "{{%s}}" % t}
    if ro: body["readOptions"] = ro
    return (P+"lookup", body)
def query(q, t=None, **ro):
    body = {"query": q}
    if t: body["readOptions"] = {"transaction": "{{%s}}" % t}
    if ro: body["readOptions"] = ro
    return (P+"runQuery", body)
def rollback(t): return (P+"rollback", {"transaction": "{{%s}}" % t})
def cap(step, *caps): return step + (list(caps),)
def write(name, comment, steps):
    lines = [f"# {comment}"]
    for s in steps:
        path, body = s[0], s[1]
        caps = s[2] if len(s) > 2 else []
        method = "GET" if body is None else ("PATCH" if path.endswith("/v1/config") else "POST")
        lines.append(f"{method} {path}")
        if body is not None: lines.append(body if isinstance(body, str) else json.dumps(body))
        lines += [f"> {c}" for c in caps]
        lines.append("")
    open(os.path.join(OUT, name + ".scn"), "w").write("\n".join(lines))
KA, KB, KC = K("K","a"), K("K","b"), K("K","a","C",1)
ancq = {"kind": [{"name": "C"}], "filter": {"propertyFilter": {"property": {"name": "__key__"}, "op": "HAS_ANCESTOR", "value": {"keyValue": KA}}}}

write("txn_basic", "Transaction lifecycle: begin, transactional reads, commit, rollback, handle reuse.", [
  commit_nt(up(KA, v=I(1))),
  cap(begin(), "t1 = transaction"), cap(begin(), "t2 = transaction"),
  lookup(KA, KB, t="t1"),
  commit_t("t1", up(KA, v=I(2)), ins(K("K",None)), up(KB, v=I(1))),
  lookup(KA, KB),
  commit_t("t1", up(KA, v=I(3))), lookup(KA, t="t1"), rollback("t1"),
  rollback("t2"), rollback("t2"), commit_t("t2"), lookup(KA, t="t2"),
  (P+"rollback", {"transaction": "AAAA"}), (P+"commit", {"mode": "TRANSACTIONAL", "transaction": "EQkAAAAAAAAA"}),
  (P+"rollback", {}), (P+"commit", {"mode": "TRANSACTIONAL", "transaction": ""}),
  cap(begin(), "t3 = transaction"), commit_t("t3"),
  cap(begin(), "t4 = transaction"), commit_t("t4", up(KA, v=I(5)), up(KA, v=I(6))), lookup(KA),
  cap(begin(), "t5 = transaction"), commit_t("t5", up(KC, v=I(1)), dele(KC)), lookup(KC),
  cap(begin(), "t6 = transaction"), commit_t("t6", ins(KA)),  lookup(KA),
  cap(begin(), "t7 = transaction"), commit_t("t7", up(KA, v=I(6))),
  cap(begin(), "t8 = transaction"), (P+"commit", {"mode": "NON_TRANSACTIONAL", "transaction": "{{t8}}"}),
  (P+"commit", {"transaction": "{{t8}}", "mutations": [up(KB, v=I(9))]}), lookup(KB),
  cap((P+"beginTransaction", {"databaseId": "db"}), "t9 = transaction"), commit_t("t9", up(KA)),
  ("/v1/projects/q:commit", {"mode": "TRANSACTIONAL", "transaction": "{{t2}}"}),
])

write("txn_contention", "Optimistic concurrency: conflicting writes abort transactions; snapshot reads.", [
  commit_nt(up(KA, v=I(1)), up(KB, v=I(1))),
  cap(begin(), "t1 = transaction"), lookup(KA, t="t1"),
  commit_nt(up(KA, v=I(2))),
  lookup(KA, t="t1"), lookup(KC, t="t1"),
  commit_t("t1", up(KA, v=I(3))), lookup(KA),
  cap(begin(), "t2 = transaction"), cap(begin(), "t3 = transaction"),
  lookup(KA, t="t2"), lookup(KA, t="t3"),
  commit_t("t2", up(KA, v=I(10))), commit_t("t3", up(KA, v=I(11))), lookup(KA),
  cap(begin(), "t4 = transaction"), lookup(KB, t="t4"), commit_nt(up(KA, v=I(4))), commit_t("t4", up(KB, v=I(4))),
  cap(begin(), "t5 = transaction"), lookup(KA, t="t5"), commit_nt(up(KC, v=I(1))), commit_t("t5", up(KB, v=I(5))),
  cap(begin(), "t6 = transaction"), commit_nt(up(KA, v=I(7))), commit_t("t6", up(KA, v=I(8))), lookup(KA),
  cap(begin(), "t7 = transaction"), lookup(KA, t="t7"), commit_nt(up(KA, v=I(7))), commit_t("t7", up(KA, v=I(9))),
  cap(begin(), "t8 = transaction"), lookup(KA, t="t8"), commit_nt(dele(K("K","nope"))), commit_t("t8", up(KA, v=I(12))),
  cap(begin(), "t9 = transaction"), query(ancq, t="t9"), commit_nt(up(KC, v=I(2))), query(ancq, t="t9"), commit_t("t9", up(KC, v=I(3))),
  cap(begin(), "t10 = transaction"), lookup(KA, t="t10"), commit_t("t10", up(KA, v=I(13))), lookup(KA, t="t10"),
])

groups = [up(K("G", i), v=I(i)) for i in range(1, 27)]
write("txn_limits", "Read-only transactions, entity-group limits, queries in transactions, new/single-use transactions.", [
  commit_nt(up(KA, v=I(1)), up(KC, v=I(1))),
  cap(begin(transactionOptions={"readOnly": {}}), "r1 = transaction"), lookup(KA, t="r1"), commit_t("r1", up(KA, v=I(2))),
  cap(begin(transactionOptions={"readOnly": {}}), "r2 = transaction"), commit_t("r2"), rollback("r2"),
  cap(begin(transactionOptions={"readOnly": {"readTime": "2020-01-01T00:00:00Z"}}), "r3 = transaction"), lookup(KA, t="r3"),
  cap(begin(transactionOptions={"readWrite": {"previousTransaction": "{{r1}}"}}), "w1 = transaction"),
  begin(transactionOptions={"readOnly": {}, "readWrite": {}}),
  cap(begin(), "g1 = transaction"), commit_t("g1", *groups[:25]),
  cap(begin(), "g2 = transaction"), commit_t("g2", *groups),
  cap(begin(), "g3 = transaction"), lookup(*[K("H", i) for i in range(1, 27)], t="g3"), commit_t("g3"),
  cap(begin(), "q1 = transaction"), query({"kind": [{"name": "K"}]}, t="q1"), query(ancq, t="q1"),
  query({"filter": {"propertyFilter": {"property": {"name": "__key__"}, "op": "HAS_ANCESTOR", "value": {"keyValue": KA}}}}, t="q1"),
  cap(lookup(KA, newTransaction={}), "n1 = transaction"), commit_t("n1", up(KA, v=I(5))), lookup(KA),
  cap(lookup(KA, newTransaction={"readOnly": {}}), "n2 = transaction"), commit_t("n2", up(KA, v=I(6))),
  cap(query(ancq, newTransaction={}), "n3 = transaction"), commit_t("n3"),
  lookup(KA, readConsistency="STRONG", transaction="AAAA"),
  (P+"commit", {"mode": "TRANSACTIONAL", "singleUseTransaction": {}, "mutations": [up(KA, v=I(7))]}), lookup(KA),
  (P+"commit", {"mode": "TRANSACTIONAL", "singleUseTransaction": {"readOnly": {}}, "mutations": [up(KA, v=I(8))]}),
  (P+"commit", {"mode": "NON_TRANSACTIONAL", "singleUseTransaction": {}, "mutations": []}),
  lookup(KA, readTime="2020-01-01T00:00:00Z"),
])

CFG = "/v1/config"
write("consistency", "Eventual consistency via the emulator config: unapplied writes are invisible to global queries until rolled forward.", [
  (CFG, {"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "1", "unappliedJobPercentage": 100}}}),
  commit_nt(up(KA, v=I(1)), up(KB, v=I(1)), up(KC, v=I(1))),
  query({"kind": [{"name": "K"}]}), query({"kind": [{"name": "C"}]}),
  lookup(KA), query({"kind": [{"name": "K"}]}), query({"kind": [{"name": "C"}]}), query(ancq), query({"kind": [{"name": "C"}]}),
  commit_nt(up(KA, v=I(2))), query({"kind": [{"name": "K"}]}), lookup(KA, readConsistency="EVENTUAL"), query({"kind": [{"name": "K"}]}),
  commit_nt(dele(KB)), query({"kind": [{"name": "K"}]}), query({"kind": [{"name": "K"}], "projection": [{"property": {"name": "__key__"}}]}),
  cap(begin(), "t1 = transaction"), commit_t("t1", up(K("T","x"), v=I(1))), query({"kind": [{"name": "T"}]}), lookup(K("T","x")), query({"kind": [{"name": "T"}]}),
  (CFG, {"jobPolicy": {"forceStrongConsistency": True}}), query({"kind": [{"name": "K"}]}),
  commit_nt(up(K("U","u"), v=I(1))), query({"kind": [{"name": "U"}]}),
  (CFG, {"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "1", "unappliedJobPercentage": 0}}}),
  (CFG, {"idAllocationPolicy": {"policy": "SCATTERED"}}), commit_nt(ins(K("S",None))),
  (CFG, {"idAllocationPolicy": {"policy": "SEQUENTIAL"}}), commit_nt(ins(K("S",None))),
  (CFG, {}), (CFG, {"jobPolicy": {"probabilityJobPolicy": {"unappliedJobPercentage": 150}}}),
  (CFG, {"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "1"}}}), (CFG, None),
])
write("consistency_seeded", "Probabilistic job application with a fixed seed.", [
  (CFG, {"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "42", "unappliedJobPercentage": 50}}}),
  commit_nt(*[up(K("R", i), v=I(i)) for i in range(1, 21)]),
  query({"kind": [{"name": "R"}], "projection": [{"property": {"name": "__key__"}}]}),
  commit_nt(*[up(K("R", i), v=I(i + 100)) for i in range(1, 21)]),
  query({"kind": [{"name": "R"}], "projection": [{"property": {"name": "v"}}]}),
  lookup(K("R", 3)), query({"kind": [{"name": "R"}], "projection": [{"property": {"name": "v"}}]}),
  (CFG, {"jobPolicy": {"probabilityJobPolicy": {"randomSeed": "1793075492023", "unappliedJobPercentage": 0}}}),
])
print("ok")

write("txn_failures", "Handles after contention (failed transactions), retries with previousTransaction, read-only edge cases.", [
  commit_nt(up(KA, v=I(1)), up(KB, v=I(1))),
  cap(begin(), "t1 = transaction"), lookup(KA, t="t1"), commit_nt(up(KA, v=I(2))),
  commit_t("t1", up(KA, v=I(3))), commit_t("t1", up(KA, v=I(3))), lookup(KA, t="t1"), commit_t("t1"),
  rollback("t1"), rollback("t1"),
  cap(begin(), "t2 = transaction"), lookup(KA, t="t2"), commit_nt(up(KA, v=I(4))), lookup(KB, t="t2"), lookup(KA, t="t2"), rollback("t2"),
  cap(begin(), "t3 = transaction"), lookup(KA, t="t3"), commit_nt(up(KA, v=I(5))), commit_t("t3"),
  cap(begin(), "t4 = transaction"), lookup(KA, t="t4"), commit_nt(up(KA, v=I(6))), commit_t("t4", up(KA, v=I(7))),
  cap(begin(transactionOptions={"readWrite": {"previousTransaction": "{{t4}}"}}), "t5 = transaction"), commit_t("t4"), lookup(KA, t="t5"), commit_t("t5", up(KA, v=I(8))), lookup(KA),
  cap(begin(transactionOptions={"readWrite": {"previousTransaction": "EQEAAAAAAAAA"}}), "t6 = transaction"),
  begin(transactionOptions={"readWrite": {"previousTransaction": "AAAA"}}),
  cap(begin(transactionOptions={"readOnly": {}}), "r1 = transaction"), lookup(KA, t="r1"), commit_nt(up(KA, v=I(9))), lookup(KA, t="r1"), commit_t("r1"),
  cap(begin(), "t7 = transaction"), commit_t("t7", up(K("K",None)), ins(K("K",None))),
  cap(begin(), "t8 = transaction"), commit_t("t8", ins(KC), ins(KC)), lookup(KC),
  cap(begin(), "t9 = transaction"), commit_t("t9", dele(KC), {"update": {"key": KC, "properties": {}}}), lookup(KC),
  cap(begin(), "t10 = transaction"), lookup(K("K","a","__entity_group__",1), t="t10"),
])
