//! Production features beyond the Java emulator, on by default (docs/superset-backlog.md). The
//! conformance tests run with `--no_superset` and check that responses stay exactly Java's.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

struct Emulator {
    child: Child,
    base: String,
}

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn start(extra: &[&str]) -> Emulator {
    let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        .args(["start", "--testing", "--port=0"])
        .args(extra)
        .env("OXIDE_LOG", "off")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port: u16 = first.rsplit(':').next().unwrap().parse().unwrap();
    std::thread::spawn(move || lines.for_each(drop));
    Emulator {
        child,
        base: format!("http://127.0.0.1:{port}"),
    }
}

impl Emulator {
    fn post(&self, method: &str, body: Value) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut r = agent
            .post(format!("{}/v1/projects/p:{method}", self.base))
            .header("content-type", "application/json")
            .send(body.to_string())
            .unwrap();
        let status = r.status().as_u16();
        (
            status,
            serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap(),
        )
    }

    /// Inserts `A` entities with the given `n` property values (`null` leaves `n` out).
    fn seed(&self, values: &[Value]) {
        let mutations: Vec<Value> = values
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut properties = json!({"tag": {"stringValue": "x"}});
                if !n.is_null() {
                    properties["n"] = n.clone();
                }
                json!({"insert": {
                    "key": {"path": [{"kind": "A", "id": (i + 1).to_string()}]},
                    "properties": properties,
                }})
            })
            .collect();
        let (status, _) = self.post(
            "commit",
            json!({"mode": "NON_TRANSACTIONAL", "mutations": mutations}),
        );
        assert_eq!(status, 200);
    }

    /// The aggregate properties of a successful aggregation query.
    fn aggregate(&self, request: Value) -> Value {
        let (status, body) = self.post("runAggregationQuery", request);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["batch"]["moreResults"], "NO_MORE_RESULTS");
        body["batch"]["aggregationResults"][0]["aggregateProperties"].clone()
    }
}

fn gql(query: &str) -> Value {
    json!({"gqlQuery": {"queryString": query, "allowLiterals": true}})
}

#[test]
fn no_superset_turns_aggregation_queries_off() {
    let emulator = start(&["--no_superset"]);
    let (status, body) = emulator.post("runAggregationQuery", gql("SELECT COUNT(*) FROM A"));
    assert_eq!(status, 501);
    assert_eq!(
        body["error"]["message"],
        "Method google.datastore.v1.Datastore/RunAggregationQuery is unimplemented"
    );
}

#[test]
fn structured_aggregations() {
    let emulator = start(&[]);
    emulator.seed(&[
        json!({"integerValue": "1"}),
        json!({"integerValue": "2"}),
        json!({"doubleValue": 0.5}),
        json!({"stringValue": "skipped"}),
        Value::Null,
    ]);
    let request = |nested: Value, aggregations: Value| json!({"aggregationQuery": {"nestedQuery": nested, "aggregations": aggregations}});
    assert_eq!(
        emulator.aggregate(request(
            json!({"kind": [{"name": "A"}]}),
            json!([
                {"count": {}, "alias": "all"},
                {"count": {"upTo": "3"}},
                {"sum": {"property": {"name": "n"}}, "alias": "total"},
                {"avg": {"property": {"name": "n"}}, "alias": "mean"},
            ]),
        )),
        json!({
            "all": {"integerValue": "5"},
            "mean": {"doubleValue": 3.5 / 3.0},
            "property_1": {"integerValue": "3"},
            "total": {"doubleValue": 3.5},
        })
    );

    // Filters, offset and limit apply before aggregating; SUM over integers stays an integer.
    assert_eq!(
        emulator.aggregate(request(
            json!({
                "kind": [{"name": "A"}],
                // Both bounds are integers: doubles and strings sort after every integer.
                "filter": {"compositeFilter": {"op": "AND", "filters": [
                    {"propertyFilter": {
                        "property": {"name": "n"}, "op": "GREATER_THAN_OR_EQUAL",
                        "value": {"integerValue": "1"},
                    }},
                    {"propertyFilter": {
                        "property": {"name": "n"}, "op": "LESS_THAN",
                        "value": {"integerValue": "100"},
                    }},
                ]}},
                "order": [{"property": {"name": "n"}}],
                "offset": 1,
                "limit": 5,
            }),
            json!([{"count": {}, "alias": "c"}, {"sum": {"property": {"name": "n"}}, "alias": "s"}]),
        )),
        json!({"c": {"integerValue": "1"}, "s": {"integerValue": "2"}})
    );

    // Nothing to aggregate.
    assert_eq!(
        emulator.aggregate(request(
            json!({"kind": [{"name": "Empty"}]}),
            json!([
                {"count": {}, "alias": "c"},
                {"sum": {"property": {"name": "n"}}, "alias": "s"},
                {"avg": {"property": {"name": "n"}}, "alias": "a"},
            ]),
        )),
        json!({"a": {"nullValue": null}, "c": {"integerValue": "0"}, "s": {"integerValue": "0"}})
    );

    let (status, body) = emulator.post(
        "runAggregationQuery",
        request(json!({"kind": [{"name": "A"}]}), json!([])),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "At least one aggregation is required."
    );
}

