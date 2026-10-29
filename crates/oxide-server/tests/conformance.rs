//! Replays the REST conformance scenarios (`tests/conformance/scenarios`) against our binary and
//! compares every response with the Java emulator's (`tests/conformance/java`), after
//! `conformance::normalize`. Re-record goldens with `record-conformance` (see its docs).

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

struct Emulator(Child, u16);

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start(extra: &[&str]) -> Emulator {
    let mut child = Command::new(env!("CARGO_BIN_EXE_datastore-oxide"))
        // Java's behavior: no superset features.
        .args(["start", "--testing", "--port=0", "--no_superset"])
        .args(extra)
        .env("OXIDE_LOG", "off")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let port = first.rsplit(':').next().unwrap().parse().unwrap();
    // Keep draining stdout in the background so the child never blocks on a full pipe.
    std::thread::spawn(move || lines.for_each(drop));
    Emulator(child, port)
}

fn allowlist(file: &str) -> HashSet<(String, usize)> {
    let text = std::fs::read_to_string(conformance::workspace_root().join(file)).unwrap();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.split_whitespace();
            (
                parts.next().unwrap().to_string(),
                parts.next().unwrap().parse().unwrap(),
            )
        })
        .collect()
}

#[test]
fn rest_matches_java_goldens() {
    check_mode(
        &[],
        &conformance::golden_dir(),
        "tests/conformance/allowlist.txt",
    );
}

/// The same scenarios against a Java emulator started with `--firestore_in_datastore_mode`.
#[test]
fn firestore_mode_matches_java_goldens() {
    check_mode(
        &["--firestore_in_datastore_mode"],
        &conformance::firestore_golden_dir(),
        "tests/conformance/allowlist-firestore.txt",
    );
}

fn check_mode(extra: &[&str], golden_dir: &std::path::Path, allowlist_file: &str) {
    let emulator = start(extra);
    let base = format!("http://127.0.0.1:{}", emulator.1);
    let allowed = allowlist(allowlist_file);
    let only = std::env::var("SCENARIO").ok();

    let mut failures = Vec::new();
    let mut checked = 0;
    for name in conformance::scenario_names().unwrap() {
        // SCENARIO=name, or a prefix ending in `*` (e.g. `v3_*`).
        if only.as_ref().is_some_and(|o| match o.strip_suffix('*') {
            Some(prefix) => !name.starts_with(prefix),
            None => o != &name,
        }) {
            continue;
        }
        let golden_path = golden_dir.join(format!("{name}.golden"));
        let Ok(golden) = std::fs::read_to_string(&golden_path) else {
            failures.push(format!(
                "{name}: no golden (record it with record-conformance)"
            ));
            continue;
        };
        let golden = conformance::parse_golden(&golden).unwrap();
        let steps = conformance::load_scenario(&name).unwrap();
        let actual = match conformance::run(&base, &steps) {
            Ok(actual) => actual,
            Err(e) => {
                failures.push(format!("{name}: run failed: {e:#}"));
                continue;
            }
        };
        let want_bodies = conformance::normalize_run(&golden);
        let got_bodies = conformance::normalize_run(&actual);
        for (i, (want, got)) in golden.iter().zip(&actual).enumerate() {
            let step = i + 1;
            checked += 1;
            if allowed.contains(&(name.clone(), step)) {
                continue;
            }
            let (want_body, got_body) = (&want_bodies[i], &got_bodies[i]);
            if want.status != got.status || want_body != got_body {
                failures.push(format!(
                    "{name} [{step}] {} {}: status java {} / oxide {}\n{}",
                    want.method,
                    want.path,
                    want.status,
                    got.status,
                    first_difference(want_body, got_body)
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} steps differ from Java:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// The first differing line of two bodies, with a few lines of context.
fn first_difference(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let Some(i) = (0..w.len().max(g.len())).find(|&i| w.get(i) != g.get(i)) else {
        return "(bodies differ only in trailing newline)".into();
    };
    let from = i.saturating_sub(3);
    let show = |lines: &[&str]| lines[from.min(lines.len())..(i + 3).min(lines.len())].join("\n");
    format!(
        "--- java (line {})\n{}\n--- oxide\n{}",
        i + 1,
        show(&w),
        show(&g)
    )
}
