//! REST (HTTP/JSON) gateway, transcoded onto the gRPC services in-process.
//!
//! Routes come from the `google.api.http` rules in the descriptors. A matched request is parsed
//! (JSON against the schema the Java emulator accepts, see [`java_schema`]; or binary for
//! `application/x-protobuf`), framed as a gRPC call to the tonic router,
//! and the response is printed the way the Java emulator prints it (see `oxide-json`).

use std::collections::HashMap;

use bytes::{BufMut, Bytes, BytesMut};
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use prost::Message as _;
use prost_reflect::{
    DescriptorPool, DynamicMessage, MessageDescriptor, ReflectMessage, SerializeOptions,
};
use tonic::body::Body;
use tonic::service::Routes;
use tower::ServiceExt;

const PAYLOAD_INVALID: &str = "Payload isn't valid for request.";

/// API versions the Java emulator recognizes; other `/vN...` prefixes get a specific 404.
const KNOWN_API_VERSIONS: &[&str] = &["v1", "v1beta1", "v1beta3"];

/// Standard query parameters that never map to request fields.
const SYSTEM_PARAMS: &[&str] = &[
    "alt",
    "$alt",
    "prettyPrint",
    "key",
    "fields",
    "$fields",
    "quotaUser",
    "callback",
    "access_token",
    "upload_protocol",
    "uploadType",
    "$.xgafv",
];

/// Request fields of the published Datastore v1 API that the Java emulator rejects as unknown
/// (`Payload isn't valid for request.`), probed one by one (docs/reference-behavior.md).
const JAVA_UNKNOWN_FIELDS: &[(&str, &str)] = &[
    ("LookupRequest", "request_options"),
    ("RunQueryRequest", "request_options"),
    ("RunQueryRequest", "explain_options"),
    ("RunAggregationQueryRequest", "request_options"),
    ("RunAggregationQueryRequest", "explain_options"),
    ("BeginTransactionRequest", "request_options"),
    ("CommitRequest", "request_options"),
    ("RollbackRequest", "request_options"),
    ("AllocateIdsRequest", "request_options"),
    ("ReserveIdsRequest", "request_options"),
    ("Mutation", "property_transforms"),
    ("Query", "find_nearest"),
];

