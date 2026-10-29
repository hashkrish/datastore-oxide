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
