//! App Engine v3 API server steps: calls to `apphosting.tools.devappserver2.CallHandler/HandleCall`
//! over gRPC (h2c), on the same port as REST.
//!
//! ```text
//! V3 Put
//! entity { key { app: "s~p" path { Element { type: "K" name: "a" } } } entity_group {} }
//!
//! V3 Get service=memcache request_id=r1 hostport=127.0.0.1:1
//! key { app: "s~p" path { Element { type: "K" name: "a" } } }
//!
//! V3HEX Get
//! 0a0b
//! ```
//!
//! The step line names the method (`-` for the empty method name) and optional envelope fields.
//! A `V3` body is the inner request in protobuf text format (`datastore_v3` messages; `{{name}}`
//! captures are substituted first); a `V3HEX` body is the raw request bytes in hex. The recorded
//! status is the gRPC status (0 = OK). The recorded body is the decoded response in text format, or
//! the envelope's `application_error`/`rpc_error`, or `grpc-message` for a failed call. Captures
//! use the response's JSON form (e.g. `> h = handle`).

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use http_body_util::BodyExt;
use prost::Message;
use prost_reflect::text_format::FormatOptions;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor, Value};

const V3: &str = "apphosting_datastore_v3";
const ENVELOPE: &str = "apphosting.tools.devappserver2";

/// The response message of each supported method (`None`: an empty `VoidProto`).
fn response_type(method: &str) -> Option<&'static str> {
    Some(match method {
        "Get" => "GetResponse",
        "Put" => "PutResponse",
        "Delete" => "DeleteResponse",
        "RunQuery" | "Next" => "QueryResult",
        "BeginTransaction" => "Transaction",
        "Commit" => "CommitResponse",
        "AllocateIds" => "AllocateIdsResponse",
        "GetIndices" => "CompositeIndices",
        _ => return None,
    })
}

fn request_type(method: &str) -> Option<&'static str> {
    Some(match method {
        "Get" => "GetRequest",
        "Put" => "PutRequest",
        "Delete" => "DeleteRequest",
        "RunQuery" => "Query",
        "Next" => "NextRequest",
        "BeginTransaction" => "BeginTransactionRequest",
        "Commit" | "Rollback" => "Transaction",
        "AllocateIds" => "AllocateIdsRequest",
        "GetIndices" => "GetIndicesRequest",
        _ => return None,
    })
}

fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let mut fds = prost_types::FileDescriptorSet::decode(oxide_proto::FILE_DESCRIPTOR_SET)
            .expect("descriptor set");
        oxide_proto::as_proto1(&mut fds);
        DescriptorPool::from_file_descriptor_set(fds).expect("descriptor pool")
    })
}

fn message(name: &str) -> Result<MessageDescriptor> {
    pool()
        .get_message_by_name(name)
        .with_context(|| format!("unknown message {name}"))
}

/// Envelope fields from a step line: `<method> [service=..] [request_id=..] [hostport=..]`.
struct Call {
    method: String,
    service: String,
    request_id: String,
    hostport: String,
}

fn parse_call(path: &str) -> Result<Call> {
    let mut words = path.split_whitespace();
    let method = match words.next().context("V3 step without a method")? {
        "-" => String::new(),
        m => m.to_string(),
    };
    let mut call = Call {
        method,
        service: "datastore_v3".into(),
        request_id: String::new(),
        hostport: String::new(),
    };
    for word in words {
        let (k, v) = word.split_once('=').context("expected key=value")?;
        let v = if v == "-" {
            String::new()
        } else {
            v.to_string()
        };
        match k {
            "service" => call.service = v,
            "request_id" => call.request_id = v,
            "hostport" => call.hostport = v,
            _ => bail!("unknown V3 option {k}"),
        }
    }
    Ok(call)
}

/// Hex bytes; `(...)` encloses bytes to prefix with their varint length.
fn hex(s: &str) -> Result<Vec<u8>> {
    let s: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut pos = 0;
    let out = hex_until(&s, &mut pos)?;
    if pos != s.len() {
        bail!("unbalanced ')' in hex body");
    }
    Ok(out)
}

fn hex_until(s: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while *pos < s.len() {
        match s[*pos] {
            b'(' => {
                *pos += 1;
                let inner = hex_until(s, pos)?;
                if s.get(*pos) != Some(&b')') {
                    bail!("unbalanced '(' in hex body");
                }
                *pos += 1;
                let mut n = inner.len() as u64;
                while n >= 0x80 {
                    out.push((n as u8) | 0x80);
                    n >>= 7;
                }
                out.push(n as u8);
                out.extend(inner);
            }
            b')' => return Ok(out),
            _ => {
                let pair = s.get(*pos..*pos + 2).context("odd hex")?;
                out.push(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?);
                *pos += 2;
            }
        }
    }
    Ok(out)
}

