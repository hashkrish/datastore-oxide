"""Transaction smoke test with the official google-cloud-datastore client (gRPC).

    DATASTORE_EMULATOR_HOST=localhost:8081 uv run --with google-cloud-datastore python tests/clients/python_txn.py
"""

import os

from google.api_core import exceptions
from google.cloud import datastore

assert os.environ.get("DATASTORE_EMULATOR_HOST"), "set DATASTORE_EMULATOR_HOST"
client = datastore.Client(project="txn-test")
key = client.key("Counter", "c")
counter = datastore.Entity(key)
counter["n"] = 0
client.put(counter)

# Commit: read-modify-write.
with client.transaction():
    c = client.get(key)
    c["n"] += 1
    client.put(c)
assert client.get(key)["n"] == 1

# Rollback on error.
try:
    with client.transaction():
        c = client.get(key)
        c["n"] = 100
        client.put(c)
        raise RuntimeError("abort")
except RuntimeError:
    pass
assert client.get(key)["n"] == 1

# Contention: two transactions read the same group; the second commit fails.
t1, t2 = client.transaction(), client.transaction()
t1.begin()
t2.begin()
a, b = client.get(key, transaction=t1), client.get(key, transaction=t2)
a["n"], b["n"] = 10, 20
t1.put(a)
t2.put(b)
t1.commit()
try:
    t2.commit()
    raise AssertionError("expected contention")
except exceptions.Aborted as e:
    assert "too much contention" in str(e), e
assert client.get(key)["n"] == 10

# Read-only transactions can read.
with client.transaction(read_only=True) as ro:
    assert client.get(key, transaction=ro)["n"] == 10
# (The client itself refuses writes to read-only transactions; the server error is covered by
# the REST conformance scenarios.)

# Incomplete keys in a transaction are completed at commit.
with client.transaction() as t:
    e = datastore.Entity(client.key("Item"))
    t.put(e)
assert e.key.id is not None

print("OK: python client transactions")
