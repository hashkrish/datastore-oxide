//! App Engine v3 API server: transactional tasks (`AddActions`) are delivered to the CallHandler
//! at `txn_add_task_callback_hostport` when the transaction commits, as taskqueue `Add` calls.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use oxide_proto::apphosting::tools::devappserver2 as das;
use oxide_proto::apphosting_datastore_v3 as v3;
use prost::Message;
use tonic::{Request, Response, Status};

struct Emulator(Child, u16);

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start() -> Emulator {
    start_with(&["--no_superset"])
}

fn start_with(flags: &[&str]) -> Emulator {
    start_with_env(flags, &[])
}

fn start_with_env(flags: &[&str], env: &[(&str, &str)]) -> Emulator {
    let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        .args(["start", "--testing", "--port=0"])
        .args(flags)
        .env("OXIDE_LOG", "off")
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first.rsplit(':').next().unwrap().parse().unwrap();
    std::thread::spawn(move || lines.for_each(drop));
    Emulator(child, port)
}

/// A CallHandler that records what it is sent.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<das::Request>>>);

#[tonic::async_trait]
impl das::call_handler_server::CallHandler for Recorder {
    async fn handle_call(
        &self,
        request: Request<das::Request>,
    ) -> Result<Response<das::Response>, Status> {
        self.0.lock().unwrap().push(request.into_inner());
        Ok(Response::new(das::Response::default()))
    }
}

fn field(number: u32, value: &[u8]) -> Vec<u8> {
    let mut out = vec![(number << 3 | 2) as u8, value.len() as u8];
    out.extend_from_slice(value);
    out
}

#[tokio::test]
async fn transactional_tasks_reach_the_callback() {
    let emulator = start();
    let recorder = Recorder::default();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let callback = listener.local_addr().unwrap();
    drop(listener);
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(das::call_handler_server::CallHandlerServer::new(
                recorder.clone(),
            ))
            .serve(callback),
    );

    let mut client = das::call_handler_client::CallHandlerClient::connect(format!(
        "http://127.0.0.1:{}",
        emulator.1
    ))
    .await
    .unwrap();
    let mut call = async |method: &str, request: Vec<u8>, hostport: &str| {
        client
            .handle_call(das::Request {
                service_name: "datastore_v3".into(),
                method: method.into(),
                request: request.into(),
                txn_add_task_callback_hostport: hostport.into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
    };

    let begun = call(
        "BeginTransaction",
        v3::BeginTransactionRequest {
            app: "s~cb".into(),
            ..Default::default()
        }
        .encode_to_vec(),
        "",
    )
    .await;
    let txn = v3::Transaction::decode(begun.response).unwrap();
    // TaskQueueAddRequest: queue_name, task_name, eta_usec, body, transaction, app_id.
    let mut task = field(1, b"default");
    task.extend(field(2, b"t1"));
    task.extend([3 << 3, 123]);
    task.extend(field(9, b"body"));
    task.extend(field(10, &txn.encode_to_vec()));
    task.extend(field(11, b"s~cb"));
    let added = call("AddActions", field(1, &task), "").await;
    assert_eq!(added, das::Response::default());

    let committed = call(
        "Commit",
        txn.encode_to_vec(),
        &format!("127.0.0.1:{}", callback.port()),
    )
    .await;
    assert!(committed.application_error.is_none(), "{committed:?}");

    let got = recorder.0.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].service_name.as_str(), got[0].method.as_str()),
        ("taskqueue", "Add")
    );
    // The task without its transaction, with the request's app id.
    let mut expected = field(1, b"default");
    expected.extend(field(2, b"t1"));
    expected.extend([3 << 3, 123]);
    expected.extend(field(9, b"body"));
    expected.extend(field(11, b"s~cb"));
    assert_eq!(got[0].request.as_ref(), expected.as_slice());
}

mod superset {
    use super::*;
    use oxide_proto::storage_onestore_v3 as os;

    const APP: &str = "s~app";

    fn reference(name: &str) -> os::Reference {
        os::Reference {
            app: APP.into(),
            path: os::Path {
                element: vec![os::path::Element {
                    r#type: "K".into(),
                    name: Some(name.to_string().into()),
                    ..Default::default()
                }],
            },
            ..Default::default()
        }
    }

