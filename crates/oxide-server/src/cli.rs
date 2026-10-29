//! Command-line parsing that mirrors the Java emulator's `cloud_datastore_emulator` exactly,
//! including its quirks (recorded in `tests/golden/cli/java`):
//!
//! - Options are matched in order; parsing stops at the first argument that is not an option.
//! - Value options only accept `--name=value` (and `-p VALUE` for the port); flags only `--name`.
//! - Option values are validated as soon as they are matched.
//! - The last unmatched argument is the directory; any other leftovers are "Unexpected option(s)".
//! - All messages go to stdout and failures exit with status 1.

use std::path::{Path, PathBuf};

const MAIN_USAGE: &str = include_str!("help/main.txt");
const START_HELP: &str = include_str!("help/start.txt");
const CREATE_HELP: &str = include_str!("help/create.txt");
const LICENSE_HELP: &str = include_str!("help/license.txt");
const HELP_HELP: &str = include_str!("help/help.txt");

/// What `main` should do after parsing.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Print `stdout` and exit with `code`.
    Exit {
        stdout: String,
        code: i32,
    },
    Start(StartConfig),
    Create(CreateConfig),
    License,
}

impl Command {
    fn print(text: &str, code: i32) -> Self {
        Command::Exit {
            stdout: text.to_string(),
            code,
        }
    }

    fn fail(msg: impl std::fmt::Display) -> Self {
        Command::Exit {
            stdout: format!("{msg}\nExiting.\n"),
            code: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoIdPolicy {
    #[default]
    Sequential,
    Scattered,
}

/// Options of the `start` action, as given on the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct StartConfig {
    pub host: String,
    /// Kept as given (Java `int`); range-checked when binding, like the Java emulator.
    pub port: i32,
    pub testing: bool,
    pub consistency: Option<f32>,
    pub store_on_disk: Option<bool>,
    pub store_index_configuration_on_disk: Option<bool>,
    pub storage_file: Option<PathBuf>,
    pub index_file: Option<PathBuf>,
    pub allow_reserved_kinds: bool,
    pub require_indexes: bool,
    pub regenerate_indexes: bool,
    pub auto_id_policy: AutoIdPolicy,
    pub firestore_in_datastore_mode: bool,
    pub enable_entity_groups: Option<bool>,
    pub allow_remote_shutdown: bool,
    pub directory: Option<PathBuf>,
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8080,
            testing: false,
            consistency: None,
            store_on_disk: None,
            store_index_configuration_on_disk: None,
            storage_file: None,
            index_file: None,
            allow_reserved_kinds: false,
            require_indexes: false,
            regenerate_indexes: true,
            auto_id_policy: AutoIdPolicy::Sequential,
            firestore_in_datastore_mode: false,
            enable_entity_groups: None,
            allow_remote_shutdown: false,
            directory: None,
        }
    }
}

impl StartConfig {
    /// `--testing` implies in-memory storage unless `--store_on_disk` is given explicitly.
    pub fn store_on_disk(&self) -> bool {
        self.store_on_disk.unwrap_or(!self.testing)
    }

    /// Fraction of eventually-consistent job applications that succeed.
    pub fn consistency(&self) -> f32 {
        if self.firestore_in_datastore_mode {
            1.0
        } else {
            self.consistency
                .unwrap_or(if self.testing { 1.0 } else { 0.9 })
        }
    }

    /// The consistency Java stores in its config even in Firestore-in-Datastore mode, where it
    /// only takes effect after a config update.
    pub fn stored_consistency(&self) -> f32 {
        self.consistency
            .unwrap_or(if self.testing { 1.0 } else { 0.9 })
    }

    pub fn store_index_configuration_on_disk(&self) -> bool {
        self.store_index_configuration_on_disk
            .unwrap_or(!self.testing)
    }

    /// Where data is persisted when storing on disk. The default differs from Java's
    /// `local_db.bin` because the file format differs; data moves between the two via export/import.
    pub fn storage_path(&self) -> Option<PathBuf> {
        if !self.store_on_disk() {
            return None;
        }
        self.storage_file.clone().or_else(|| {
            self.directory
                .as_ref()
                .map(|d| d.join(DEFAULT_STORAGE_FILE))
        })
    }
}

