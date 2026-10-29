//! REST conformance scenarios.
//!
//! A scenario (`tests/conformance/scenarios/*.scn`) is a list of HTTP requests run in order
//! against a freshly reset emulator:
//!
//! ```text
//! # comments start with '#'
//! POST /v1/projects/p:commit
//! {"mode": "NON_TRANSACTIONAL", "mutations": [...]}
//!
//! POST /v1/projects/p:lookup
//! {"keys": [...]}
//! ```
//!
//! A line starting with an HTTP method begins a step; the following lines (up to the next step)
//! are its body. `{{repeat N text}}` in a path or body expands to `text` repeated N times (for size
//! limits). A line `> name = batch.endCursor` after a step captures that JSON field (dotted path,
//! numeric segments index arrays) from its response; later steps use it as `{{name}}`.
//! Responses recorded from the Java emulator are stored as goldens
//! (`tests/conformance/java/*.golden`) and compared after [`normalize`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub mod project;
mod v3;

/// Step keywords: HTTP methods, plus `FILE`, `LS`, `RECORDS`, `PROTO` and `RESTART` for project cases
/// ([`project`]).
const METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD", "FILE", "LS", "RECORDS",
    "ENTITIES", "PROTO", "GLOB", "RESTART", "V3", "V3HEX",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub method: String,
    pub path: String,
    pub body: String,
    /// `(variable, json path)` pairs captured from the response.
    pub captures: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub method: String,
    pub path: String,
    pub status: u16,
    pub body: String,
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn scenario_dir() -> PathBuf {
    workspace_root().join("tests/conformance/scenarios")
}

pub fn golden_dir() -> PathBuf {
    workspace_root().join("tests/conformance/java")
}

/// Goldens recorded from a Java emulator started with `--firestore_in_datastore_mode`.
pub fn firestore_golden_dir() -> PathBuf {
    workspace_root().join("tests/conformance/java-firestore")
}

/// All scenario names (file stems), sorted.
pub fn scenario_names() -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(scenario_dir())?
        .filter_map(|e| {
            let path = e.ok()?.path();
            (path.extension()? == "scn").then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    Ok(names)
}

pub fn load_scenario(name: &str) -> Result<Vec<Step>> {
    let path = scenario_dir().join(format!("{name}.scn"));
    parse_scenario(
        &std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )
}

pub fn parse_scenario(text: &str) -> Result<Vec<Step>> {
    let mut steps: Vec<Step> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let first_word = line.split(' ').next().unwrap_or_default();
        if METHODS.contains(&first_word) {
            let (method, path) = line.split_once(' ').unwrap_or((line, ""));
            steps.push(Step {
                method: method.into(),
                path: path.trim().into(),
                body: String::new(),
                captures: Vec::new(),
            });
        } else if let Some(capture) = line.strip_prefix("> ") {
            let (name, path) = capture
                .split_once('=')
                .context("expected `> name = path`")?;
            let step = steps.last_mut().context("capture before first step")?;
            step.captures.push((name.trim().into(), path.trim().into()));
        } else if let Some(step) = steps.last_mut() {
            if !line.trim().is_empty() || !step.body.is_empty() {
                step.body.push_str(line);
                step.body.push('\n');
            }
        } else if !line.trim().is_empty() {
            bail!("body before first step: {line}");
        }
    }
    for step in &mut steps {
        step.body = expand_repeats(step.body.trim_end())?;
        step.path = expand_repeats(&step.path)?;
    }
    Ok(steps)
}

fn expand_repeats(body: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("{{repeat ") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("}}").context("unterminated {{repeat")? + start;
        let spec = &rest[start + "{{repeat ".len()..end];
        let (count, text) = spec.split_once(' ').context("expected {{repeat N text}}")?;
        out.push_str(&text.repeat(count.parse()?));
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into()
}