#[test]
fn counts_past_the_batch_size() {
    let emulator = start(&[]);
    emulator.seed(&vec![Value::Null; 450]);
    assert_eq!(
        emulator.aggregate(gql("SELECT COUNT(*) AS c FROM A")),
        json!({"c": {"integerValue": "450"}})
    );
}

#[test]
fn gql_aggregations() {
    let emulator = start(&[]);
    emulator.seed(&[json!({"integerValue": "1"}), json!({"integerValue": "5"})]);
    let (status, body) = emulator.post(
        "runAggregationQuery",
        gql("AGGREGATE COUNT(*) AS c, SUM(n) AS s OVER (SELECT * FROM A WHERE n > 1)"),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["batch"]["aggregationResults"][0]["aggregateProperties"],
        json!({"c": {"integerValue": "1"}, "s": {"integerValue": "5"}})
    );
    // GQL requests echo the parsed query, as RunQuery does.
    assert_eq!(body["query"]["aggregations"][0]["alias"], "c");

    assert_eq!(
        emulator.aggregate(gql("SELECT COUNT_UP_TO(1) AS c, AVG(n) AS a FROM A")),
        json!({"a": {"doubleValue": 3.0}, "c": {"integerValue": "1"}})
    );

    // RunQuery itself still rejects aggregation syntax.
    let (status, _) = emulator.post("runQuery", gql("SELECT COUNT(*) FROM A"));
    assert_eq!(status, 400);
}

