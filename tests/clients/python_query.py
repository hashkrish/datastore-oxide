"""Query smoke test with the official google-cloud-datastore client (gRPC).

    DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_query.py
"""

import os

from google.cloud import datastore
from google.cloud.datastore.query import PropertyFilter

assert os.environ.get("DATASTORE_EMULATOR_HOST"), "set DATASTORE_EMULATOR_HOST"
client = datastore.Client(project="query-test")

people = []
for i, (name, age, tags) in enumerate([("ann", 30, ["x"]), ("bob", 25, ["y", "z"]), ("cid", 35, []), ("dan", 30, ["x", "z"])]):
    e = datastore.Entity(client.key("Person", name))
    e.update({"age": age, "tags": tags, "n": i})
    people.append(e)
client.put_multi(people)
parent = client.key("Person", "ann")
client.put_multi([datastore.Entity(client.key("Pet", f"p{i}", parent=parent)) for i in range(3)])

def names(q, **kw):
    return [e.key.name for e in q.fetch(**kw)]

q = client.query(kind="Person")
assert names(q) == ["ann", "bob", "cid", "dan"], names(q)

q = client.query(kind="Person", filters=[PropertyFilter("age", "=", 30)])
assert names(q) == ["ann", "dan"]

q = client.query(kind="Person", filters=[PropertyFilter("age", ">", 26)], order=["-age"])
assert names(q) == ["cid", "ann", "dan"], names(q)

q = client.query(kind="Person", filters=[PropertyFilter("tags", "=", "z")])
assert names(q) == ["bob", "dan"]

q = client.query(kind="Person", projection=["age"], distinct_on=["age"])
assert [(e.key.name, e["age"]) for e in q.fetch()] == [("bob", 25), ("ann", 30), ("cid", 35)]

q = client.query(kind="Person")
q.keys_only()
assert [k.key.name for k in q.fetch()] == ["ann", "bob", "cid", "dan"]

q = client.query(kind="Pet", ancestor=parent)
assert names(q) == ["p0", "p1", "p2"]

# Paging with cursors, and a result set larger than one 300-entity batch.
it = client.query(kind="Person", order=["n"]).fetch(limit=2)
page = next(it.pages)
first = [e.key.name for e in page]
rest = names(client.query(kind="Person", order=["n"]), start_cursor=it.next_page_token)
assert first + rest == ["ann", "bob", "cid", "dan"], (first, rest)

batch = [datastore.Entity(client.key("Many", i + 1)) for i in range(450)]
for chunk in range(0, 450, 200):
    client.put_multi(batch[chunk:chunk + 200])
assert len(list(client.query(kind="Many").fetch())) == 450

print("OK: python client queries")