/// Options of the `create` action.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateConfig {
    pub project_id: Option<String>,
    pub directory: PathBuf,
}

/// Default storage location, relative to the project directory.
pub const DEFAULT_STORAGE_FILE: &str = "WEB-INF/appengine-generated/oxide_db.bin";

#[derive(Clone, Copy)]
enum Kind {
    Flag,
    Value,
}

struct OptSpec {
    name: &'static str,
    short: Option<&'static str>,
    kind: Kind,
}

const fn flag(name: &'static str) -> OptSpec {
    OptSpec {
        name,
        short: None,
        kind: Kind::Flag,
    }
}

const fn value(name: &'static str) -> OptSpec {
    OptSpec {
        name,
        short: None,
        kind: Kind::Value,
    }
}

const START_OPTIONS: &[OptSpec] = &[
    value("host"),
    OptSpec {
        name: "port",
        short: Some("p"),
        kind: Kind::Value,
    },
    flag("testing"),
    value("consistency"),
    value("store_on_disk"),
    value("store_index_configuration_on_disk"),
    value("storage_file"),
    value("index_file"),
    flag("allow_reserved_kinds"),
    flag("require_indexes"),
    value("regenerate_indexes"),
    value("auto_id_policy"),
    flag("firestore_in_datastore_mode"),
    value("enable_entity_groups"),
    flag("allow_remote_shutdown"),
];

const CREATE_OPTIONS: &[OptSpec] = &[value("project_id")];

/// A matched option: its canonical name, the form the user typed (for error messages) and value.
struct Matched<'a> {
    name: &'static str,
    form: String,
    value: Option<&'a str>,
}

/// Matches options from the front of `args`; returns them plus the unmatched remainder.
fn match_options<'a>(specs: &[OptSpec], args: &'a [String]) -> (Vec<Matched<'a>>, Vec<&'a str>) {
    let mut matched = Vec::new();
    let mut i = 0;
    'args: while i < args.len() {
        let arg = args[i].as_str();
        for spec in specs {
            let long = format!("--{}", spec.name);
            match spec.kind {
                Kind::Flag if arg == long => {
                    matched.push(Matched {
                        name: spec.name,
                        form: long,
                        value: None,
                    });
                    i += 1;
                    continue 'args;
                }
                Kind::Value => {
                    if let Some(v) = arg
                        .strip_prefix(long.as_str())
                        .and_then(|r| r.strip_prefix('='))
                    {
                        matched.push(Matched {
                            name: spec.name,
                            form: long,
                            value: Some(v),
                        });
                        i += 1;
                        continue 'args;
                    }
                    if let (Some(short), Some(next)) = (spec.short, args.get(i + 1))
                        && arg.strip_prefix('-') == Some(short)
                    {
                        matched.push(Matched {
                            name: spec.name,
                            form: arg.to_string(),
                            value: Some(next),
                        });
                        i += 2;
                        continue 'args;
                    }
                }
                Kind::Flag => {}
            }
        }
        break;
    }
    (matched, args[i..].iter().map(String::as_str).collect())
}

/// Splits the leftovers into the trailing directory argument and an error for anything else.
fn take_directory(mut rest: Vec<&str>) -> Result<Option<&str>, Command> {
    let dir = rest.pop();
    if rest.is_empty() {
        Ok(dir)
    } else {
        Err(Command::fail(format_args!(
            "Unexpected option(s): {}.",
            rest.join(" ")
        )))
    }
}

pub fn parse(args: &[String]) -> Command {
    let Some((action, rest)) = args.split_first() else {
        return Command::print(MAIN_USAGE, 1);
    };
    match action.as_str() {
        "start" => parse_start(rest),
        "create" => parse_create(rest),
        "license" => match take_directory(rest.iter().map(String::as_str).collect()) {
            Ok(None) => Command::License,
            Ok(Some(extra)) => Command::fail(format_args!("Unexpected option(s): {extra}.")),
            Err(e) => e,
        },
        "help" => parse_help(rest),
        _ => Command::print(MAIN_USAGE, 1),
    }
}