/// Runs one `V3`/`V3HEX` step. Returns the gRPC status and the recorded body.
pub(crate) fn exchange(
    base: &str,
    kind: &str,
    path: &str,
    body: &str,
    captures: &[(String, String)],
    vars: &mut HashMap<String, String>,
) -> Result<(u16, String)> {
    let call = parse_call(path)?;
    let request = if kind == "V3HEX" {
        hex(body)?
    } else if body.trim().is_empty() {
        Vec::new()
    } else {
        let name = request_type(&call.method)
            .with_context(|| format!("no request type for {}; use V3HEX", call.method))?;
        DynamicMessage::parse_text_format(message(&format!("{V3}.{name}"))?, body)
            .with_context(|| format!("parsing {name} text"))?
            .encode_to_vec()
    };
    let mut envelope = DynamicMessage::new(message(&format!("{ENVELOPE}.Request"))?);
    for (field, value) in [
        ("service_name", Value::String(call.service)),
        ("method", Value::String(call.method.clone())),
        ("request", Value::Bytes(request.into())),
        ("request_id", Value::String(call.request_id)),
        (
            "txn_add_task_callback_hostport",
            Value::String(call.hostport),
        ),
    ] {
        envelope.set_field_by_name(field, value);
    }
    let (status, grpc_message, payload) = post(base, envelope.encode_to_vec())?;
    if status != 0 {
        return Ok((status, grpc_message));
    }
    let response = DynamicMessage::decode(message_desc("Response")?, payload.as_slice())?;
    let inner = response
        .get_field_by_name("response")
        .and_then(|v| v.as_bytes().cloned())
        .unwrap_or_default();
    let has = |f: &str| response.has_field_by_name(f);
    if has("application_error") || has("rpc_error") || has("exception") || has("java_exception") {
        return Ok((0, pretty(&response)));
    }
    let Some(name) = response_type(&call.method) else {
        return Ok((0, pretty(&response)));
    };
    let mut decoded = DynamicMessage::decode(message(&format!("{V3}.{name}"))?, inner.as_ref())?;
    if name == "CommitResponse" {
        sort_repeated(&mut decoded, "version");
    }
    if !captures.is_empty() {
        let json = serde_json::to_value(&decoded)?;
        for (var, path) in captures {
            if let Some(v) = crate::lookup_path(&json, path) {
                vars.insert(
                    var.clone(),
                    v.as_str().map_or_else(|| v.to_string(), str::to_string),
                );
            }
        }
    }
    Ok((0, pretty(&decoded)))
}

fn message_desc(name: &str) -> Result<MessageDescriptor> {
    message(&format!("{ENVELOPE}.{name}"))
}

fn pretty(m: &DynamicMessage) -> String {
    m.to_text_format_with_options(&FormatOptions::new().pretty(true))
}

/// Sorts a repeated message field by its text form (Java lists it in `HashMap` order).
fn sort_repeated(m: &mut DynamicMessage, field: &str) {
    let Some(Value::List(items)) = m.get_field_by_name(field).map(|v| v.into_owned()) else {
        return;
    };
    let mut items = items;
    items.sort_by_cached_key(|v| {
        v.as_message()
            .map(|m| m.to_text_format())
            .unwrap_or_default()
    });
    m.set_field_by_name(field, Value::List(items));
}

/// Sends one gRPC call; returns `(grpc-status, grpc-message, response message bytes)`.
fn post(base: &str, envelope: Vec<u8>) -> Result<(u16, String, Vec<u8>)> {
    let authority = base
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let stream = tokio::net::TcpStream::connect(&authority).await?;
        let (mut sender, conn) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            hyper_util::rt::TokioIo::new(stream),
        )
        .await?;
        tokio::spawn(conn);
        let mut framed = vec![0u8];
        framed.extend((envelope.len() as u32).to_be_bytes());
        framed.extend(envelope);
        let request = http::Request::post(format!(
            "http://{authority}/apphosting.tools.devappserver2.CallHandler/HandleCall"
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(http_body_util::Full::new(bytes::Bytes::from(framed)))?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            sender.send_request(request),
        )
        .await??;
        let headers = response.headers().clone();
        let collected = response.into_body().collect().await?;
        let trailers = collected.trailers().cloned().unwrap_or_default();
        let data = collected.to_bytes();
        let get = |name: &str| {
            trailers
                .get(name)
                .or_else(|| headers.get(name))
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let status: u16 = get("grpc-status").context("no grpc-status")?.parse()?;
        let message = percent_decode(&get("grpc-message").unwrap_or_default());
        let payload = if data.len() >= 5 {
            data[5..].to_vec()
        } else {
            Vec::new()
        };
        Ok((status, message, payload))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Replaces transaction handles (`handle: N`) and query handles (`cursor: N`) with their order of
/// first appearance: both counters survive `/reset` in Java.
pub(crate) fn normalize_handles(body: &str, handles: &mut Vec<String>) -> String {
    let mut out = String::new();
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let replaced = ["handle: ", "cursor: "].iter().find_map(|prefix| {
            let rest = trimmed.strip_prefix(prefix)?;
            let value = rest.trim_end();
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n = handles.iter().position(|h| h == value).unwrap_or_else(|| {
                handles.push(value.to_string());
                handles.len() - 1
            });
            let indent = &line[..line.len() - trimmed.len()];
            let newline = if line.ends_with('\n') { "\n" } else { "" };
            Some(format!("{indent}{prefix}<h{n}>{newline}"))
        });
        out.push_str(replaced.as_deref().unwrap_or(line));
    }
    out
}
