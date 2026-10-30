//! A data viewer at `/_ah/admin/datastore`, like the old App Engine development server's admin
//! console (superset only: the Java emulator answers 404). Read-only HTML pages listing projects,
//! their kinds and their entities, with a delete button per entity.
//!
//! - `GET /_ah/admin/datastore`: projects.
//! - `GET /_ah/admin/datastore?project=p`: databases, namespaces and kinds, with entity counts.
//! - `GET /_ah/admin/datastore?project=p&database=d&namespace=n&kind=k&offset=o`: entities.
//! - `POST /_ah/admin/datastore/delete?project=p&key=k&back=url`: deletes one entity.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Method, Response, StatusCode, header};
use oxide_engine::query::Query;
use oxide_engine::{Engine, Mutation, Op, ReadMode};
use oxide_model::{Entity, Key, Value, ValueKind};
use prost::Message;
use tonic::body::Body;

pub const PATH: &str = "/_ah/admin/datastore";
const DELETE_PATH: &str = "/_ah/admin/datastore/delete";
/// Entities per page.
const PAGE: usize = 50;
/// Longest string value shown in full.
const MAX_SHOWN: usize = 200;

/// Serves a viewer request, or `None` for other paths.
pub fn handle(method: &Method, path: &str, query: &str, engine: &Engine) -> Option<Response<Body>> {
    let params = Params::parse(query);
    match (method, path) {
        (&Method::GET, PATH) => Some(page(&params, engine)),
        (&Method::POST, DELETE_PATH) => Some(delete(&params, engine)),
        _ => None,
    }
}

struct Params(Vec<(String, String)>);