/// Resets the emulator at `base` (e.g. `http://localhost:8081`) and runs every step.
pub fn run(base: &str, steps: &[Step]) -> Result<Vec<Exchange>> {
    let agent = agent();
    let reset = agent.post(format!("{base}/reset")).send_empty()?;
    if reset.status() != 200 {
        bail!(
            "reset failed with {} (is the emulator running with --testing?)",
            reset.status()
        );
    }
    let mut out = Vec::new();
    let mut vars = std::collections::HashMap::new();
    for step in steps {
        let (status, body) = exchange(&agent, base, step, &mut vars)?;
        out.push(Exchange {
            method: step.method.clone(),
            path: step.path.clone(),
            status,
            body,
        });
    }
    Ok(out)
}

/// Sends one HTTP step and records its captures. Returns status and body.
pub(crate) fn exchange(
    agent: &ureq::Agent,
    base: &str,
    step: &Step,
    vars: &mut std::collections::HashMap<String, String>,
) -> Result<(u16, String)> {
    if step.method.starts_with("V3") {
        let body = substitute(&step.body, vars)?;
        return v3::exchange(base, &step.method, &step.path, &body, &step.captures, vars);
    }
    let url = format!("{base}{}", substitute(&step.path, vars)?);
    let body = substitute(&step.body, vars)?;
    let builder = ureq::http::Request::builder()
        .method(step.method.as_str())
        .uri(&url)
        .header("content-type", "application/json");
    let result = if body.is_empty() {
        agent.run(builder.body(())?)
    } else {
        agent.run(builder.body(body)?)
    };
    // A request the server never answers is recorded as status 0 rather than aborting.
    let (status, body) = match result {
        Ok(mut response) => (
            response.status().as_u16(),
            response.body_mut().read_to_string()?,
        ),
        Err(ureq::Error::Timeout(_)) => (0, "<timeout>".to_string()),
        Err(e) => return Err(e).with_context(|| format!("{} {url}", step.method)),
    };
    // A capture whose field is absent (e.g. the step failed) leaves the variable unset; a later
    // `{{name}}` then reports it.
    for (name, path) in &step.captures {
        let value = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| lookup_path(&v, path).cloned());
        if let Some(value) = value {
            let text = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            vars.insert(name.clone(), text);
        }
    }
    Ok((status, body))
}

/// Replaces `{{name}}` with captured values.
fn substitute(text: &str, vars: &std::collections::HashMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start..].find("}}").context("unterminated {{")? + start;
        let name = &rest[start + 2..end];
        // `{{name:fixed64}}`: a captured integer as little-endian fixed64 hex (for V3HEX bodies).
        let (name, fixed64) = match name.strip_suffix(":fixed64") {
            Some(n) => (n, true),
            None => (name, false),
        };
        let value = vars
            .get(name)
            .with_context(|| format!("unknown variable {{{{{name}}}}}"))?;
        if fixed64 {
            let v: u64 = value.parse().context("fixed64 of a non-integer")?;
            for b in v.to_le_bytes() {
                let _ = write!(out, "{b:02x}");
            }
        } else {
            out.push_str(value);
        }
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .try_fold(value, |v, segment| match segment.parse::<usize>() {
            Ok(i) => v.get(i),
            Err(_) => v.get(segment),
        })
}

/// Serializes exchanges in the golden-file format.
pub fn to_golden(exchanges: &[Exchange]) -> String {
    let mut out = String::new();
    for e in exchanges {
        let _ = write!(
            out,
            ">>> {} {}\n<<< {}\n{}\n<<< END\n",
            e.method, e.path, e.status, e.body
        );
    }
    out
}

