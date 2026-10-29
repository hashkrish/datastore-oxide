//! App ids (`s~p`, `e~p`, `dev~p`, `p`, ...) and the project ids they stand for.

use std::collections::HashMap;
use std::sync::Mutex;

/// An app id that is not a valid project id even after scrubbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unscrubbable;

/// Maps app ids to project ids and back, remembering the last app id seen for each project. The
/// memory lives as long as the process (it survives `/reset`).
#[derive(Default)]
pub struct Resolver {
    app_ids: Mutex<HashMap<String, String>>,
}

impl Resolver {
    pub fn to_project_id(&self, app_id: &str) -> Result<String, Unscrubbable> {
        let project = scrubbed_id(app_id)?;
        if project != app_id && parse_app_id(app_id).is_none() {
            tracing::warn!(
                "Provided application id '{app_id}' is invalid for storage in Cloud Datastore \
                 Emulator. Using '{project}' instead."
            );
        }
        self.app_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(project.clone(), app_id.to_string());
        Ok(project)
    }

    pub fn to_app_id(&self, project_id: &str) -> String {
        match self
            .app_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(project_id)
        {
            Some(app) => app.clone(),
            None => {
                tracing::warn!(
                    "Attempting to convert a stored project ID to an application ID failed. The \
                     application ID of returned entities may have changed."
                );
                project_id.to_string()
            }
        }
    }
}

/// The project id of `app_id`: its own if valid, else the scrubbed form, if that is valid.
pub fn scrubbed_id(app_id: &str) -> Result<String, Unscrubbable> {
    if app_id.is_empty() {
        return Ok(String::new());
    }
    if let Some(project) = parse_app_id(app_id) {
        return Ok(project);
    }
    let rest = app_id.split_once('~').map_or(app_id, |(_, rest)| rest);
    let scrubbed = match rest.split_once(':') {
        Some((domain, name)) => format!(
            "{}:scrubbed-{}",
            replace_invalid(domain, 100, ".-"),
            replace_invalid(name, 91, "-")
        ),
        None => format!("scrubbed-{}", replace_invalid(rest, 91, "-")),
    };
    if valid_project(&scrubbed) {
        Ok(scrubbed)
    } else {
        Err(Unscrubbable)
    }
}

/// At most `max` characters of `s`, with characters outside `[a-z0-9]` and `extra` replaced by `-`.
fn replace_invalid(s: &str, max: usize, extra: &str) -> String {
    s.chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The project id of a valid app id (`[partition~]project`), or `None`.
pub fn parse_app_id(a: &str) -> Option<String> {
    let project = match a.split_once('~') {
        Some((partition, rest)) => {
            let partition_ok = (1..=100).contains(&partition.len())
                && partition
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !partition_ok || rest.is_empty() {
                return None;
            }
            rest
        }
        None => a,
    };
    if let Some(inner) = project
        .strip_prefix("__metadata-")
        .and_then(|m| m.strip_suffix("__"))
    {
        let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
        if let Some(target) = inner[digits..].strip_prefix('_')
            && digits > 0
            && valid_plain(target)
        {
            return Some(target.to_string());
        }
    }
    valid_project(project).then(|| project.to_string())
}

/// `[domain:]name`, optionally wrapped in `__…__`.
fn valid_project(p: &str) -> bool {
    if let Some(inner) = p
        .strip_prefix("__")
        .and_then(|s| s.strip_suffix("__"))
        .filter(|_| p.len() >= 4)
        && valid_plain(inner)
    {
        return true;
    }
    valid_plain(p)
}

fn valid_plain(p: &str) -> bool {
    let (domain, name) = match p.split_once(':') {
        Some((domain, name)) => (Some(domain), name),
        None => (None, p),
    };
    let part = |s: &str, extra: &[u8]| {
        s.len() <= 100
            && s.bytes().enumerate().all(|(i, b)| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || (i > 0 && extra.contains(&b))
            })
    };
    domain.is_none_or(|d| !d.is_empty() && part(d, b".-")) && part(name, b"-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_app_ids_like_java() {
        for (app, project) in [
            ("s~p", "p"),
            ("e~p", "p"),
            ("dev~p", "p"),
            ("p", "p"),
            ("example.com:foo", "example.com:foo"),
            ("s~example.com:foo", "example.com:foo"),
            ("__metadata-12_p__", "p"),
            ("s~__x__", "__x__"),
            ("S~Foo", "scrubbed--oo"),
            ("a~b~c", "scrubbed-b-c"),
            ("~p", "scrubbed-p"),
            ("-p", "scrubbed--p"),
            ("s~-p", "scrubbed--p"),
            ("s~p_q", "scrubbed-p-q"),
            ("s~p3#db", "scrubbed-p3-db"),
            ("s~", "scrubbed-"),
            ("", ""),
        ] {
            assert_eq!(scrubbed_id(app).as_deref(), Ok(project), "{app}");
        }
        assert_eq!(
            scrubbed_id(&format!("{}~p", "a".repeat(101))).as_deref(),
            Ok("scrubbed-p")
        );
        assert_eq!(
            scrubbed_id(&"x".repeat(101)),
            Ok(format!("scrubbed-{}", "x".repeat(91)))
        );
        assert_eq!(scrubbed_id("s~Example.com:Foo"), Err(Unscrubbable));
    }

    #[test]
    fn remembers_the_last_app_id() {
        let r = Resolver::default();
        assert_eq!(r.to_app_id("p"), "p");
        r.to_project_id("s~p").unwrap();
        r.to_project_id("e~p").unwrap();
        assert_eq!(r.to_app_id("p"), "e~p");
    }
}