fn parse_help(args: &[String]) -> Command {
    let target = match take_directory(args.iter().map(String::as_str).collect()) {
        Ok(Some(target)) => target,
        Ok(None) => return Command::print(MAIN_USAGE, 1),
        Err(e) => return e,
    };
    match target {
        "start" => Command::print(START_HELP, 0),
        "create" => Command::print(CREATE_HELP, 0),
        "license" => Command::print(LICENSE_HELP, 0),
        "help" => Command::print(HELP_HELP, 0),
        other => Command::fail(format_args!("Unknown action: {other}.")),
    }
}

fn parse_bool(m: &Matched) -> Result<bool, Command> {
    let v = m.value.unwrap_or_default();
    if v.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if v.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(Command::fail(format_args!(
            "Invalid boolean value for {}: {v}. Must be 'true' or 'false'.",
            m.form
        )))
    }
}

fn parse_start(args: &[String]) -> Command {
    let (matched, rest) = match_options(START_OPTIONS, args);
    let mut cfg = StartConfig::default();
    for m in &matched {
        if let Err(e) = apply_start_option(&mut cfg, m) {
            return e;
        }
    }

    let dir = match take_directory(rest) {
        Ok(dir) => dir,
        Err(e) => return e,
    };
    if let Some(dir) = dir {
        if let Err(e) = check_project_directory(Path::new(dir)) {
            return e;
        }
        cfg.directory = Some(dir.into());
    } else if cfg.store_on_disk() {
        return Command::print(START_HELP, 1);
    }
    Command::Start(cfg)
}

/// Applies one matched `start` option, validating its value the way Java does.
fn apply_start_option(cfg: &mut StartConfig, m: &Matched) -> Result<(), Command> {
    let v = m.value.unwrap_or_default();
    match m.name {
        "host" => cfg.host = v.to_string(),
        "port" => {
            cfg.port = v.parse().map_err(|_| {
                Command::fail(format_args!("Invalid int value for {}: {v}.", m.form))
            })?
        }
        "testing" => cfg.testing = true,
        "consistency" => {
            let c = parse_java_float(v).ok_or_else(|| {
                Command::fail(format_args!("Invalid float value for {}: {v}.", m.form))
            })?;
            // Written as comparisons (not a range check) so NaN passes, as in Java.
            #[allow(clippy::manual_range_contains)]
            if c < 0.0 || c > 1.0 {
                return Err(Command::fail(format_args!(
                    "Invalid value for {}: {}. Must be in [0.0, 1.0].",
                    m.form,
                    java_format_f(c)
                )));
            }
            cfg.consistency = Some(c);
        }
        "store_on_disk" => cfg.store_on_disk = Some(parse_bool(m)?),
        "store_index_configuration_on_disk" => {
            cfg.store_index_configuration_on_disk = Some(parse_bool(m)?)
        }
        "storage_file" => cfg.storage_file = Some(v.into()),
        "index_file" => cfg.index_file = Some(v.into()),
        "allow_reserved_kinds" => cfg.allow_reserved_kinds = true,
        "require_indexes" => cfg.require_indexes = true,
        "regenerate_indexes" => cfg.regenerate_indexes = parse_bool(m)?,
        "auto_id_policy" => {
            cfg.auto_id_policy = match v {
                "SEQUENTIAL" => AutoIdPolicy::Sequential,
                "SCATTERED" => AutoIdPolicy::Scattered,
                _ => {
                    return Err(Command::fail(
                        "Invalid value for auto_id_policy, should be either SEQUENTIAL or SCATTERED",
                    ));
                }
            }
        }
        "firestore_in_datastore_mode" => cfg.firestore_in_datastore_mode = true,
        "enable_entity_groups" => cfg.enable_entity_groups = Some(parse_bool(m)?),
        "allow_remote_shutdown" => cfg.allow_remote_shutdown = true,
        _ => unreachable!("unhandled option {}", m.name),
    }
    Ok(())
}