pub fn parse_golden(text: &str) -> Result<Vec<Exchange>> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (request, after) = rest.split_once('\n').context("truncated golden")?;
        let request = request.strip_prefix(">>> ").context("expected >>> line")?;
        let (method, path) = request.split_once(' ').context("bad request line")?;
        let (status, after) = after.split_once('\n').context("truncated golden")?;
        let status = status
            .strip_prefix("<<< ")
            .context("expected <<< status")?
            .parse()?;
        let (body, after) = after.split_once("\n<<< END\n").context("missing <<< END")?;
        out.push(Exchange {
            method: method.into(),
            path: path.into(),
            status,
            body: body.into(),
        });
        rest = after;
    }
    Ok(out)
}

/// Canonical form for comparison. Proto map fields (`properties`) are unordered, and Java emits
/// them in `HashMap` order, so their keys are sorted. Everything else, including layout, must
/// match byte for byte.
/// Normalizes every body of a scenario run. Besides [`normalize`], transaction handles (opaque,
/// and numbered by a counter that survives `/reset` in Java) become `<txn-N>` by first appearance.
pub fn normalize_run(exchanges: &[Exchange]) -> Vec<String> {
    let mut handles: Vec<String> = Vec::new();
    let mut v3_handles: Vec<String> = Vec::new();
    exchanges
        .iter()
        .map(|e| {
            if e.method.starts_with("V3") {
                return v3::normalize_handles(&e.body, &mut v3_handles);
            }
            let body = normalize(&e.body);
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
                return body;
            };
            if !replace_handles(&mut value, &mut handles) {
                return body;
            }
            let trailing = if body.ends_with('\n') { "\n" } else { "" };
            format!("{}{trailing}", oxide_json::to_java_pretty(&value))
        })
        .collect()
}

fn replace_handles(value: &mut serde_json::Value, handles: &mut Vec<String>) -> bool {
    let mut changed = false;
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                match v {
                    serde_json::Value::String(h) if key == "transaction" => {
                        let n = handles.iter().position(|x| x == h).unwrap_or_else(|| {
                            handles.push(h.clone());
                            handles.len() - 1
                        });
                        *v = serde_json::Value::String(format!("<txn-{n}>"));
                        changed = true;
                    }
                    _ => changed |= replace_handles(v, handles),
                }
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                changed |= replace_handles(v, handles);
            }
        }
        _ => {}
    }
    changed
}

pub fn normalize(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    if !body.contains('\n') {
        return body.to_string(); // compact error bodies are compared verbatim
    }
    sort_property_maps(&mut value);
    let trailing = if body.ends_with('\n') { "\n" } else { "" };
    format!("{}{trailing}", oxide_json::to_java_pretty(&value))
}

fn sort_property_maps(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if key == "properties"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_steps_and_bodies() {
        let steps =
            parse_scenario("# c\nPOST /v1/projects/p:commit\n{\"a\":\n 1}\n\nGET /\n\nPOST /x\n")
                .unwrap();
        assert_eq!(steps.len(), 3);
        let with_capture = parse_scenario("POST /x\n{}\n> c = batch.endCursor\n").unwrap();
        assert_eq!(
            with_capture[0].captures,
            vec![("c".to_string(), "batch.endCursor".to_string())]
        );
        assert_eq!(with_capture[0].body, "{}");
        assert_eq!(steps[0].body, "{\"a\":\n 1}");
        assert_eq!(
            parse_scenario("POST /\n\"{{repeat 3 ab}}\"").unwrap()[0].body,
            "\"ababab\""
        );
        assert_eq!(
            steps[1],
            Step {
                method: "GET".into(),
                path: "/".into(),
                body: String::new(),
                captures: vec![]
            }
        );
    }

    #[test]
    fn golden_round_trip() {
        let ex = vec![
            Exchange {
                method: "POST".into(),
                path: "/a".into(),
                status: 200,
                body: "{\n}\n".into(),
            },
            Exchange {
                method: "GET".into(),
                path: "/b".into(),
                status: 400,
                body: "{\"error\":1}".into(),
            },
        ];
        assert_eq!(parse_golden(&to_golden(&ex)).unwrap(), ex);
    }
}
