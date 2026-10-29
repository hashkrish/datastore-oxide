//! Starts the real binary on a free port and checks the banner, control endpoints, gRPC
//! reflection over h2c on the same port, and remote shutdown.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use tonic_reflection::pb::v1::ServerReflectionRequest;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;

struct Emulator {
    child: Child,
    port: u16,
}

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn start(args: &[&str]) -> Emulator {
    let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        .args(args)
        .env("OXIDE_LOG", "off")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unexpected banner line: {first}"));
    // The banner is 11 lines; the process keeps running, so never read to EOF.
    let rest: Vec<String> = lines.by_ref().take(10).map(Result::unwrap).collect();
    assert!(rest.contains(&format!(
        "  export DATASTORE_EMULATOR_HOST=localhost:{port}"
    )));
    assert!(rest.contains(&"Dev App Server is now running.".to_string()));
    Emulator { child, port }
}

/// Minimal HTTP/1.1 client: returns the raw response with CRLFs normalized.
fn http(port: u16, method: &str, path: &str) -> String {
    http_with(port, method, path, "")
}

/// Like [`http`], with extra header lines (each ending in `\r\n`).
fn http_with(port: u16, method: &str, path: &str, headers: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\ncontent-length: 0\r\n{headers}\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    // We ask for `connection: close` to read to EOF; hyper echoes it, which is not under test.
    out.replace("\r\n", "\n").replace("connection: close\n", "")
}

#[test]
fn serves_control_endpoints_and_grpc_on_one_port() {
    let mut emu = start(&["start", "--testing", "--port=0"]);
    let port = emu.port;

    assert_eq!(
        http(port, "GET", "/"),
        "HTTP/1.1 200 OK\ncontent-length: 3\n\nOk\n"
    );
    // With an Origin, every HTTP response carries Java's CORS headers.
    assert_eq!(
        http_with(
            port,
            "GET",
            "/nope",
            "origin: http://x\r\naccess-control-request-headers: a\r\n"
        ),
        "HTTP/1.1 404 Not Found\ncontent-length: 10\naccess-control-allow-origin: http://x\n\
         access-control-allow-methods: DELETE,GET,HEAD,PATCH,POST,PUT\n\
         access-control-allow-credentials: true\naccess-control-allow-headers: a\n\nNot Found\n"
    );
    assert_eq!(
        http(port, "GET", "/nope"),
        "HTTP/1.1 404 Not Found\ncontent-length: 10\n\nNot Found\n"
    );
    assert_eq!(
        http(port, "POST", "/reset"),
        "HTTP/1.1 200 OK\ncontent-length: 13\n\nResetting...\n"
    );

    let services = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = ServerReflectionClient::new(channel);
        let request = ServerReflectionRequest {
            host: String::new(),
            message_request: Some(MessageRequest::ListServices(String::new())),
        };
        let mut stream = client
            .server_reflection_info(tokio_stream::once(request))
            .await
            .unwrap()
            .into_inner();
        match stream.message().await.unwrap().unwrap().message_response {
            Some(MessageResponse::ListServicesResponse(list)) => {
                list.service.into_iter().map(|s| s.name).collect::<Vec<_>>()
            }
            other => panic!("unexpected reflection response: {other:?}"),
        }
    });
    assert!(
        services
            .iter()
            .any(|s| s == "grpc.reflection.v1.ServerReflection"),
        "{services:?}"
    );

    assert_eq!(
        http(port, "POST", "/shutdown"),
        "HTTP/1.1 200 OK\ncontent-length: 17\n\nShutting down...\n"
    );
    let status = wait_with_timeout(&mut emu.child, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0));
}

#[test]
fn on_disk_mode_refuses_reset() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    let emu = start(&["start", "--port=0", project.to_str().unwrap()]);
    assert_eq!(
        http(emu.port, "POST", "/reset"),
        "HTTP/1.1 500 Internal Server Error\ncontent-length: 12\n\nreset failed"
    );
    assert_eq!(
        http(emu.port, "POST", "/persist"),
        "HTTP/1.1 200 OK\ncontent-length: 14\n\nPersisting...\n"
    );
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "emulator did not exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
