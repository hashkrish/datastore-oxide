"""CRUD smoke test with the official google-cloud-datastore client (gRPC).

Run against a started emulator:
    DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_crud.py
"""

import datetime
import os
import sys

from google.cloud import datastore

assert os.environ.get("DATASTORE_EMULATOR_HOST"), "set DATASTORE_EMULATOR_HOST"
client = datastore.Client(project="smoke-test", namespace="ns1")

# Incomplete key: id allocated by the commit.
task = datastore.Entity(client.key("Task"), exclude_from_indexes=("notes",))
when = datetime.datetime(2024, 1, 2, 3, 4, 5, 123456, tzinfo=datetime.timezone.utc)
embedded = datastore.Entity(client.key("Embedded", "e"))
embedded.update({"x": 1})
task.update(
    {
        "title": "write tests",
        "done": False,
        "priority": 4,
        "score": 1.5,
        "when": when,
        "tags": ["a", "b"],
        "notes": "x" * 2000,
        "blob": b"\x00\x01",
        "owner": client.key("User", "alice"),
        "location": datastore.helpers.GeoPoint(1.5, -2.0),
        "nothing": None,
        "embedded": embedded,
    }
)
client.put(task)
assert task.key.id is not None, task.key
print("put ->", task.key)

fetched = client.get(task.key)
assert fetched is not None
for name, value in task.items():
    got = fetched[name]
    if isinstance(value, datastore.Entity):
        assert dict(got) == dict(value) and got.key == value.key, (name, got, value)
    else:
        assert got == value, (name, got, value)
assert fetched.exclude_from_indexes == {"notes"}, fetched.exclude_from_indexes
print("get -> all", len(task), "properties round-tripped")

# Named keys, multi put/get/delete, missing keys.
alice, bob = client.key("User", "alice"), client.key("User", "bob")
users = [datastore.Entity(alice), datastore.Entity(bob)]
users[0]["age"], users[1]["age"] = 30, 40
client.put_multi(users)
missing = []
got = client.get_multi([alice, bob, client.key("User", "nobody")], missing=missing)
assert sorted(e["age"] for e in got) == [30, 40], got
assert [m.key for m in missing] == [client.key("User", "nobody")], missing

users[0]["age"] = 31
client.put(users[0])
assert client.get(alice)["age"] == 31

client.delete_multi([alice, task.key])
assert client.get(alice) is None and client.get(task.key) is None
assert client.get(bob)["age"] == 40

# Id allocation and reservation.
ids = client.allocate_ids(client.key("Task"), 3)
assert len({k.id for k in ids}) == 3 and all(k.id >= 1 << 52 for k in ids), ids
client.reserve_ids_sequential(client.key("Task", 10), 5)
print("allocate_ids ->", [k.id for k in ids])

print("OK: python client CRUD round-trips")
sys.exit(0)