    fn entity(name: &str, n: i64, words: &str) -> os::EntityProto {
        let property = |name: &str, value: os::PropertyValue| os::Property {
            name: name.to_string().into(),
            multiple: false,
            value,
            ..Default::default()
        };
        os::EntityProto {
            key: reference(name),
            entity_group: reference(name).path,
            property: vec![
                property(
                    "n",
                    os::PropertyValue {
                        int64_value: Some(n),
                        ..Default::default()
                    },
                ),
                property(
                    "s",
                    os::PropertyValue {
                        string_value: Some(words.to_string().into()),
                        ..Default::default()
                    },
                ),
            ],
            ..Default::default()
        }
    }

    fn n_filter(op: i32, values: &[i64]) -> v3::query::Filter {
        v3::query::Filter {
            op,
            property: values
                .iter()
                .map(|v| os::Property {
                    name: "n".into(),
                    multiple: false,
                    value: os::PropertyValue {
                        int64_value: Some(*v),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    struct Client(das::call_handler_client::CallHandlerClient<tonic::transport::Channel>);

    impl Client {
        async fn call(&mut self, method: &str, request: Vec<u8>) -> das::Response {
            self.0
                .handle_call(das::Request {
                    service_name: "datastore_v3".into(),
                    method: method.into(),
                    request: request.into(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_inner()
        }

        async fn ok(&mut self, method: &str, request: Vec<u8>) -> bytes::Bytes {
            let r = self.call(method, request).await;
            assert!(r.application_error.is_none(), "{method}: {r:?}");
            r.response
        }

        /// Names of the entities a query returns.
        async fn names(&mut self, q: v3::Query) -> Vec<String> {
            let result =
                v3::QueryResult::decode(self.ok("RunQuery", q.encode_to_vec()).await).unwrap();
            result
                .result
                .iter()
                .map(|e| {
                    String::from_utf8(e.key.path.element[0].name.clone().unwrap().to_vec()).unwrap()
                })
                .collect()
        }
    }

    async fn connect(emulator: &Emulator) -> Client {
        Client(
            das::call_handler_client::CallHandlerClient::connect(format!(
                "http://127.0.0.1:{}",
                emulator.1
            ))
            .await
            .unwrap(),
        )
    }

    fn query(filters: Vec<v3::query::Filter>, search: Option<&str>) -> v3::Query {
        v3::Query {
            app: APP.into(),
            kind: Some("K".into()),
            filter: filters,
            search_query: search.map(|s| s.to_string().into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn filters_search_indexes_and_errors() {
        let emulator = start_with(&[]);
        let mut client = connect(&emulator).await;
        client
            .ok(
                "Put",
                v3::PutRequest {
                    entity: vec![
                        entity("a", 1, "The quick brown fox"),
                        entity("b", 2, "a lazy dog"),
                        entity("c", 3, "Quick thinking"),
                    ],
                    ..Default::default()
                }
                .encode_to_vec(),
            )
            .await;

        // IN with several values, NOT_EQUAL (9), NOT_IN (10).
        assert_eq!(
            client.names(query(vec![n_filter(6, &[1, 3])], None)).await,
            ["a", "c"]
        );
        assert_eq!(
            client.names(query(vec![n_filter(9, &[2])], None)).await,
            ["a", "c"]
        );
        assert_eq!(
            client.names(query(vec![n_filter(10, &[1, 2])], None)).await,
            ["c"]
        );

        // Full-text search: every word, case-insensitively.
        assert_eq!(client.names(query(vec![], Some("QUICK"))).await, ["a", "c"]);
        assert_eq!(client.names(query(vec![], Some("quick fox"))).await, ["a"]);

        // Touch and DeleteCursor answer; index calls manage composite indexes.
        client
            .ok(
                "Touch",
                v3::TouchRequest {
                    key: vec![reference("a")],
                    ..Default::default()
                }
                .encode_to_vec(),
            )
            .await;
        let index = os::CompositeIndex {
            app_id: APP.into(),
            id: 0,
            definition: os::Index {
                entity_type: "K".into(),
                ancestor: false,
                property: ["n", "s"]
                    .iter()
                    .map(|n| os::index::Property {
                        name: n.to_string().into(),
                        direction: Some(1),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            state: 2,
            ..Default::default()
        };
        let id = client.ok("CreateIndex", index.encode_to_vec()).await;
        assert_eq!(id.as_ref(), [0x08, 1]);
        let listed = v3::CompositeIndices::decode(
            client
                .ok(
                    "GetIndices",
                    v3::GetIndicesRequest {
                        app_id: APP.into(),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
                .await,
        )
        .unwrap();
        assert_eq!(listed.index.len(), 1);
        client.ok("DeleteIndex", index.encode_to_vec()).await;

        // A proper error where Java throws.
        let r = client
            .call(
                "RunQuery",
                v3::Query {
                    limit: Some(-1),
                    ..query(vec![], None)
                }
                .encode_to_vec(),
            )
            .await;
        assert_eq!(
            r.application_error.unwrap().detail,
            "The query limit must not be negative."
        );
    }

    #[tokio::test]
    async fn deferred_gets_and_read_time() {
        let emulator = start_with(&[]);
        let mut client = connect(&emulator).await;
        let big = "x".repeat(400_000);
        let names: Vec<String> = (0..5).map(|i| format!("e{i}")).collect();
        let mut put = v3::PutRequest::default();
        for name in &names {
            let mut e = entity(name, 1, "");
            e.raw_property.push(os::Property {
                name: "big".into(),
                multiple: false,
                meaning: Some(15),
                value: os::PropertyValue {
                    string_value: Some(big.clone().into()),
                    ..Default::default()
                },
                ..Default::default()
            });
            put.entity.push(e);
        }
        client.ok("Put", put.encode_to_vec()).await;
        let get = |allow_deferred, read_time| v3::GetRequest {
            key: names.iter().map(|n| reference(n)).collect(),
            allow_deferred,
            read_time_epoch_micros: read_time,
            ..Default::default()
        };
        let got = v3::GetResponse::decode(
            client
                .ok("Get", get(Some(true), None).encode_to_vec())
                .await,
        )
        .unwrap();
        assert_eq!(got.entity.len(), 3);
        assert_eq!(got.deferred.len(), 2);
        assert_eq!(got.in_order, Some(false));

        // Before the put, nothing existed.
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as i64
            - 30_000_000;
        let got = v3::GetResponse::decode(
            client
                .ok("Get", get(None, Some(micros)).encode_to_vec())
                .await,
        )
        .unwrap();
        assert!(got.entity.iter().all(|e| e.entity.is_none()));
        assert_eq!(got.read_time_epoch_micros, Some(micros));
    }

    /// Live queries expire like Java's (`datastore.max_query_lifetime`): a sweep every five
    /// lifetimes drops the snapshots of queries that were never read to the end.
    #[tokio::test]
    async fn unfinished_queries_expire() {
        let emulator = start_with_env(&["--no_superset"], &[("OXIDE_QUERY_LIFETIME_MS", "100")]);
        let mut client = connect(&emulator).await;
        client
            .ok(
                "Put",
                v3::PutRequest {
                    entity: vec![entity("a", 1, ""), entity("b", 2, ""), entity("c", 3, "")],
                    ..Default::default()
                }
                .encode_to_vec(),
            )
            .await;
        async fn run(client: &mut Client) -> v3::Cursor {
            let q = v3::Query {
                count: Some(1),
                ..query(vec![], None)
            };
            let result = client.ok("RunQuery", q.encode_to_vec()).await;
            v3::QueryResult::decode(result).unwrap().cursor.unwrap()
        }
        let next = |cursor: &v3::Cursor| {
            v3::NextRequest {
                cursor: cursor.clone(),
                count: Some(1),
                ..Default::default()
            }
            .encode_to_vec()
        };

        let fresh = run(&mut client).await;
        client.ok("Next", next(&fresh)).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let r = client.call("Next", next(&fresh)).await;
        let error = r.application_error.expect("expired");
        assert!(error.detail.starts_with("query has expired"), "{error:?}");
        // Queries run since the sweep still work.
        let fresh = run(&mut client).await;
        client.ok("Next", next(&fresh)).await;
    }
}