/// Turns a published `google/datastore/v1` file into the schema the Java emulator uses for REST
/// JSON: without [`JAVA_UNKNOWN_FIELDS`] (kept with the superset), with the `HAS_PARENT` (12)
/// filter operator, which Java accepts by name and by number, and with
/// `CommitResponse.commit_version`, which Java prints.
fn java_schema(file: &mut prost_types::FileDescriptorProto, superset: bool) {
    use prost_types::field_descriptor_proto::{Label, Type};
    for m in &mut file.message_type {
        m.field.retain(|f| {
            superset
                || !JAVA_UNKNOWN_FIELDS.contains(&(m.name.as_deref().unwrap_or_default(), f.name()))
        });
        if m.name() == "CommitResponse" {
            m.field.push(prost_types::FieldDescriptorProto {
                name: Some("commit_version".into()),
                number: Some(5),
                label: Some(Label::Optional as i32),
                r#type: Some(Type::Int64 as i32),
                json_name: Some("commitVersion".into()),
                ..Default::default()
            });
        }
        if m.name() == "PropertyFilter" {
            for e in &mut m.enum_type {
                if e.name() == "Operator" {
                    e.value.push(prost_types::EnumValueDescriptorProto {
                        name: Some("HAS_PARENT".into()),
                        number: Some(12),
                        options: None,
                    });
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Segment {
    Literal(String),
    /// `*`: exactly one segment.
    Star,
    /// `**`: the rest of the path.
    DoubleStar,
    /// `{field}` or `{field=pattern}`.
    Variable {
        field: String,
        pattern: Vec<Segment>,
    },
}

#[derive(Debug, Clone)]
struct Route {
    method: Method,
    segments: Vec<Segment>,
    verb: Option<String>,
    /// `*` (whole message), a field name, or empty (no body).
    body: String,
    /// `/package.Service/Method`
    grpc_path: String,
    input: String,
    output: String,
}

pub struct RestGateway {
    routes: Vec<Route>,
    /// Java's schema, used to parse request JSON and print responses.
    parse_pool: DescriptorPool,
}

impl RestGateway {
    /// With `superset`, requests may also carry the production fields Java's schema lacks.
    pub fn new(superset: bool) -> Result<Self, prost_reflect::DescriptorError> {
        let print_pool = DescriptorPool::decode(oxide_proto::FILE_DESCRIPTOR_SET)?;

        let mut parse_pool = DescriptorPool::new();
        parse_pool.add_file_descriptor_protos(print_pool.file_descriptor_protos().cloned().map(
            |mut f| {
                if f.name() == "google/datastore/v1/datastore.proto"
                    || f.name() == "google/datastore/v1/query.proto"
                {
                    java_schema(&mut f, superset);
                }
                f
            },
        ))?;

        let http_rule = print_pool
            .get_extension_by_name("google.api.http")
            .expect("google.api.http in pool");
        let mut routes = Vec::new();
        for service in print_pool.services() {
            for method in service.methods() {
                let options = method.options();
                if !options.has_extension(&http_rule) {
                    continue;
                }
                let rule = options.get_extension(&http_rule);
                let Some(rule) = rule.as_message() else {
                    continue;
                };
                let grpc_path = format!("/{}/{}", service.full_name(), method.name());
                let input = method.input().full_name().to_string();
                let output = method.output().full_name().to_string();
                for rule in std::iter::once(rule.clone()).chain(
                    rule.get_field_by_name("additional_bindings")
                        .and_then(|v| {
                            v.as_list().map(|l| {
                                l.iter()
                                    .filter_map(|m| m.as_message().cloned())
                                    .collect::<Vec<_>>()
                            })
                        })
                        .unwrap_or_default(),
                ) {
                    if let Some(route) = route_from_rule(&rule, &grpc_path, &input, &output) {
                        routes.push(route);
                    }
                }
            }
        }
        if superset {
            routes.extend(operation_routes());
        }
        Ok(Self { routes, parse_pool })
    }

    pub async fn handle<B>(&self, req: Request<B>, grpc: Routes) -> Response<Body>
    where
        B: http_body::Body + Send,
        B::Data: Send,
    {
        let path = req.uri().path().to_string();
        let Some((route, vars)) = self.match_route(req.method(), &path) else {
            return not_found(&path);
        };
        let content_type = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let binary = match mime.as_str() {
            "application/json" => false,
            "application/x-protobuf" => true,
            "" if route.body.is_empty() => false,
            _ => return plain(StatusCode::NOT_FOUND, "Not Found\n"),
        };
        let query = req.uri().query().unwrap_or("").to_string();
        let Ok(body) = req.into_body().collect().await.map(|c| c.to_bytes()) else {
            return error_response(&tonic::Status::invalid_argument(PAYLOAD_INVALID));
        };
        let input = match self.build_input(route, &vars, &query, &body, binary) {
            Ok(bytes) => bytes,
            Err(status) => return error_response(&status),
        };
        match call_grpc(grpc, &route.grpc_path, input).await {
            Ok(output) => self.success_response(route, output, binary),
            Err(mut status) => {
                if status.code() == tonic::Code::Unimplemented && status.message().is_empty() {
                    status = tonic::Status::unimplemented(format!(
                        "Method {} is unimplemented",
                        route.grpc_path.trim_start_matches('/')
                    ));
                }
                error_response(&status)
            }
        }
    }

    fn match_route(
        &self,
        method: &Method,
        path: &str,
    ) -> Option<(&Route, HashMap<String, String>)> {
        let path = path.strip_prefix('/')?;
        // The verb follows the first ':' in the last segment.
        let (last_start, last) = path
            .rsplit_once('/')
            .map_or((0, path), |(head, last)| (head.len() + 1, last));
        let (path, verb) = match last.find(':') {
            Some(i) => (&path[..last_start + i], Some(&last[i + 1..])),
            None => (path, None),
        };
        let parts: Vec<&str> = path.split('/').collect();
        self.routes
            .iter()
            .filter(|r| r.method == *method && r.verb.as_deref() == verb)
            .find_map(|r| {
                let mut vars = HashMap::new();
                match_segments(&r.segments, &parts, &mut vars).then_some((r, vars))
            })
    }

    /// Builds the serialized request message from path variables, query parameters and body.
    fn build_input(
        &self,
        route: &Route,
        vars: &HashMap<String, String>,
        query: &str,
        body: &[u8],
        binary: bool,
    ) -> Result<Bytes, tonic::Status> {
        let invalid = || tonic::Status::invalid_argument(PAYLOAD_INVALID);
        let desc = self
            .parse_pool
            .get_message_by_name(&route.input)
            .expect("route input type in pool");
        let mut message = if route.body == "*" {
            parse_message(desc.clone(), body, binary).ok_or_else(invalid)?
        } else {
            let mut message = DynamicMessage::new(desc.clone());
            if !route.body.is_empty() {
                let field = desc
                    .get_field_by_name(&route.body)
                    .expect("body field exists");
                let sub = field.kind().as_message().cloned().ok_or_else(invalid)?;
                let value = parse_message(sub, body, binary).ok_or_else(invalid)?;
                message.set_field(&field, prost_reflect::Value::Message(value));
            }
            message
        };
        for (name, value) in form_urlencoded(query) {
            if SYSTEM_PARAMS.contains(&name.as_str()) {
                continue;
            }
            if route.body == "*" {
                return Err(invalid());
            }
            set_scalar(&mut message, &name, &value).ok_or_else(invalid)?;
        }
        for (field, value) in vars {
            set_scalar(&mut message, field, value).ok_or_else(invalid)?;
        }
        Ok(message.encode_to_vec().into())
    }

    fn success_response(&self, route: &Route, output: Bytes, binary: bool) -> Response<Body> {
        if binary {
            return bytes_response(StatusCode::OK, "application/x-protobuf", output);
        }
        // Printed with Java's schema, so responses carry exactly the fields Java's do.
        let desc = self
            .parse_pool
            .get_message_by_name(&route.output)
            .expect("route output type in pool");
        let Ok(message) = DynamicMessage::decode(desc, output) else {
            return error_response(&tonic::Status::internal("malformed response"));
        };
        let options = SerializeOptions::new()
            .skip_default_fields(true)
            .stringify_64_bit_integers(true);
        let mut json = match message.serialize_with_options(serde_json::value::Serializer, &options)
        {
            Ok(json) => json,
            Err(e) => return error_response(&tonic::Status::internal(e.to_string())),
        };
        sort_property_maps(&mut json);
        drop_empty_any_values(&mut json);
        let text = format!("{}\n", oxide_json::to_java_pretty(&json));
        bytes_response(StatusCode::OK, "application/json", Bytes::from(text))
    }
}

/// Java's `JsonFormat` prints an `Any` holding `google.protobuf.Empty` as just its `@type`.
fn drop_empty_any_values(json: &mut serde_json::Value) {
    match json {
        serde_json::Value::Object(map) => {
            if map.get("@type").and_then(|t| t.as_str())
                == Some("type.googleapis.com/google.protobuf.Empty")
            {
                map.remove("value");
            }
            map.values_mut().for_each(drop_empty_any_values);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(drop_empty_any_values),
        _ => {}
    }
}

fn route_from_rule(
    rule: &DynamicMessage,
    grpc_path: &str,
    input: &str,
    output: &str,
) -> Option<Route> {
    let (method, template) = ["get", "put", "post", "delete", "patch"]
        .iter()
        .find_map(|m| {
            let v = rule.get_field_by_name(m)?;
            let s = v.as_str()?.to_string();
            (!s.is_empty()).then(|| {
                (
                    Method::from_bytes(m.to_ascii_uppercase().as_bytes()).ok(),
                    s,
                )
            })
        })?;
    let body = rule
        .get_field_by_name("body")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let (segments, verb) = parse_template(&template)?;
    Some(Route {
        method: method?,
        segments,
        verb,
        body,
        grpc_path: grpc_path.into(),
        input: input.into(),
        output: output.into(),
    })
}

/// Production's REST bindings for the Admin API's operations (superset; Java has no Operations
/// service). The `google.longrunning` protos only bind `/v1/{name=operations/**}`.
fn operation_routes() -> Vec<Route> {
    const SERVICE: &str = "/google.longrunning.Operations";
    [
        (
            "GET",
            "/v1/{name=projects/*}/operations",
            "ListOperations",
            "ListOperationsRequest",
            "ListOperationsResponse",
        ),
        (
            "GET",
            "/v1/{name=projects/*/operations/*}",
            "GetOperation",
            "GetOperationRequest",
            "Operation",
        ),
        (
            "DELETE",
            "/v1/{name=projects/*/operations/*}",
            "DeleteOperation",
            "DeleteOperationRequest",
            "google.protobuf.Empty",
        ),
        (
            "POST",
            "/v1/{name=projects/*/operations/*}:cancel",
            "CancelOperation",
            "CancelOperationRequest",
            "google.protobuf.Empty",
        ),
    ]
    .into_iter()
    .map(|(method, template, rpc, input, output)| {
        let (segments, verb) = parse_template(template).expect("valid template");
        let qualify = |t: &str| {
            if t.contains('.') {
                t.to_string()
            } else {
                format!("google.longrunning.{t}")
            }
        };
        Route {
            method: Method::from_bytes(method.as_bytes()).expect("valid method"),
            segments,
            verb,
            body: if method == "POST" {
                "*".into()
            } else {
                String::new()
            },
            grpc_path: format!("{SERVICE}/{rpc}"),
            input: qualify(input),
            output: qualify(output),
        }
    })
    .collect()
}

/// Parses `/v1/projects/{project_id}:lookup` or `/v1/{name=projects/*/operations/*}`.
fn parse_template(template: &str) -> Option<(Vec<Segment>, Option<String>)> {
    let template = template.strip_prefix('/')?;
    // The verb is after the last ':' that is outside braces.
    let mut depth = 0;
    let mut verb_at = None;
    for (i, c) in template.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ':' if depth == 0 => verb_at = Some(i),
            _ => {}
        }
    }
    let (path, verb) = match verb_at {
        Some(i) => (&template[..i], Some(template[i + 1..].to_string())),
        None => (template, None),
    };
    let mut segments = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(inner) = rest.strip_prefix('{') {
            let end = inner.find('}')?;
            let var = &inner[..end];
            let (field, pattern) = match var.split_once('=') {
                Some((f, p)) => (f.to_string(), p.split('/').map(simple_segment).collect()),
                None => (var.to_string(), vec![Segment::Star]),
            };
            segments.push(Segment::Variable { field, pattern });
            rest = inner[end + 1..]
                .strip_prefix('/')
                .unwrap_or(&inner[end + 1..]);
        } else {
            let (seg, after) = rest.split_once('/').unwrap_or((rest, ""));
            segments.push(simple_segment(seg));
            rest = after;
        }
    }
    Some((segments, verb))
}

fn simple_segment(s: &str) -> Segment {
    match s {
        "*" => Segment::Star,
        "**" => Segment::DoubleStar,
        lit => Segment::Literal(lit.to_string()),
    }
}

fn match_segments(
    segments: &[Segment],
    parts: &[&str],
    vars: &mut HashMap<String, String>,
) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return parts.is_empty();
    };
    match first {
        Segment::Literal(lit) => {
            parts.first() == Some(&lit.as_str()) && match_segments(rest, &parts[1..], vars)
        }
        Segment::Star => !parts.is_empty() && match_segments(rest, &parts[1..], vars),
        Segment::DoubleStar => true,
        Segment::Variable { field, pattern } => {
            let width = if pattern.contains(&Segment::DoubleStar) {
                parts.len() - rest.len().min(parts.len())
            } else {
                pattern.len()
            };
            if parts.len() < width {
                return false;
            }
            let mut inner = HashMap::new();
            if !match_segments(pattern, &parts[..width], &mut inner) {
                return false;
            }
            vars.insert(field.clone(), percent_decode(&parts[..width].join("/")));
            match_segments(rest, &parts[width..], vars)
        }
    }
}

fn parse_message(desc: MessageDescriptor, body: &[u8], binary: bool) -> Option<DynamicMessage> {
    if binary {
        return DynamicMessage::decode(desc, body).ok();
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Some(DynamicMessage::new(desc));
    }
    let mut de = serde_json::Deserializer::from_slice(body);
    let message = DynamicMessage::deserialize(desc, &mut de).ok()?;
    de.end().ok()?;
    Some(message)
}

/// Sets a (possibly dotted) scalar field from its string form, by proto or JSON name.
fn set_scalar(message: &mut DynamicMessage, path: &str, value: &str) -> Option<()> {
    use prost_reflect::{Kind, Value};
    let (head, tail) = match path.split_once('.') {
        Some((h, t)) => (h, Some(t)),
        None => (path, None),
    };
    let desc = message.descriptor();
    let field = desc
        .get_field_by_name(head)
        .or_else(|| desc.get_field_by_json_name(head))?;
    if let Some(tail) = tail {
        let mut sub = message.get_field(&field).as_message()?.clone();
        set_scalar(&mut sub, tail, value)?;
        message.set_field(&field, Value::Message(sub));
        return Some(());
    }
    let parsed = match field.kind() {
        Kind::String => Value::String(value.to_string()),
        Kind::Bool => Value::Bool(value.parse().ok()?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(value.parse().ok()?),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => Value::I64(value.parse().ok()?),
        Kind::Uint32 | Kind::Fixed32 => Value::U32(value.parse().ok()?),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(value.parse().ok()?),
        Kind::Double => Value::F64(value.parse().ok()?),
        Kind::Float => Value::F32(value.parse().ok()?),
        Kind::Enum(e) => Value::EnumNumber(e.get_value_by_name(value)?.number()),
        _ => return None,
    };
    message.set_field(&field, parsed);
    Some(())
}

async fn call_grpc(grpc: Routes, path: &str, input: Bytes) -> Result<Bytes, tonic::Status> {
    let mut framed = BytesMut::with_capacity(input.len() + 5);
    framed.put_u8(0);
    framed.put_u32(input.len() as u32);
    framed.put(input);
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers")
        .body(Body::new(Full::new(framed.freeze())))
        .expect("valid request");
    let response = grpc
        .oneshot(request)
        .await
        .map_err(|e| tonic::Status::internal(e.to_string()))?;
    let (parts, body) = response.into_parts();
    // Trailers-only responses carry the status in the headers.
    if let Some(status) =
        tonic::Status::from_header_map(&parts.headers).filter(|s| s.code() != tonic::Code::Ok)
    {
        return Err(status);
    }
    let collected = body
        .collect()
        .await
        .map_err(|e| tonic::Status::internal(e.to_string()))?;
    if let Some(status) = collected
        .trailers()
        .and_then(tonic::Status::from_header_map)
        && status.code() != tonic::Code::Ok
    {
        return Err(status);
    }
    let data = collected.to_bytes();
    if data.len() < 5 {
        return Err(tonic::Status::internal("empty gRPC response"));
    }
    Ok(data.slice(5..))
}

fn http_status(code: tonic::Code) -> (u16, &'static str) {
    use tonic::Code::*;
    match code {
        Ok => (200, "OK"),
        Cancelled => (499, "CANCELLED"),
        Unknown => (500, "UNKNOWN"),
        InvalidArgument => (400, "INVALID_ARGUMENT"),
        DeadlineExceeded => (504, "DEADLINE_EXCEEDED"),
        NotFound => (404, "NOT_FOUND"),
        AlreadyExists => (409, "ALREADY_EXISTS"),
        PermissionDenied => (403, "PERMISSION_DENIED"),
        ResourceExhausted => (429, "RESOURCE_EXHAUSTED"),
        FailedPrecondition => (400, "FAILED_PRECONDITION"),
        Aborted => (409, "ABORTED"),
        OutOfRange => (400, "OUT_OF_RANGE"),
        Unimplemented => (501, "UNIMPLEMENTED"),
        Internal => (500, "INTERNAL"),
        Unavailable => (503, "UNAVAILABLE"),
        DataLoss => (500, "DATA_LOSS"),
        Unauthenticated => (401, "UNAUTHENTICATED"),
    }
}

fn error_response(status: &tonic::Status) -> Response<Body> {
    let (code, name) = http_status(status.code());
    let body = oxide_json::error_body(code, status.message(), name);
    bytes_response(
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        "application/json",
        body.into(),
    )
}

fn not_found(path: &str) -> Response<Body> {
    let version = path.trim_start_matches('/').split('/').next().unwrap_or("");
    let looks_versioned =
        version.len() > 1 && version.starts_with('v') && version.as_bytes()[1].is_ascii_digit();
    if looks_versioned && path[1..].contains('/') && !KNOWN_API_VERSIONS.contains(&version) {
        let body = format!("Not Found (Unsupported API version \"{version}\")\n");
        return bytes_response_untyped(StatusCode::NOT_FOUND, Bytes::from(body));
    }
    plain(StatusCode::NOT_FOUND, "Not Found\n")
}

fn plain(status: StatusCode, body: &'static str) -> Response<Body> {
    bytes_response_untyped(status, Bytes::from_static(body.as_bytes()))
}

fn bytes_response_untyped(status: StatusCode, body: Bytes) -> Response<Body> {
    let len = body.len();
    let mut resp = Response::new(Body::new(Full::new(body)));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    resp
}

fn bytes_response(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Body> {
    let mut resp = bytes_response_untyped(status, body);
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

/// Map fields named `properties` (entity properties) or `aggregateProperties` are unordered;
/// sort them for stable output.
fn sort_property_maps(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if (key == "properties" || key == "aggregateProperties")
                    && let serde_json::Value::Object(props) = v
                {
                    props.sort_keys();
                }
                sort_property_maps(v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sort_property_maps),
        _ => {}
    }
}

fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form_urlencoded(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (
                percent_decode(&k.replace('+', " ")),
                percent_decode(&v.replace('+', " ")),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_matching() {
        let (segs, verb) = parse_template("/v1/projects/{project_id}:lookup").unwrap();
        assert_eq!(verb.as_deref(), Some("lookup"));
        let mut vars = HashMap::new();
        assert!(match_segments(
            &segs,
            &["v1", "projects", "p%20q"],
            &mut vars
        ));
        assert_eq!(vars["project_id"], "p q");

        let (segs, verb) = parse_template("/v1/{name=projects/*/operations/*}").unwrap();
        assert_eq!(verb, None);
        let mut vars = HashMap::new();
        assert!(match_segments(
            &segs,
            &["v1", "projects", "p", "operations", "op1"],
            &mut vars
        ));
        assert_eq!(vars["name"], "projects/p/operations/op1");
        assert!(!match_segments(
            &segs,
            &["v1", "projects", "p"],
            &mut HashMap::new()
        ));
    }

    #[test]
    fn gateway_routes_datastore_methods() {
        let gw = RestGateway::new(false).unwrap();
        let (route, vars) = gw
            .match_route(&Method::POST, "/v1/projects/p:lookup")
            .unwrap();
        assert_eq!(route.grpc_path, "/google.datastore.v1.Datastore/Lookup");
        assert_eq!(vars["project_id"], "p");
        assert!(
            gw.match_route(&Method::GET, "/v1/projects/p:lookup")
                .is_none()
        );
        assert!(
            gw.match_route(&Method::POST, "/v1/projects/example.com:proj:lookup")
                .is_none()
        );
        assert!(gw.match_route(&Method::GET, "/v1/config").is_some());
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("p%20q%7E%zz%"), "p q~%zz%");
        assert_eq!(percent_decode("é%C3%A9"), "éé");
    }
}