impl Params {
    fn parse(query: &str) -> Self {
        Self(
            query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (decode(k), decode(v))
                })
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Percent-decoding of a query component (`+` is a space).
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(status: StatusCode, title: &str, body: &str) -> Response<Body> {
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>\
         body{{font-family:system-ui,sans-serif;margin:1.5em;color:#222}}\
         table{{border-collapse:collapse;font-size:14px}}\
         td,th{{border:1px solid #ccc;padding:4px 8px;text-align:left;vertical-align:top}}\
         th{{background:#f3f3f3}}code{{font-size:13px}}a{{color:#0645ad}}\
         </style></head><body><h1>{title}</h1>{body}</body></html>",
        title = escape(title)
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::new(http_body_util::Full::new(bytes::Bytes::from(
            page,
        ))))
        .expect("valid response")
}

fn link(params: &[(&str, &str)]) -> String {
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode(v)))
        .collect();
    format!("{PATH}?{}", query.join("&"))
}

fn page(params: &Params, engine: &Engine) -> Response<Body> {
    let Some(project) = params.get("project") else {
        let mut body = String::from("<ul>");
        for p in engine.project_ids() {
            let _ = write!(
                body,
                "<li><a href=\"{}\">{}</a></li>",
                escape(&link(&[("project", &p)])),
                escape(&p)
            );
        }
        body.push_str("</ul>");
        if engine.project_ids().is_empty() {
            body = "<p>No data yet.</p>".into();
        }
        return html(StatusCode::OK, "Datastore viewer", &body);
    };
    let database = params.get("database").unwrap_or("");
    let namespace = params.get("namespace").unwrap_or("");
    match params.get("kind") {
        None => kinds(project, engine),
        Some(kind) => {
            let offset = params
                .get("offset")
                .and_then(|o| o.parse().ok())
                .unwrap_or(0);
            entities(project, database, namespace, kind, offset, engine)
        }
    }
}

fn kinds(project: &str, engine: &Engine) -> Response<Body> {
    let mut body = format!(
        "<p><a href=\"{PATH}\">Projects</a></p>\
         <table><tr><th>Database</th><th>Namespace</th><th>Kind</th><th>Entities</th></tr>"
    );
    for (database, namespace, kind, count) in engine.kind_summary(project) {
        let href = link(&[
            ("project", project),
            ("database", &database),
            ("namespace", &namespace),
            ("kind", &kind),
        ]);
        let _ = write!(
            body,
            "<tr><td>{}</td><td>{}</td><td><a href=\"{}\">{}</a></td><td>{count}</td></tr>",
            escape(if database.is_empty() {
                "(default)"
            } else {
                &database
            }),
            escape(if namespace.is_empty() {
                "(default)"
            } else {
                &namespace
            }),
            escape(&href),
            escape(&kind)
        );
    }
    body.push_str("</table>");
    html(StatusCode::OK, &format!("Project {project}"), &body)
}

fn entities(
    project: &str,
    database: &str,
    namespace: &str,
    kind: &str,
    offset: usize,
    engine: &Engine,
) -> Response<Body> {
    let query = Query {
        project_id: project.into(),
        database_id: database.into(),
        namespace_id: namespace.into(),
        kind: Some(kind.into()),
        offset,
        limit: Some(PAGE + 1),
        unbatched: true,
        ..Default::default()
    };
    let rows = match engine.run_query(&query, ReadMode::Strong) {
        Ok(batch) => batch.results,
        Err(e) => {
            return html(
                StatusCode::BAD_REQUEST,
                "Datastore viewer",
                &format!("<p>{}</p>", escape(&e.message)),
            );
        }
    };
    let rows: Vec<&Entity> = rows
        .iter()
        .skip(offset)
        .filter_map(|r| r.entity.as_ref().map(|(e, _)| e))
        .collect();
    let more = rows.len() > PAGE;
    let rows = &rows[..rows.len().min(PAGE)];
    let columns: BTreeSet<&str> = rows.iter().flat_map(|e| e.properties.keys()).collect();
    let here = link(&[
        ("project", project),
        ("database", database),
        ("namespace", namespace),
        ("kind", kind),
        ("offset", &offset.to_string()),
    ]);
    let mut body = format!(
        "<p><a href=\"{}\">All kinds of {}</a></p><table><tr><th>Key</th>",
        escape(&link(&[("project", project)])),
        escape(project)
    );
    for c in &columns {
        let _ = write!(body, "<th>{}</th>", escape(c));
    }
    body.push_str("<th></th></tr>");
    for e in rows {
        let Some(key) = &e.key else { continue };
        let _ = write!(
            body,
            "<tr><td><code>{}</code></td>",
            escape(&key.display_path())
        );
        for c in &columns {
            let shown = e.properties.get(*c).map(show).unwrap_or_default();
            let _ = write!(body, "<td>{}</td>", escape(&shown));
        }
        let delete = format!(
            "{DELETE_PATH}?project={}&key={}&back={}",
            encode(project),
            encode_key(key),
            encode(&here)
        );
        let _ = write!(
            body,
            "<td><form method=\"post\" action=\"{}\"><button>Delete</button></form></td></tr>",
            escape(&delete)
        );
    }
    body.push_str("</table><p>");
    if offset > 0 {
        let prev = link(&[
            ("project", project),
            ("database", database),
            ("namespace", namespace),
            ("kind", kind),
            ("offset", &offset.saturating_sub(PAGE).to_string()),
        ]);
        let _ = write!(body, "<a href=\"{}\">Previous</a> ", escape(&prev));
    }
    if more {
        let next = link(&[
            ("project", project),
            ("database", database),
            ("namespace", namespace),
            ("kind", kind),
            ("offset", &(offset + PAGE).to_string()),
        ]);
        let _ = write!(body, "<a href=\"{}\">Next</a>", escape(&next));
    }
    body.push_str("</p>");
    html(StatusCode::OK, &format!("Kind {kind}"), &body)
}

fn encode_key(key: &Key) -> String {
    URL_SAFE_NO_PAD.encode(oxide_api::convert::key_to_proto(key).encode_to_vec())
}

fn decode_key(s: &str) -> Option<Key> {
    let bytes = URL_SAFE_NO_PAD.decode(s).ok()?;
    let proto = oxide_proto::google::datastore::v1::Key::decode(bytes.as_slice()).ok()?;
    oxide_api::convert::stored_key_from_proto(&proto)
}

fn delete(params: &Params, engine: &Engine) -> Response<Body> {
    let (Some(project), Some(key)) = (
        params.get("project"),
        params.get("key").and_then(decode_key),
    ) else {
        return html(
            StatusCode::BAD_REQUEST,
            "Datastore viewer",
            "<p>Bad request.</p>",
        );
    };
    let result = engine.commit_non_transactional(
        project,
        vec![Mutation {
            op: Op::Delete(key),
            base_version: None,
            on_conflict: Default::default(),
            partial: Default::default(),
        }],
    );
    if let Err(e) = result {
        return html(
            StatusCode::BAD_REQUEST,
            "Datastore viewer",
            &format!("<p>{}</p>", escape(&e.message)),
        );
    }
    let back = params
        .get("back")
        .filter(|b| b.starts_with(PATH))
        .unwrap_or(PATH);
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, back)
        .body(Body::empty())
        .expect("valid response")
}

/// A value as the viewer shows it.
fn show(v: &Value) -> String {
    match &v.kind {
        ValueKind::Null => "null".into(),
        ValueKind::Boolean(b) => b.to_string(),
        ValueKind::Integer(i) => i.to_string(),
        ValueKind::Double(d) => d.to_string(),
        ValueKind::Timestamp(t) => oxide_api::convert::micros_to_timestamp(*t).to_string(),
        ValueKind::Key(k) => k.display_path(),
        ValueKind::String(s) if s.chars().count() > MAX_SHOWN => {
            format!("{}…", s.chars().take(MAX_SHOWN).collect::<String>())
        }
        ValueKind::String(s) => s.clone(),
        ValueKind::Blob(b) => format!("<blob, {} bytes>", b.len()),
        ValueKind::GeoPoint {
            latitude,
            longitude,
        } => format!("({latitude}, {longitude})"),
        ValueKind::Entity(e) => {
            let fields: Vec<String> = e
                .properties
                .iter()
                .map(|(k, v)| format!("{k}: {}", show(v)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        ValueKind::Array(a) => {
            let items: Vec<String> = a.iter().map(show).collect();
            format!("[{}]", items.join(", "))
        }
    }
}