/// `Float.parseFloat`: surrounding whitespace is trimmed, an `f`/`F`/`d`/`D` suffix is allowed,
/// and the only spelled-out values are `NaN` and `Infinity` (case-sensitive). Hex floats are not
/// supported.
fn parse_java_float(s: &str) -> Option<f32> {
    let s = s.trim_matches(|c: char| c <= ' ');
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return match unsigned {
            "NaN" | "Infinity" => s.replace("Infinity", "inf").parse().ok(),
            _ => None,
        };
    }
    s.strip_suffix(['f', 'F', 'd', 'D'])
        .unwrap_or(s)
        .parse()
        .ok()
}

/// Java's `String.format("%f", x)`.
fn java_format_f(x: f32) -> String {
    match x {
        f32::INFINITY => "Infinity".into(),
        f32::NEG_INFINITY => "-Infinity".into(),
        _ => format!("{x:.6}"),
    }
}

/// Java's check: the directory's parent (`File.getParentFile()`) must be an existing directory.
fn check_project_directory(dir: &Path) -> Result<(), Command> {
    match dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        None => Err(Command::fail("Invalid project directory: null")),
        Some(parent) if !parent.is_dir() => Err(Command::fail(format_args!(
            "Invalid project directory: {}",
            parent.display()
        ))),
        Some(_) => Ok(()),
    }
}

fn parse_create(args: &[String]) -> Command {
    let (matched, rest) = match_options(CREATE_OPTIONS, args);
    let project_id = matched.last().and_then(|m| m.value).map(str::to_string);
    match take_directory(rest) {
        Ok(Some(dir)) => Command::Create(CreateConfig {
            project_id,
            directory: dir.into(),
        }),
        Ok(None) => Command::print(CREATE_HELP, 1),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(s: &str) -> Command {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    fn start(s: &str) -> StartConfig {
        match parse_str(s) {
            Command::Start(cfg) => cfg,
            other => panic!("expected start, got {other:?}"),
        }
    }

    #[test]
    fn gcloud_invocation() {
        // Exactly what `gcloud beta emulators datastore start` passes (Python-style booleans).
        let cfg = start(
            "start --host=::1 --port=8081 --store_on_disk=True --allow_remote_shutdown --consistency=0.9 /tmp/x",
        );
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 8081);
        assert!(cfg.store_on_disk());
        assert!(cfg.allow_remote_shutdown);
        assert_eq!(cfg.consistency(), 0.9);
        assert_eq!(cfg.directory.as_deref(), Some(Path::new("/tmp/x")));
        assert_eq!(
            cfg.storage_path(),
            Some("/tmp/x/WEB-INF/appengine-generated/oxide_db.bin".into())
        );
    }

    #[test]
    fn testing_mode_defaults() {
        let cfg = start("start --testing -p 9000");
        assert_eq!(cfg.port, 9000);
        assert!(!cfg.store_on_disk());
        assert!(!cfg.store_index_configuration_on_disk());
        assert_eq!(cfg.consistency(), 1.0);
        assert_eq!(cfg.storage_path(), None);
    }

    #[test]
    fn firestore_mode_is_strongly_consistent() {
        let cfg = start("start --testing --firestore_in_datastore_mode --consistency=0.2");
        assert_eq!(cfg.consistency(), 1.0);
    }

    #[test]
    fn java_float_grammar() {
        for (input, want) in [("0.5", 0.5), (" 0.5f ", 0.5), ("1e-1", 0.1), ("+1D", 1.0)] {
            assert_eq!(parse_java_float(input), Some(want), "{input:?}");
        }
        assert!(parse_java_float("NaN").unwrap().is_nan());
        assert_eq!(parse_java_float("-Infinity"), Some(f32::NEG_INFINITY));
        for input in ["nan", "inf", "infinity", "abc", "", "0.5ff"] {
            assert_eq!(parse_java_float(input), None, "{input:?}");
        }
        assert_eq!(java_format_f(2.0), "2.000000");
        assert_eq!(java_format_f(f32::INFINITY), "Infinity");
    }

    #[test]
    fn in_memory_without_directory() {
        let cfg = start("start --store_on_disk=false");
        assert!(!cfg.store_on_disk());
        assert_eq!(cfg.directory, None);
    }
}
