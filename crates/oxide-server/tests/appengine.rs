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
    let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        .args(["start", "--testing", "--port=0"])
        .env("OXIDE_LOG", "off")
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