#[test]
fn transactions_need_ancestor_queries() {
    let emulator = start(&[]);
    let (status, body) = emulator.post(
        "runAggregationQuery",
        json!({
            "readOptions": {"newTransaction": {}},
            "gqlQuery": {"queryString": "SELECT COUNT(*) FROM A"},
        }),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "Only ancestor queries are allowed inside transactions."
    );

    let (status, body) = emulator.post(
        "runAggregationQuery",
        json!({
            "readOptions": {"newTransaction": {}},
            "gqlQuery": {
                "queryString": "SELECT COUNT(*) AS c FROM A WHERE __key__ HAS ANCESTOR KEY(A, 1)",
                "allowLiterals": true,
            },
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body["transaction"].is_string());
}

#[test]
fn production_commit_limits() {
    let commit = |n: usize, blob_bytes: usize| {
        let mutations: Vec<Value> = (0..n)
            .map(|i| {
                json!({"upsert": {
                    "key": {"path": [{"kind": "L", "id": (i + 1).to_string()}]},
                    "properties": {"b": {
                        "blobValue": base64(blob_bytes),
                        "excludeFromIndexes": true,
                    }},
                }})
            })
            .collect();
        json!({"mode": "NON_TRANSACTIONAL", "mutations": mutations})
    };
    for (flags, limited) in [(&[][..], true), (&["--no_superset"][..], false)] {
        let emulator = start(flags);
        assert_eq!(emulator.post("commit", commit(500, 1)).0, 200);
        let (status, body) = emulator.post("commit", commit(501, 1));
        if limited {
            assert_eq!(status, 400);
            assert_eq!(
                body["error"]["message"],
                "cannot write more than 500 entities in a single call"
            );
        } else {
            assert_eq!(status, 200);
        }
        assert_eq!(emulator.post("commit", commit(1, 1_000_000)).0, 200);
        let (status, body) = emulator.post("commit", commit(1, 1_048_576));
        if limited {
            assert_eq!(status, 400);
            assert_eq!(body["error"]["message"], "entity is too big");
        } else {
            assert_eq!(status, 200);
        }
    }
}

/// Base64 of `n` zero bytes.
fn base64(n: usize) -> String {
    let mut s = "AAAA".repeat(n / 3);
    match n % 3 {
        1 => s.push_str("AA=="),
        2 => s.push_str("AAA="),
        _ => {}
    }
    s
}

#[test]
fn rest_accepts_production_request_fields() {
    let lookup = json!({
        "keys": [{"path": [{"kind": "A", "name": "a"}]}],
        "requestOptions": {"requestTags": ["t"]},
    });
    let (status, _) = start(&[]).post("lookup", lookup.clone());
    assert_eq!(status, 200);
    let (status, body) = start(&["--no_superset"]).post("lookup", lookup);
    assert_eq!(status, 400);
    assert_eq!(body["error"]["message"], "Payload isn't valid for request.");
}

/// Names of the `A` entities a query returns, in order.
fn names(body: &Value) -> Vec<String> {
    body["batch"]["entityResults"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .map(|r| {
                    r["entity"]["key"]["path"][0]["name"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn seed_named(emulator: &Emulator, rows: &[(&str, Value)]) {
    let mutations: Vec<Value> = rows
        .iter()
        .map(|(name, props)| {
            json!({"insert": {"key": {"path": [{"kind": "A", "name": name}]}, "properties": props}})
        })
        .collect();
    let (status, body) = emulator.post(
        "commit",
        json!({"mode": "NON_TRANSACTIONAL", "mutations": mutations}),
    );
    assert_eq!(status, 200, "{body}");
}

fn int(n: i64) -> Value {
    json!({"integerValue": n.to_string()})
}

fn pf(name: &str, op: &str, value: Value) -> Value {
    json!({"propertyFilter": {"property": {"name": name}, "op": op, "value": value}})
}

fn composite(op: &str, filters: Vec<Value>) -> Value {
    json!({"compositeFilter": {"op": op, "filters": filters}})
}

fn run(emulator: &Emulator, query: Value) -> (u16, Value) {
    emulator.post("runQuery", json!({"query": query}))
}

fn filter_fixture(flags: &[&str]) -> Emulator {
    let emulator = start(flags);
    seed_named(
        &emulator,
        &[
            ("a", json!({"n": int(1), "c": {"stringValue": "red"}})),
            ("b", json!({"n": int(2), "c": {"stringValue": "blue"}})),
            ("c", json!({"n": int(3), "c": {"stringValue": "red"}})),
            (
                "d",
                json!({"n": {"nullValue": null}, "c": {"stringValue": "green"}}),
            ),
            ("e", json!({"c": {"stringValue": "blue"}})),
            (
                "f",
                json!({"n": {"arrayValue": {"values": [int(2), int(9)]}}}),
            ),
        ],
    );
    emulator
}

#[test]
fn disjunctive_filters() {
    let emulator = filter_fixture(&[]);
    let kind = json!([{"name": "A"}]);
    let query = |filter: Value| json!({"kind": kind, "filter": filter});
    let ok = |q: Value| {
        let (status, body) = run(&emulator, q);
        assert_eq!(status, 200, "{body}");
        names(&body)
    };

    // OR, deduplicated and in key order.
    assert_eq!(
        ok(query(composite(
            "OR",
            vec![
                pf("c", "EQUAL", json!({"stringValue": "red"})),
                pf("n", "EQUAL", int(2))
            ]
        ))),
        ["a", "b", "c", "f"]
    );
    // AND of an OR.
    assert_eq!(
        ok(query(composite(
            "AND",
            vec![
                composite(
                    "OR",
                    vec![pf("n", "EQUAL", int(1)), pf("n", "EQUAL", int(3))]
                ),
                pf("c", "EQUAL", json!({"stringValue": "red"})),
            ]
        ))),
        ["a", "c"]
    );
    // IN with several values; an array matches through any element.
    assert_eq!(
        ok(query(pf(
            "n",
            "IN",
            json!({"arrayValue": {"values": [int(2), int(3)]}})
        ))),
        ["b", "c", "f"]
    );
    // NOT_EQUAL and NOT_IN: ordered by the property, nulls and missing values excluded; an
    // array matches when some element is not excluded.
    assert_eq!(ok(query(pf("n", "NOT_EQUAL", int(2)))), ["a", "c", "f"]);
    assert_eq!(
        ok(query(pf(
            "n",
            "NOT_IN",
            json!({"arrayValue": {"values": [int(1), int(9)]}})
        ))),
        ["b", "f", "c"]
    );
    assert_eq!(
        ok(query(pf("n", "NOT_EQUAL", json!({"nullValue": null})))),
        ["a", "b", "f", "c"]
    );
    // An inequality in one branch orders everything by that property, descending here.
    let ordered = json!({
        "kind": kind,
        "filter": composite("OR", vec![pf("n", "GREATER_THAN", int(2)), pf("n", "EQUAL", int(1))]),
        "order": [{"property": {"name": "n"}, "direction": "DESCENDING"}],
    });
    assert_eq!(ok(ordered), ["f", "c", "a"]);
}

#[test]
fn disjunctive_filters_page_with_cursors() {
    let emulator = filter_fixture(&[]);
    let query = |cursor: Option<&str>| {
        let mut q = json!({
            "kind": [{"name": "A"}],
            "filter": composite(
                "OR",
                vec![pf("c", "EQUAL", json!({"stringValue": "blue"})), pf("n", "LESS_THAN", int(3))],
            ),
            "limit": 2,
        });
        if let Some(c) = cursor {
            q["startCursor"] = json!(c);
        }
        q
    };
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..4 {
        let (status, body) = run(&emulator, query(cursor.as_deref()));
        assert_eq!(status, 200, "{body}");
        let page = names(&body);
        if page.is_empty() {
            break;
        }
        seen.extend(page);
        cursor = body["batch"]["endCursor"].as_str().map(str::to_string);
    }
    // Ordered by n, the inequality property (null sorts first and is < 3). The implicit order on
    // n requires the property, so `e` (blue, without n) is left out, as in production.
    assert_eq!(seen, ["d", "a", "b", "f"]);
}

#[test]
fn disjunctive_filter_errors() {
    let emulator = filter_fixture(&[]);
    let kind = json!([{"name": "A"}]);
    let err = |filter: Value| {
        let (status, body) = run(&emulator, json!({"kind": kind, "filter": filter}));
        assert_eq!(status, 400, "{body}");
        body["error"]["message"].as_str().unwrap().to_string()
    };
    let array = |n: i64| json!({"arrayValue": {"values": (0..n).map(int).collect::<Vec<_>>()}});
    assert_eq!(
        err(pf("n", "NOT_IN", array(11))),
        "'NOT_IN' supports up to 10 comparison values."
    );
    assert_eq!(
        err(pf("n", "IN", array(31))),
        "The query contains more than the maximum 30 disjunctions."
    );
    assert_eq!(
        err(composite(
            "OR",
            vec![pf("n", "NOT_IN", array(2)), pf("c", "EQUAL", int(1))]
        )),
        "'NOT_IN' cannot be used in the same query with 'OR', 'IN' or 'NOT_EQUAL' filters."
    );

    // Java's behavior without the superset.
    let java = filter_fixture(&["--no_superset"]);
    let (status, body) = run(
        &java,
        json!({"kind": kind, "filter": composite("OR", vec![pf("n", "EQUAL", int(1))])}),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "unsupported composite property operator"
    );
}

#[test]
fn gql_disjunctive_filters() {
    let emulator = filter_fixture(&[]);
    let ok = |q: &str| {
        let (status, body) = emulator.post("runQuery", gql(q));
        assert_eq!(status, 200, "{body}");
        names(&body)
    };
    assert_eq!(
        ok("SELECT * FROM A WHERE n = 1 OR c = 'blue'"),
        ["a", "b", "e"]
    );
    assert_eq!(ok("SELECT * FROM A WHERE n IN ARRAY(1, 3)"), ["a", "c"]);
    assert_eq!(ok("SELECT * FROM A WHERE n != 2"), ["a", "c", "f"]);
    assert_eq!(ok("SELECT * FROM A WHERE n NOT IN ARRAY(1, 2, 3)"), ["f"]);
    assert_eq!(
        emulator.aggregate(gql(
            "AGGREGATE COUNT(*) AS c OVER (SELECT * FROM A WHERE c = 'red' OR c = 'green')"
        )),
        json!({"c": {"integerValue": "3"}})
    );
}

#[test]
fn multiple_inequality_properties() {
    let emulator = filter_fixture(&[]);
    let kind = json!([{"name": "A"}]);
    let ok = |q: Value| {
        let (status, body) = run(&emulator, q);
        assert_eq!(status, 200, "{body}");
        names(&body)
    };
    let both = composite(
        "AND",
        vec![
            pf("n", "GREATER_THAN", int(1)),
            pf("c", "GREATER_THAN", json!({"stringValue": "a"})),
        ],
    );
    // Implicit orders: c then n (by name), ascending.
    assert_eq!(ok(json!({"kind": kind, "filter": both})), ["b", "c"]);
    // An explicit order on another property comes first; the rest follow its direction.
    assert_eq!(
        ok(json!({
            "kind": kind,
            "filter": both,
            "order": [{"property": {"name": "n"}, "direction": "DESCENDING"}],
        })),
        ["c", "b"]
    );
    // Inequalities on different properties in different branches. Both are ordered implicitly,
    // so both must exist: `e` (no n) and `f` (no c) are left out, as in production.
    assert_eq!(
        ok(json!({
            "kind": kind,
            "filter": composite(
                "OR",
                vec![pf("n", "GREATER_THAN", int(2)), pf("c", "LESS_THAN", json!({"stringValue": "c"}))],
            ),
        })),
        ["b", "c"]
    );

    let java = filter_fixture(&["--no_superset"]);
    let (status, body) = run(&java, json!({"kind": kind, "filter": both}));
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "Only one inequality filter per query is supported.  Encountered both n and c"
    );
}

fn key(name: &str) -> Value {
    json!({"path": [{"kind": "T", "name": name}]})
}

fn lookup_properties(emulator: &Emulator, name: &str, mask: Option<Value>) -> Value {
    let mut request = json!({"keys": [key(name)]});
    if let Some(m) = mask {
        request["propertyMask"] = m;
    }
    let (status, body) = emulator.post("lookup", request);
    assert_eq!(status, 200, "{body}");
    body["found"][0]["entity"]["properties"].clone()
}

#[test]
fn property_transforms_and_masks() {
    let emulator = start(&[]);
    let commit = |mutations: Value| {
        let (status, body) = emulator.post(
            "commit",
            json!({"mode": "NON_TRANSACTIONAL", "mutations": mutations}),
        );
        assert_eq!(status, 200, "{body}");
        body["mutationResults"].clone()
    };
    commit(json!([{"insert": {"key": key("t"), "properties": {
        "n": int(5),
        "keep": {"stringValue": "k"},
        "tags": {"arrayValue": {"values": [{"stringValue": "a"}]}},
    }}}]));

    // Masked upsert with transforms: unmasked properties are kept, the counter is incremented
    // from its stored value.
    let results = commit(json!([{
        "upsert": {"key": key("t"), "properties": {"note": {"stringValue": "hi"}}},
        "propertyMask": {"paths": ["note"]},
        "propertyTransforms": [
            {"property": "n", "increment": int(2)},
            {"property": "n", "maximum": {"doubleValue": 6.5}},
            {"property": "tags", "appendMissingElements": {"values": [{"stringValue": "a"}, {"stringValue": "b"}]}},
            {"property": "seen", "setToServerValue": "REQUEST_TIME"},
        ],
    }]));
    let transform_results = results[0]["transformResults"].as_array().unwrap();
    assert_eq!(transform_results[0], int(7));
    assert_eq!(transform_results[1], json!({"integerValue": "7"}));
    assert_eq!(transform_results[2], json!({"nullValue": null}));
    assert!(transform_results[3]["timestampValue"].is_string());

    let properties = lookup_properties(&emulator, "t", None);
    assert_eq!(properties["n"], int(7));
    assert_eq!(properties["keep"], json!({"stringValue": "k"}));
    assert_eq!(properties["note"], json!({"stringValue": "hi"}));
    assert_eq!(
        properties["tags"],
        json!({"arrayValue": {"values": [{"stringValue": "a"}, {"stringValue": "b"}]}})
    );
    assert_eq!(properties["seen"], transform_results[3]);

    // A masked path the written entity lacks is removed.
    commit(json!([{"update": {"key": key("t")}, "propertyMask": {"paths": ["keep"]}}]));
    assert!(
        lookup_properties(&emulator, "t", None)
            .get("keep")
            .is_none()
    );

    // Read masks on lookup and queries.
    assert_eq!(
        lookup_properties(&emulator, "t", Some(json!({"paths": ["n", "missing"]}))),
        json!({"n": int(7)})
    );
    let (status, body) = emulator.post(
        "runQuery",
        json!({"query": {"kind": [{"name": "T"}]}, "propertyMask": {"paths": ["note"]}}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["batch"]["entityResults"][0]["entity"]["properties"],
        json!({"note": {"stringValue": "hi"}})
    );

    // Transforms in a transaction see the transaction's own writes.
    let (_, begin) = emulator.post("beginTransaction", json!({}));
    let (status, body) = emulator.post(
        "commit",
        json!({
            "transaction": begin["transaction"],
            "mutations": [
                {"upsert": {"key": key("u"), "properties": {"n": int(1)}}},
                {"update": {"key": key("u")}, "propertyMask": {"paths": []},
                 "propertyTransforms": [{"property": "n", "increment": {"doubleValue": 0.5}}]},
            ],
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["mutationResults"][1]["transformResults"][0],
        json!({"doubleValue": 1.5})
    );
    assert_eq!(
        lookup_properties(&emulator, "u", None)["n"],
        json!({"doubleValue": 1.5})
    );

    let (status, body) = emulator.post(
        "commit",
        json!({"mode": "NON_TRANSACTIONAL", "mutations": [{
            "upsert": {"key": key("t")},
            "propertyTransforms": [{"property": "n", "increment": {"stringValue": "x"}}],
        }]}),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "The increment value must be an integer or a double."
    );
}

#[test]
fn no_superset_ignores_masks() {
    let emulator = start(&["--no_superset"]);
    let commit = |mutation: Value| {
        let (status, _) = emulator.post(
            "commit",
            json!({"mode": "NON_TRANSACTIONAL", "mutations": [mutation]}),
        );
        assert_eq!(status, 200);
    };
    commit(json!({"insert": {"key": key("t"), "properties": {"a": int(1), "b": int(2)}}}));
    commit(
        json!({"upsert": {"key": key("t"), "properties": {"a": int(9)}}, "propertyMask": {"paths": ["a"]}}),
    );
    assert_eq!(
        lookup_properties(&emulator, "t", Some(json!({"paths": ["b"]}))),
        json!({"a": int(9)})
    );
}

/// A server timestamp after every commit so far and before the next one: the request time of
/// a commit that writes a marker entity.
fn server_time(emulator: &Emulator) -> Value {
    let (status, body) = emulator.post(
        "commit",
        json!({"mode": "NON_TRANSACTIONAL", "mutations": [{
            "upsert": {"key": {"path": [{"kind": "Marker", "name": "m"}]}},
            "propertyTransforms": [{"property": "t", "setToServerValue": "REQUEST_TIME"}],
        }]}),
    );
    assert_eq!(status, 200, "{body}");
    body["mutationResults"][0]["transformResults"][0]["timestampValue"].clone()
}

#[test]
fn point_in_time_reads() {
    let emulator = start(&[]);
    let commit = |mutations: Value| {
        let (status, body) = emulator.post(
            "commit",
            json!({"mode": "NON_TRANSACTIONAL", "mutations": mutations}),
        );
        assert_eq!(status, 200, "{body}");
    };
    commit(json!([
        {"upsert": {"key": key("a"), "properties": {"n": int(1)}}},
        {"upsert": {"key": key("b"), "properties": {"n": int(1)}}},
    ]));
    std::thread::sleep(std::time::Duration::from_millis(5));
    let then = server_time(&emulator);
    std::thread::sleep(std::time::Duration::from_millis(5));
    commit(json!([
        {"update": {"key": key("a"), "properties": {"n": int(2)}}},
        {"delete": key("b")},
        {"insert": {"key": key("c"), "properties": {"n": int(3)}}},
    ]));

    // Lookup as of then: a's old value, b still there, c not yet.
    let (status, body) = emulator.post(
        "lookup",
        json!({"keys": [key("a"), key("b"), key("c")], "readOptions": {"readTime": then}}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["readTime"], then);
    let found: Vec<(String, Value)> = body["found"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["entity"]["key"]["path"][0]["name"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                r["entity"]["properties"]["n"].clone(),
            )
        })
        .collect();
    assert_eq!(found, [("a".into(), int(1)), ("b".into(), int(1))]);
    assert_eq!(body["missing"][0]["entity"]["key"]["path"][0]["name"], "c");

    // Queries and aggregations as of then.
    let query = json!({"kind": [{"name": "T"}], "filter": pf("n", "GREATER_THAN", int(0))});
    let (status, body) = emulator.post(
        "runQuery",
        json!({"query": query, "readOptions": {"readTime": then}}),
    );
    assert_eq!(status, 200, "{body}");
    let names: Vec<&str> = body["batch"]["entityResults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["entity"]["key"]["path"][0]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(body["batch"]["readTime"], then);
    assert_eq!(
        emulator.aggregate(json!({
            "aggregationQuery": {"nestedQuery": query, "aggregations": [
                {"sum": {"property": {"name": "n"}}, "alias": "s"},
            ]},
            "readOptions": {"readTime": then},
        })),
        json!({"s": {"integerValue": "2"}})
    );
    // Now: a=2, c=3.
    assert_eq!(
        emulator.aggregate(json!({
            "aggregationQuery": {"nestedQuery": query, "aggregations": [
                {"sum": {"property": {"name": "n"}}, "alias": "s"},
            ]},
        })),
        json!({"s": {"integerValue": "5"}})
    );

    // A read-only transaction at then.
    let (status, begin) = emulator.post(
        "beginTransaction",
        json!({"transactionOptions": {"readOnly": {"readTime": then}}}),
    );
    assert_eq!(status, 200, "{begin}");
    let (status, body) = emulator.post(
        "lookup",
        json!({"keys": [key("a")], "readOptions": {"transaction": begin["transaction"]}}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["found"][0]["entity"]["properties"]["n"], int(1));

    // Too old.
    let (status, body) = emulator.post(
        "lookup",
        json!({"keys": [key("a")], "readOptions": {"readTime": "2000-01-01T00:00:00Z"}}),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "The read_time is older than the one-hour version retention period."
    );
}

#[test]
fn no_superset_ignores_read_time() {
    let emulator = start(&["--no_superset"]);
    let (status, _) = emulator.post(
        "commit",
        json!({"mode": "NON_TRANSACTIONAL", "mutations": [
            {"upsert": {"key": key("a"), "properties": {"n": int(1)}}},
        ]}),
    );
    assert_eq!(status, 200);
    let (status, body) = emulator.post(
        "lookup",
        json!({"keys": [key("a")], "readOptions": {"readTime": "2000-01-01T00:00:00Z"}}),
    );
    assert_eq!(status, 200);
    assert_eq!(body["found"][0]["entity"]["properties"]["n"], int(1));
    assert!(body.get("readTime").is_none());
}

#[test]
fn query_explain() {
    let emulator = filter_fixture(&[]);
    let query = json!({
        "kind": [{"name": "A"}],
        "filter": composite("AND", vec![
            pf("c", "EQUAL", json!({"stringValue": "red"})),
            pf("n", "GREATER_THAN", int(0)),
        ]),
        "order": [{"property": {"name": "n"}, "direction": "DESCENDING"}],
    });
    let index = json!([{"query_scope": "Kind", "properties": "(c ASC, n DESC, __key__ ASC)"}]);

    // Explain only: the plan, no results.
    let (status, body) = emulator.post("runQuery", json!({"query": query, "explainOptions": {}}));
    assert_eq!(status, 200, "{body}");
    assert!(body.get("batch").is_none());
    assert_eq!(body["explainMetrics"]["planSummary"]["indexesUsed"], index);
    assert!(body["explainMetrics"].get("executionStats").is_none());

    // Analyze: results and execution statistics.
    let (status, body) = emulator.post(
        "runQuery",
        json!({"query": query, "explainOptions": {"analyze": true}}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(names(&body), ["c", "a"]);
    let stats = &body["explainMetrics"]["executionStats"];
    assert_eq!(stats["resultsReturned"], "2");
    assert_eq!(stats["readOperations"], "2");
    assert_eq!(stats["debugStats"]["documents_scanned"], "2");
    assert!(stats["executionDuration"].is_string());

    // Disjunctions use one index each; aggregations explain their nested query.
    let (status, body) = emulator.post(
        "runAggregationQuery",
        json!({
            "aggregationQuery": {
                "nestedQuery": {"kind": [{"name": "A"}], "filter": composite("OR", vec![
                    pf("c", "EQUAL", json!({"stringValue": "red"})),
                    pf("n", "EQUAL", int(2)),
                ])},
                "aggregations": [{"count": {}, "alias": "c"}],
            },
            "explainOptions": {"analyze": true},
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["explainMetrics"]["planSummary"]["indexesUsed"],
        json!([
            {"query_scope": "Kind", "properties": "(c ASC, __key__ ASC)"},
            {"query_scope": "Kind", "properties": "(n ASC, __key__ ASC)"},
        ])
    );
    assert_eq!(
        body["explainMetrics"]["executionStats"]["resultsReturned"],
        "1"
    );
    assert_eq!(
        body["batch"]["aggregationResults"][0]["aggregateProperties"]["c"],
        json!({"integerValue": "4"})
    );
}

fn vector(components: &[f64]) -> Value {
    json!({
        "arrayValue": {"values": components.iter().map(|c| json!({"doubleValue": c})).collect::<Vec<_>>()},
        "meaning": 31,
        "excludeFromIndexes": true,
    })
}

#[test]
fn vector_search() {
    let emulator = start(&[]);
    seed_named(
        &emulator,
        &[
            (
                "x",
                json!({"v": vector(&[1.0, 0.0]), "c": {"stringValue": "a"}}),
            ),
            (
                "y",
                json!({"v": vector(&[0.0, 2.0]), "c": {"stringValue": "a"}}),
            ),
            (
                "z",
                json!({"v": vector(&[3.0, 3.0]), "c": {"stringValue": "b"}}),
            ),
            (
                "w",
                json!({"v": vector(&[1.0, 2.0, 3.0]), "c": {"stringValue": "a"}}),
            ),
            ("n", json!({"c": {"stringValue": "a"}})),
        ],
    );
    // Vectors round-trip.
    let (_, body) = emulator.post(
        "lookup",
        json!({"keys": [{"path": [{"kind": "A", "name": "x"}]}]}),
    );
    assert_eq!(
        body["found"][0]["entity"]["properties"]["v"],
        vector(&[1.0, 0.0])
    );

    let search = |measure: &str, extra: Value, filter: Option<Value>| {
        let mut nearest = json!({
            "vectorProperty": {"name": "v"},
            "queryVector": vector(&[1.0, 1.0]),
            "distanceMeasure": measure,
            "limit": 10,
        });
        for (k, v) in extra.as_object().unwrap() {
            nearest[k] = v.clone();
        }
        let mut query = json!({"kind": [{"name": "A"}], "findNearest": nearest});
        if let Some(f) = filter {
            query["filter"] = f;
        }
        let (status, body) = run(&emulator, query);
        assert_eq!(status, 200, "{body}");
        body
    };
    // Euclidean: x (1), y (~1.41), z (~2.83); w has another dimension, n no vector.
    let body = search("EUCLIDEAN", json!({"distanceResultProperty": "d"}), None);
    assert_eq!(names(&body), ["x", "y", "z"]);
    assert_eq!(
        body["batch"]["entityResults"][0]["entity"]["properties"]["d"],
        json!({"doubleValue": 1.0})
    );
    assert_eq!(body["batch"]["moreResults"], "NO_MORE_RESULTS");
    // Dot product: larger is nearer; the threshold keeps distances >= 2.
    assert_eq!(
        names(&search(
            "DOT_PRODUCT",
            json!({"distanceThreshold": 2.0}),
            None
        )),
        ["z", "y"]
    );
    // Cosine with a filter and a limit.
    assert_eq!(
        names(&search(
            "COSINE",
            json!({"limit": 1}),
            Some(pf("c", "EQUAL", json!({"stringValue": "a"})))
        )),
        ["x"]
    );

    let (status, body) = run(
        &emulator,
        json!({"kind": [{"name": "A"}], "findNearest": {
            "vectorProperty": {"name": "v"},
            "queryVector": vector(&[1.0]),
            "distanceMeasure": "EUCLIDEAN",
            "limit": 101,
        }}),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "find_nearest.limit must be between 1 and 100."
    );

    // Java rejects vector values.
    let java = start(&["--no_superset"]);
    let (status, body) = java.post(
        "commit",
        json!({"mode": "NON_TRANSACTIONAL", "mutations": [{"insert": {
            "key": {"path": [{"kind": "A", "name": "x"}]},
            "properties": {"v": vector(&[1.0])},
        }}]}),
    );
    assert_eq!(status, 400);
    assert_eq!(
        body["error"]["message"],
        "Exclude from indexes cannot be set on a list value"
    );
}

impl Emulator {
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}{path}", self.base);
        let mut r = match (method, body) {
            ("GET", _) => agent.get(&url).call(),
            ("DELETE", _) => agent.delete(&url).call(),
            (_, body) => agent
                .post(&url)
                .header("content-type", "application/json")
                .send(body.unwrap_or(json!({})).to_string()),
        }
        .unwrap();
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

#[test]
fn admin_indexes_and_operations() {
    let emulator = start(&["--require_indexes"]);
    let index = json!({
        "kind": "A",
        "ancestor": "NONE",
        "properties": [
            {"name": "c", "direction": "ASCENDING"},
            {"name": "n", "direction": "DESCENDING"},
        ],
    });
    let query = json!({"query": {
        "kind": [{"name": "A"}],
        "filter": pf("c", "EQUAL", json!({"stringValue": "x"})),
        "order": [{"property": {"name": "n"}, "direction": "DESCENDING"}],
    }});
    let (status, body) = emulator.post("runQuery", query.clone());
    assert_eq!(status, 400, "the query needs the composite index");
    assert_eq!(body["error"]["status"], "FAILED_PRECONDITION");

    let (status, op) = emulator.call("POST", "/v1/projects/p/indexes", Some(index.clone()));
    assert_eq!(status, 200, "{op}");
    assert_eq!(op["done"], true);
    let index_id = op["response"]["indexId"].as_str().unwrap().to_string();
    assert_eq!(op["response"]["state"], "READY");
    assert_eq!(op["metadata"]["indexId"], index_id);
    let name = op["name"].as_str().unwrap().to_string();
    assert!(name.starts_with("projects/p/operations/"), "{name}");

    let (status, _) = emulator.post("runQuery", query.clone());
    assert_eq!(status, 200, "the created index serves the query");
    let (status, _) = emulator.call("POST", "/v1/projects/p/indexes", Some(index));
    assert_eq!(status, 409);

    let (_, list) = emulator.call("GET", "/v1/projects/p/indexes", None);
    assert_eq!(list["indexes"][0]["indexId"], index_id);
    let (status, got) = emulator.call("GET", &format!("/v1/projects/p/indexes/{index_id}"), None);
    assert_eq!(status, 200);
    assert_eq!(got["properties"][1]["direction"], "DESCENDING");

    // Operations.
    let (status, got) = emulator.call("GET", &format!("/v1/{name}"), None);
    assert_eq!(status, 200, "{got}");
    assert_eq!(got["name"], name);
    let (_, list) = emulator.call("GET", "/v1/projects/p/operations", None);
    assert_eq!(list["operations"].as_array().unwrap().len(), 1);
    let (status, _) = emulator.call("POST", &format!("/v1/{name}:cancel"), Some(json!({})));
    assert_eq!(status, 200);

    let (status, _) = emulator.call(
        "DELETE",
        &format!("/v1/projects/p/indexes/{index_id}"),
        None,
    );
    assert_eq!(status, 200);
    let (status, _) = emulator.call("GET", &format!("/v1/projects/p/indexes/{index_id}"), None);
    assert_eq!(status, 404);
    let (status, _) = emulator.call("DELETE", &format!("/v1/{name}"), None);
    assert_eq!(status, 200);
    let (status, _) = emulator.call("GET", &format!("/v1/{name}"), None);
    assert_eq!(status, 404);

    // Java has neither.
    let java = start(&["--no_superset"]);
    assert_eq!(java.call("GET", "/v1/projects/p/indexes", None).0, 501);
    assert_eq!(java.call("GET", "/v1/projects/p/operations", None).0, 404);
}

#[test]
fn gcs_export_and_import() {
    let root = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        .args(["start", "--testing", "--port=0"])
        .env("OXIDE_LOG", "off")
        .env("DATASTORE_OXIDE_GCS_ROOT", root.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let port: u16 = lines
        .next()
        .unwrap()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    std::thread::spawn(move || lines.for_each(drop));
    let emulator = Emulator {
        child,
        base: format!("http://127.0.0.1:{port}"),
    };
    emulator.seed(&[int(1), int(2)]);

    let (status, op) = emulator.call(
        "POST",
        "/v1/projects/p:export",
        Some(json!({"outputUrlPrefix": "gs://bucket/backups", "labels": {"team": "x"}})),
    );
    assert_eq!(status, 200, "{op}");
    let output_url = op["response"]["outputUrl"].as_str().unwrap().to_string();
    assert!(
        output_url.starts_with("gs://bucket/backups/")
            && output_url.ends_with(".overall_export_metadata"),
        "{output_url}"
    );
    assert_eq!(op["metadata"]["common"]["labels"], json!({"team": "x"}));
    assert!(root.path().join("bucket/backups").is_dir());
    // The same prefix can be exported to again.
    let (status, _) = emulator.call(
        "POST",
        "/v1/projects/p:export",
        Some(json!({"outputUrlPrefix": "gs://bucket/backups"})),
    );
    assert_eq!(status, 200);

    let (status, op) = emulator.call(
        "POST",
        "/v1/projects/q:import",
        Some(json!({"inputUrl": output_url})),
    );
    assert_eq!(status, 200, "{op}");
    let (status, body) =
        emulator.post_to("q", "runQuery", json!({"query": {"kind": [{"name": "A"}]}}));
    assert_eq!(status, 200);
    assert_eq!(body["batch"]["entityResults"].as_array().unwrap().len(), 2);
}

impl Emulator {
    fn post_to(&self, project: &str, method: &str, body: Value) -> (u16, Value) {
        self.call(
            "POST",
            &format!("/v1/projects/{project}:{method}"),
            Some(body),
        )
    }
}

#[test]
fn data_viewer() {
    let emulator = start(&[]);
    seed_named(
        &emulator,
        &[
            ("a", json!({"n": int(1), "s": {"stringValue": "<b>hi</b>"}})),
            ("b", json!({"n": int(2)})),
        ],
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let get = |path: &str| {
        let mut r = agent
            .get(format!("{}{path}", emulator.base))
            .call()
            .unwrap();
        (r.status().as_u16(), r.body_mut().read_to_string().unwrap())
    };
    let (status, projects) = get("/_ah/admin/datastore");
    assert_eq!(status, 200);
    assert!(projects.contains("?project=p"), "{projects}");
    let (_, kinds) = get("/_ah/admin/datastore?project=p");
    assert!(kinds.contains(">A</a></td><td>2</td>"), "{kinds}");
    let (_, entities) = get("/_ah/admin/datastore?project=p&kind=A");
    assert!(
        entities.contains("&lt;b&gt;hi&lt;/b&gt;"),
        "values are escaped"
    );
    assert!(entities.contains("[A: a]"));

    // Delete `a` through its form, then the viewer shows only `b`.
    let action = entities
        .split("action=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .replace("&amp;", "&");
    let mut r = agent
        .post(format!("{}{action}", emulator.base))
        .send_empty()
        .unwrap();
    assert_eq!(
        r.status().as_u16(),
        200,
        "redirected back to the kind's page"
    );
    let page = r.body_mut().read_to_string().unwrap();
    assert!(
        !page.contains("[A: a]") && page.contains("[A: b]"),
        "{page}"
    );

    let java = start(&["--no_superset"]);
    let mut r = agent
        .get(format!("{}/_ah/admin/datastore", java.base))
        .call()
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    assert_eq!(r.body_mut().read_to_string().unwrap(), "Not Found\n");
}
